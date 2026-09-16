# Argus

A self-hosted server that collects public signals about the physical world —
aircraft, ships, satellites, earthquakes, weather warnings, river levels,
sewage overflows, ocean buoys, airfield weather, buses — stores the history,
and serves it to a 3D globe in the browser and to an Android app.

The point of storing the history is that you can scrub backwards. Most tools
in this space show you the world right now and forget it the moment you close
the tab. Once you have a week of data you can ask things like "what was this
aircraft doing before it started circling" or "which vessel went dark inside
this box, and where did it reappear".

## Status

This is a personal project and it is early, but it works end to end. As of
September 2026:

- A Rust daemon (`argusd`) polls 33 upstream feeds into 30 layers, with
  provider failover chains, per-provider budgets and a disk budget.
- Postgres/PostGIS/TimescaleDB store with raw observations rolled up into
  tracks and daily summaries.
- A REST + WebSocket API with vector tiles, self-hosted terrain, device pairing
  and per-device bearer tokens.
- A CesiumJS web client on Google's photorealistic 3D tiles, with a DVR
  scrubber.
- An Android client (Kotlin, Compose, MapLibre) that pairs by QR code, works
  offline, and gets geofence alerts through a foreground service.
- Geofences that fire on entry/exit and remember which device has been told.

Live layers right now: flights (adsb.lol → adsb.fi → OpenSky), satellites
(CelesTrak → Space-Track, propagated with SGP4), earthquakes (USGS → EMSC),
US weather alerts (NWS), aviation SIGMETs and aerodrome METARs with their
TAFs, radiosondes (SondeHub), every bus in England (Bus Open Data Service),
UK storm overflows (nine water companies), Environment Agency flood warnings
and river gauges, NOAA NDBC buoys, the Argo float array, every meteor the Global Meteor Network
triangulates, the SatNOGS ground stations with what each is hearing, TfL's road disruptions across
London, the carbon intensity of the grid in each of Britain's fourteen
distribution regions, Europe's offshore platforms and wind farms, modelled
air quality, pollen and sea state sampled on a lattice over each area and
river discharge at every gauged river (Open-Meteo, CAMS, GloFAS), the Raspberry Shake citizen seismograph network, a month of
street-level crime for England and Wales (data.police.uk), every submarine
cable and where it lands (TeleGeography), the power grid — lines, substations
and plants — from OpenStreetMap, every active fire the VIIRS satellites
saw in the last day (NASA FIRMS, keyed), every fireball seen from orbit
since 1988 (CNEOS), and every airfield in the world with its runways
(OurAirports).

Not built yet: satellite imagery and fire detections, infrastructure layers
(power grid, cables, BGP), SDR receivers, phone-as-sensor, the AR sky view.
See the roadmap at the bottom.

## How it fits together

```
   public feeds ─┐
   local SDR   ──┼──►  argus-ingest   scheduler, budgets, failover chains,
   phone sensor ─┘                    disk cache
                            │  Observation
                            ▼
                     argus-store    Postgres + PostGIS + TimescaleDB
                     raw 7d ──► tracks_1m 90d ──► entity_daily forever
                            │
        ┌───────────────────┼────────────────────┬──────────────────┐
        ▼                   ▼                    ▼                  ▼
   argus-analyze       argus-alert          argus-api          argus-tiles
   derived layers      geofences            REST + WebSocket   MVT + terrain
                                            + argus-present
                                              cards: attrs → words
        └───────────────────┴────────────────────┴──────────────────┘
                            │   LAN / Tailscale
                ┌───────────┴────────────┐
                ▼                        ▼
          Android client            Web client
   Kotlin · Compose · MapLibre    TypeScript · CesiumJS
```

Every feed is reduced to one type, and a new source is usually a 200–600 line
driver plus a live test:

