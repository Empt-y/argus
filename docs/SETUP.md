# Setting up the store on Gentoo

Verified on `default/linux/amd64/23.0/systemd`, 2026-08-28.

## Packages

```sh
sudo tee /etc/portage/package.accept_keywords/argus <<'KW'
dev-db/postgis ~amd64
dev-db/timescaledb ~amd64
KW

sudo tee /etc/portage/package.use/argus <<'USE'
dev-db/postgresql  ssl
dev-db/postgis     POSTGRES_TARGETS: -postgres17 postgres18
dev-db/timescaledb POSTGRES_TARGETS: -postgres17 postgres18
dev-db/timescaledb proprietary-extensions
USE

echo 'dev-db/timescaledb timescale' | sudo tee -a /etc/portage/package.license

sudo emerge dev-db/postgresql:18 dev-db/postgis dev-db/timescaledb
```

Three things here are easy to get wrong:

**`POSTGRES_TARGETS`.** The profile default is `postgres17`. Without the
override, portage builds the extensions against 17 and installs a *second*
PostgreSQL alongside the 18 you asked for.

**`proprietary-extensions`.** Off by default, which builds the Apache-2 subset
of TimescaleDB. That subset has **no compression and no continuous
aggregates** — the entire rollup ladder in `0002_timeseries.sql` fails to apply
with `functionality not supported under the current "apache" license`. Those
features are Timescale Community (TSL), which is free for self-hosted use and
only forbids offering it as a managed database service. Enabling the flag
requires accepting the `timescale` licence, hence the `package.license` line.

**Binary packages.** There is no binhost configured on a stock Gentoo install.
Adding the official one turns `sci-libs/proj` (a 719 MB source download) and
`geos` into instant binary merges:

```sh
sudo mkdir -p /etc/portage/binrepos.conf
sudo tee /etc/portage/binrepos.conf/gentoo.conf <<'BR'
[gentoo]
priority = 9999
sync-uri = https://distfiles.gentoo.org/releases/amd64/binpackages/23.0/x86-64-v3
BR
sudo emerge sec-keys/openpgp-keys-gentoo-release
sudo env FEATURES="binpkg-request-signature" emerge --getbinpkg --binpkg-respect-use=y <pkgs>
```

Use the `x86-64-v3` variant only if the CPU supports AVX2/BMI2/FMA. `postgresql`
and `gdal` still build from source — the binhost builds them on a non-systemd
profile, so their USE flags do not match.

## Cluster

```sh
sudo emerge --config dev-db/postgresql:18
```

TimescaleDB must be preloaded, and it phones home unless told not to. In
`/etc/postgresql-18/postgresql.conf`:

```
shared_preload_libraries = 'timescaledb'
timescaledb.telemetry_level = off
```

```sh
sudo systemctl enable --now postgresql-18.service
sudo -u postgres psql -c "CREATE ROLE argus LOGIN"
sudo -u postgres createdb -O argus argus
sudo -u postgres psql -d argus -c "CREATE EXTENSION postgis; CREATE EXTENSION timescaledb;"
```

Migrations apply on startup:

```sh
ARGUS_CONFIG=./argus.toml cargo run -p argusd
```

## Tests

The unit suite needs no database. The DVR integration tests need a real one and
skip silently without it:

```sh
sudo -u postgres createdb -O argus argus_test
sudo -u postgres psql -d argus_test -c "CREATE EXTENSION postgis; CREATE EXTENSION timescaledb;"
ARGUS_TEST_DATABASE_URL=postgres://argus@localhost/argus_test cargo test --workspace
```

## DNS

Argus resolves in-process via hickory rather than through glibc, because glibc
walks `/etc/resolv.conf` serially: one blackholed nameserver listed first stalls
*every* lookup for seconds before falling back. That was not hypothetical during
development — `1.1.1.1` was unreachable on the build network and listed first,
so each poll spent 5–15 s in DNS before timing out the connect.

If lookups feel slow machine-wide, that is the shape of it. Check each
nameserver individually rather than trusting that resolution "works":

```sh
for ns in $(awk '/^nameserver/{print $2}' /etc/resolv.conf); do
  printf '%-40s ' "$ns"
  timeout 5 getent ahosts example.com >/dev/null 2>&1 && echo ok || echo SLOW
done
```

## Provider chains

Every free provider has a limit, and most layers can be served by more than one.
A layer is therefore configured as a ranked chain rather than a single upstream:

```
flights: adsb.lol -> adsb.fi
```

The chain is itself a `Source`, so the scheduler treats it exactly like any
single driver. It sticks with whichever provider answered last rather than
walking the chain each poll (that would spend the scarce primary allowance on
liveness checks), but reaches back up every 30 minutes so a daily quota that
resets at midnight is actually noticed.

Failures are classified rather than lumped together, because "spent" and
"broken" need different handling:

| Failure | Effect |
|---|---|
| Rate limited | Sidelined for `Retry-After`, or 15 min if unstated |
| Transport / decode | Sidelined 2 min |
| Allowance spent | Skipped until the quota window rolls over |
| Rejected credential, 403 on a keyed source, absent hardware | Marked unavailable; never retried on a timer |

Providers declare their own allowance (`SourceDescriptor::quota`), including the
cost per poll — OpenSky charges more credits for a global query than a bounded
one, and a chain that assumes one-per-poll sails past the real limit. The
allowance is charged *before* the call, since a request that times out still
consumed it.

Each member gets its own row in `sources`, so `GET /v1/sources` shows which
provider is carrying a layer and why the ones above it are not:

```
adsb-fi       flights     unknown   0      <- standby, never needed
adsb-lol      flights     live      349    <- serving
flights       flights     live      349    <- the chain
```

`unknown` for a standby is deliberate: it has not answered, but nothing is
wrong with it. That is a different thing from `live` with zero results.

**Adding a provider to a chain.** Implement `Source` as usual, then list it in
the chain in preference order. Providers sharing a wire format should share a
decoder — `sources/readsb.rs` serves adsb.lol, adsb.fi and, in Phase 10, a local
dump1090 receiver, because a dongle on the roof is just another provider.
