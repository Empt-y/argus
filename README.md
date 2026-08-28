# Argus

**A self-hosted spatial intelligence server with a time machine.**

Argus ingests public signals about the world — aircraft transponders, ship
beacons, orbital elements, seismographs, satellite imagery, power grids, BGP
tables, public cameras — normalises them into one model, and **stores the
history**. The planet becomes something you can scrub backwards.

A Rust daemon does the collecting. Two clients do the looking: a native Android
app over WiFi or Tailscale, and a web client for the photorealistic 3D globe.

> **Status: early.** Phase 0 of 11. The core model, schema and daemon skeleton
> exist; ingest, API and both clients are being built. See
> [the plan](#roadmap).

---

## Why

Most open-source intelligence tooling shows you *now*. The signals are abundant
and the interfaces are improving, but almost everything is ephemeral: close the
tab and the world you were watching is gone.

Recording it is the hard part, and it is the part that makes the rest
interesting. Once a week of history exists you can ask questions that are
impossible against a live feed alone:

- Which vessel stopped transmitting inside this box, and where did it resume?
- What was this aircraft doing for the twelve hours before it started orbiting?
- Which of these two dates does the imagery actually differ between?
- What normally happens here, and is today unusual?

Those are all *derived* layers. They fall out of the archive.

## Architecture

```
   public feeds ─┐
   local SDR   ──┼──►  argus-ingest   scheduler, budget governor,
   phone sensor ─┘                    disk cache, circuit breaker
                            │  normalised Observation
                            ▼
                     argus-store    Postgres + PostGIS + TimescaleDB
                     raw 7d ──► tracks_1m 90d ──► entity_daily forever
                            │
        ┌───────────────────┼────────────────────┬──────────────────┐
        ▼                   ▼                    ▼                  ▼
   argus-analyze       argus-alert          argus-api          argus-tiles
   derived layers      geofence rules      REST + WebSocket   MVT + raster
        └───────────────────┴────────────────────┴──────────────────┘
                            │   LAN / Tailscale
                ┌───────────┴────────────┐
                ▼                        ▼
          Android client            Web client
   Kotlin · Compose · MapLibre    TypeScript · CesiumJS
   AR sky · DVR · alerts          photorealistic 3D
```

Every feed reduces to one type. Get that right and a new layer is a ~200-line
driver:

```rust
pub struct Observation {
    pub source_id:   SourceId,
    pub entity:      EntityId,       // kind + natural key: icao24, MMSI, NORAD id
    pub observed_at: DateTime<Utc>,  // source time, never ingest time
    pub ingested_at: DateTime<Utc>,
    pub position:    Option<Position>,   // carries its own altitude datum
    pub kinematics:  Option<Kinematics>,
    pub geom:        Option<Geometry<f64>>,
    pub label:       Option<String>,
    pub quality:     Quality,        // live | delayed | modeled | estimated | stale
    pub attrs:       serde_json::Value,
}
```

Two of those fields carry most of the project's opinions:

**`quality`** — Argus never presents modelled data as live. The value propagates
from driver to store to API to the layer row in both clients, so you can always
see whether you are looking at a transponder return or an interpolation.

**`Position::datum`** — altitude is never stored without stating what it was
measured from. Mixing pressure altitude with geometric height is what buries
aircraft under terrain, and it is a bug you cannot find by reading the code that
displays it.

## Requirements

- Rust 1.90+
- PostgreSQL 18 with PostGIS 3.6 and TimescaleDB 2.28
- One API key for the photorealistic globe (Google Map Tiles); everything else
  runs keyless or on free developer access

On Gentoo:

```sh
sudo tee /etc/portage/package.accept_keywords/argus <<'EOF'
dev-db/postgis ~amd64
dev-db/timescaledb ~amd64
EOF
sudo tee /etc/portage/package.use/argus <<'EOF'
dev-db/postgresql ssl
dev-db/postgis     POSTGRES_TARGETS: -postgres17 postgres18
dev-db/timescaledb POSTGRES_TARGETS: -postgres17 postgres18
EOF
sudo emerge dev-db/postgresql:18 dev-db/postgis dev-db/timescaledb
```

Pinning `POSTGRES_TARGETS` matters — the default is `postgres17`, which would
build and install a second PostgreSQL alongside the one you want.

## Running

```sh
cargo build --release
install -Dm600 deploy/argus.example.toml /etc/argus/argus.toml
$EDITOR /etc/argus/argus.toml       # at minimum, set database.url
ARGUS_CONFIG=/etc/argus/argus.toml ./target/release/argusd
```

The daemon binds to loopback by default. It holds every credential you configure
and will broker them for anyone who can reach the port, so widening the bind is
a deliberate choice — prefer reaching it over Tailscale, and set provider-side
budget caps before doing either.

```sh
cargo test --workspace
```

## Roadmap

| Phase | | |
|---|---|---|
| 0 | Foundations — schema, config, daemon skeleton | in progress |
| 1 | Ingest core and the first 17 sources | |
| 2 | Store and DVR — rollups, retention, AOI rate gating | |
| 3 | API and tiles — REST, WebSocket, time-aware MVT | |
| 4 | Web client | |
| 5 | Android client | |
| 6 | Alerts, geofences, Tailscale | |
| 7 | Earth observation and hazards | |
| 8 | Infrastructure and internet | |
| 9 | Conflict and news | |
| 10 | Self-collected: SDR, phone-as-sensor | |
| 11 | Derived analytics and AR sky view | |

## Scope

Argus models **events, assets, infrastructure and systems** — aircraft, vessels,
satellites, fires, cameras, cities, networks. It does not do named-person
search, face recognition, or tracking individuals, and it will not grow features
for them.

## Licence

MIT. Bundled and live datasets carry their own terms; each source declares its
own attribution, which both clients surface verbatim. See
[ACKNOWLEDGEMENTS.md](ACKNOWLEDGEMENTS.md).
