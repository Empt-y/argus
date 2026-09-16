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

- A Rust daemon (`argusd`) polls 22 upstream feeds into 11 layers, with
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
and river gauges, and NOAA NDBC buoys.

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

## Adding a source

Each driver lives in `crates/argus-ingest/src/sources/`, implements `Source`,
and keeps its `decode` function separate from its `poll` so it can be tested
against captured data. The pattern that has held up: profile the whole feed
before writing the decoder (field cardinality, timestamp distribution, missing
value conventions), write a live test that asserts on record counts rather than
status codes, and read `sources.last_error` in the database after deploying,
because a live test with its own HTTP client is not testing the daemon's.

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
| 7 | Earth observation: imagery, fires, nightlights | |
| 8 | Infrastructure and internet: grid, cables, BGP | |
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