```rust
pub struct Observation {
    pub source_id:   SourceId,
    pub entity:      EntityId,       // kind + natural key: icao24, MMSI, NORAD id
    pub observed_at: DateTime<Utc>,  // the source's time, never ingest time
    pub ingested_at: DateTime<Utc>,
    pub position:    Option<Position>,   // altitude always carries a datum
    pub kinematics:  Option<Kinematics>,
    pub geom:        Option<Geometry<f64>>,
    pub label:       Option<String>,
    pub quality:     Quality,        // live | delayed | modeled | estimated | stale
    pub attrs:       serde_json::Value,
}
```

Two fields carry most of the opinions. `quality` says whether you are looking
at a transponder return or an interpolation, and it is carried all the way
from the driver to the layer row in both clients. `Position::datum` means
altitude is never stored without saying what it was measured from — mixing
pressure altitude with geometric height is how aircraft end up under the
terrain, and it is not a bug you can find by reading the display code.

Layers are discovered from the running drivers and described by
`GET /v1/layers`, so a new driver appears in both clients without a client
release.

## Requirements

- Rust 1.90+
- PostgreSQL 18 with PostGIS 3.6 and TimescaleDB 2.28 (the Community/TSL
  build — the Apache-only build lacks compression and continuous aggregates,
  which the migrations need)
- Node 22+ for the web client
- For the photorealistic globe, a Google Map Tiles API key and a Cesium ion
  token. Everything else runs keyless or on free self-serve accounts.

[docs/SETUP.md](docs/SETUP.md) has the Gentoo package setup, the database
bootstrap, and the Android toolchain notes.

## Running

```sh
cp deploy/argus.example.toml argus.toml    # gitignored; holds your keys
$EDITOR argus.toml                         # at minimum, database.url
ARGUS_CONFIG=./argus.toml cargo run -p argusd
```

Migrations apply on startup. The daemon binds to `127.0.0.1:8787` by default.
It holds every credential you give it and will use them on behalf of anyone who
can reach the port, so don't widen the bind casually — see
[docs/REACH.md](docs/REACH.md) for the LAN and Tailscale options and the
auth setting that goes with each.

Web client:

```sh
cd web && npm install && npm run dev      # http://localhost:5173
```

Android: `cd droid && ./gradlew installDebug`. The daemon prints a pairing QR
on its console; scan it from the app and the phone gets a bearer token that is
shown exactly once.

Tests:

```sh
cargo test --workspace                                   # unit tests, no DB
ARGUS_TEST_DATABASE_URL=postgres://argus@localhost/argus_test cargo test --workspace
ARGUS_NETWORK_TESTS=1 cargo test -p argus-ingest         # polls the real feeds
```

## ⚠️ Disk space

With every source enabled, Argus writes on the order of **10–15 GB a day**
of raw observations, and keeps seven days of them. Most of that is one
layer. Rows are about 440 bytes on disk including indexes; these are
steady-state figures from a weekday, before any TimescaleDB compression.

| Layer | Rows per day | Per day | Why |
|---|---|---|---|
| `buses` | 15–29 M | 6–12 GB | 28,000 buses reporting every 30 s, polled once a minute across the AOIs. Twenty times everything else together. |
| `flights` (OpenSky global sweep) | ~5 M | ~2 GB | 50,000 aircraft every 15 min |
| `flights` (adsb.fi/adsb.lol, AOIs) | ~2.5 M | ~1 GB | every aircraft in the AOIs, every 20 s |
| `storm-overflows` | ~1.5 M | ~0.6 GB | 15,000 outfalls dated by poll time, every 15 min |
| `satellites` | ~1.4 M | ~0.6 GB | ~1,000 objects propagated every minute |
| `ground-stations` | ~0.6 M | ~0.3 GB | 4,300 stations dated by poll time, every 10 min |
| `river-gauges` | ~0.4 M | ~0.2 GB | 4,000 gauges every 15 min |
| `radiosondes` | ~0.3 M | ~0.1 GB | tracks of every balloon aloft |
| `metars` | ~0.2 M | ~0.1 GB | 5,000 aerodromes, one row per new report |
| `fires` | ~0.2 M | ~0.1 GB | 200,000 VIIRS detections a day worldwide, each written once |
| `street-crime` | ~0.2 M | ~0.1 GB | half a million crimes a month across England and Wales, rewritten every three days |
| `argo-floats` | ~0.1 M | ~50 MB | 4,300 floats dated by poll time, hourly |
| `seismographs` | < 0.1 M | ~10 MB | 6,200 stations dated by poll time, six-hourly |
| everything else | < 0.1 M | < 50 MB | buoys, meteors, quakes, alerts, SIGMETs, TfL, carbon intensity, EMODnet, Open-Meteo lattices, cables and the grid (features: written only on change) |

The dials, all in `argus.toml`:

- **`[sources.<id>] cadence_secs`** — the biggest one. Buses at 300 s instead
  of 60 is a fifth of the rows.
- **`[[aoi]]`** — bounded sources (buses, adsb) are polled per area of
  interest. An AOI that covers the whole country costs what the country costs.
- **`[retention] raw`** — how long raw observations live; the one-minute
  rollup in `tracks_1m` lasts `retention.tracks` and is much smaller.
- **`[capture] disk_budget_gb`** — counted against *all* hypertable bytes.
  Past `disk_warn_fraction` of it the scheduler drops to AOI-only polling,
  silently from the map's point of view. The default is 80 GB; at full
  ingest that is about a week.
- Chunks older than two days are moved to the `argus_cold` tablespace by a
  TimescaleDB job, so a small fast disk can hold the hot data and a large
  slow one the rest. Point the tablespace at the big disk.

Features (`wind-farms`, `offshore-platforms`) are versioned in their own
table and only write when something changes; they cost nothing per poll.

## Adding a source

Each driver lives in `crates/argus-ingest/src/sources/`, implements `Source`,
and keeps its `decode` function separate from its `poll` so it can be tested
against captured data. The pattern that has held up: profile the whole feed
before writing the decoder (field cardinality, timestamp distribution, missing
value conventions), write a live test that asserts on record counts rather than
status codes, and read `sources.last_error` in the database after deploying,
because a live test with its own HTTP client is not testing the daemon's.

A driver stores what its feed said, in the feed's own terms (`wx: "-RA BR"`,
`squawk: "7700"`). What a person sees when they tap the entity comes from
`crates/argus-present`, which turns those attributes into a card — a title, a
sentence, labelled rows in words and units — on the server, so both clients
show it the day the driver lands. A layer without a presenter gets the
generic one (keys prettified, units recognised), and a presenter that misses
a key shows it under "Also" rather than losing it. To check a presenter
against a whole layer rather than the record it was written from:

```
psql -At -c "select row_to_json(e) from entities e" | cargo run -p argus-present --example cards -- --summary
```

[docs/DATA-SOURCES.md](docs/DATA-SOURCES.md) is the research log: every source
that has been checked, what it actually returns, and what went wrong.

## Roadmap

| Phase | | |
|---|---|---|
| 0 | Schema, config, daemon skeleton | done |
| 1 | Ingest runtime and first sources | done |
| 2 | Store and DVR: rollups, retention, AOI rate gating | done |
| 3 | API and tiles | done |
| 4 | Web client | done |
| 5 | Android client | done |
| 6 | Alerts, geofences, Tailscale | done |
| 7 | Earth observation: imagery, fires, nightlights | fires done |
| 8 | Infrastructure and internet: grid, cables, BGP | grid and cables done |
| 9 | Conflict and news | |
| 10 | Self-collected: SDR, phone-as-sensor | |
| 11 | Derived analytics and AR sky view | |

Within each phase, new sources are added as they get verified — the order is in
`docs/DATA-SOURCES.md`.

## Scope

Argus is about events, assets, infrastructure and systems: aircraft, vessels,
satellites, fires, rivers, networks. It does not do named-person search, face
recognition or tracking of individuals, and won't grow features for them.

## Licence

MIT — see [LICENSE](LICENSE). Data from each source carries its own terms;
every driver declares its attribution and both clients show it verbatim. See
[ACKNOWLEDGEMENTS.md](ACKNOWLEDGEMENTS.md).
