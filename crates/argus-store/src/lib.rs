//! Persistence for Argus: the DVR write path and the queries that read it back.
//!
//! Everything here speaks `argus_core` types. No SQL escapes this crate.

use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::geo::BoundingBox;
use chrono::{DateTime, Utc};
use geozero::ToWkb;
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;

pub mod alerting;
pub mod devices;
pub mod horizon;
pub mod model;
pub mod serve;

pub use model::EntityRow;
pub use alerting::NewAlert;
pub use serve::DeltaRow;

/// What a viewport query asks for beyond its bounding box.
///
/// `layers` and `kinds` are both here because they answer different questions
/// and clients genuinely ask both: a map style selects layers, while the AR sky
/// view wants every aircraft regardless of which feed produced it. An empty
/// vector means "no constraint", not "match nothing" - the alternative would
/// make the default filter return an empty map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntityFilter {
    pub layers: Vec<String>,
    pub kinds: Vec<argus_core::EntityKind>,
}

impl EntityFilter {
    pub fn layers(layers: impl IntoIterator<Item = String>) -> Self {
        Self {
            layers: layers.into_iter().collect(),
            kinds: Vec::new(),
        }
    }

    pub(crate) fn layers_arg(&self) -> Option<&[String]> {
        (!self.layers.is_empty()).then_some(self.layers.as_slice())
    }

    /// Kinds as the database spells them.
    pub(crate) fn kinds_arg(&self) -> Option<Vec<String>> {
        (!self.kinds.is_empty())
            .then(|| self.kinds.iter().map(|k| k.as_str().to_string()).collect())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("required extension {0} is not installed in the target database")]
    MissingExtension(&'static str),
}

/// What a batch write actually did.
///
/// The three outcomes must stay distinguishable. A poll whose rows were all
/// suppressed as already-known is perfectly healthy — a quiet event feed looks
/// exactly like that — whereas rows dropped for being malformed are a real
/// signal. Collapsing them into one number makes a calm feed report as broken.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteOutcome {
    /// Rows new to the store.
    pub inserted: u64,
    /// Rows the store already held, suppressed by the dedupe index.
    pub deduped: u64,
    /// Readings discarded before the write: no position, no geometry, no
    /// attributes, or a kind that is not time-series.
    pub skipped: u64,
}

impl WriteOutcome {
    /// Readings the store handled successfully, however it handled them.
    pub fn accepted(&self) -> u64 {
        self.inserted + self.deduped
    }
}

/// A handle to the Argus database. Cheap to clone — it wraps a pool.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Connect and verify the database is actually usable before returning.
    ///
    /// The extension check is not paranoia: connecting to a plain PostgreSQL
    /// that happens to be listening produces a pool that works fine until the
    /// first spatial query, which then fails deep inside an ingest task where
    /// the real cause is invisible. Fail here instead, naming the extension.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(10))
            .connect(url)
            .await?;

        let store = Self { pool };
        store.verify_extensions().await?;
        Ok(store)
    }

    async fn verify_extensions(&self) -> Result<(), StoreError> {
        for ext in ["postgis", "timescaledb"] {
            let present: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = $1)")
                    .bind(ext)
                    .fetch_one(&self.pool)
                    .await?;
            if !present {
                // Static strings so the error can name the extension without
                // allocating; the set is closed and known at compile time.
                return Err(StoreError::MissingExtension(match ext {
                    "postgis" => "postgis",
                    _ => "timescaledb",
                }));
            }
        }
        Ok(())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Apply any migrations the database has not seen.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        Ok(())
    }

    /// Write a batch of observations to the hypertable and refresh live entity
    /// state from them.
    ///
    /// Batched into a single statement per table via `UNNEST`. Row-at-a-time
    /// inserts cannot keep up here — a single global ADS-B poll is ~10k rows,
    /// and at a 15-second cadence that is 40k statements a minute from one
    /// source alone.
    pub async fn write_observations(
        &self,
        observations: &[Observation],
    ) -> Result<WriteOutcome, StoreError> {
        let rows: Vec<&Observation> = observations
            .iter()
            .filter(|o| {
                o.is_meaningful()
                    && o.entity.kind.is_timeseries()
                    && temporally_plausible(o)
            })
            .collect();
        let skipped = (observations.len() - rows.len()) as u64;
        if rows.is_empty() {
            return Ok(WriteOutcome {
                skipped,
                ..Default::default()
            });
        }

        let mut tx = self.pool.begin().await?;

        let observed_at: Vec<DateTime<Utc>> = rows.iter().map(|o| o.observed_at).collect();
        let ingested_at: Vec<DateTime<Utc>> = rows.iter().map(|o| o.ingested_at).collect();
        let source_id: Vec<String> = rows.iter().map(|o| o.source_id.to_string()).collect();
        let entity_kind: Vec<String> =
            rows.iter().map(|o| o.entity.kind.as_str().to_string()).collect();
        let entity_key: Vec<String> = rows.iter().map(|o| o.entity.key.clone()).collect();
        // NULL lon/lat yields a NULL geometry via ST_Point's strictness, which
        // is what we want for observations that carry only attributes.
        let lon: Vec<Option<f64>> = rows
            .iter()
            .map(|o| o.position.filter(Position::is_plausible).map(|p| p.lon))
            .collect();
        let lat: Vec<Option<f64>> = rows
            .iter()
            .map(|o| o.position.filter(Position::is_plausible).map(|p| p.lat))
            .collect();
        let alt_m: Vec<Option<f64>> = rows.iter().map(|o| o.position.and_then(|p| p.alt_m)).collect();
        let alt_datum: Vec<Option<String>> = rows
            .iter()
            .map(|o| o.position.map(|p| model::alt_datum_str(p.datum).to_string()))
            .collect();
        let course: Vec<Option<f32>> = rows
            .iter()
            .map(|o| o.kinematics.and_then(|k| k.course_deg).map(|v| v as f32))
            .collect();
        let heading: Vec<Option<f32>> = rows
            .iter()
            .map(|o| o.kinematics.and_then(|k| k.heading_deg).map(|v| v as f32))
            .collect();
        let speed: Vec<Option<f32>> = rows
            .iter()
            .map(|o| o.kinematics.and_then(|k| k.ground_speed_mps).map(|v| v as f32))
            .collect();
        let vrate: Vec<Option<f32>> = rows
            .iter()
            .map(|o| o.kinematics.and_then(|k| k.vertical_rate_mps).map(|v| v as f32))
            .collect();
        let quality: Vec<String> = rows
            .iter()
            .map(|o| model::quality_str(o.quality).to_string())
            .collect();
        let label: Vec<Option<String>> = rows.iter().map(|o| o.label.clone()).collect();
        // A driver that sets no attributes leaves `Value::Null`, which would
        // reach the database as JSON null rather than as an empty object — a
        // distinction with no meaning here and a real cost downstream, where
        // every client would have to null-check before reading a field. Coerce
        // it once, at the boundary, so `attrs` is always an object.
        let attrs: Vec<serde_json::Value> = rows
            .iter()
            .map(|o| {
                if o.attrs.is_null() {
                    serde_json::Value::Object(serde_json::Map::new())
                } else {
                    o.attrs.clone()
                }
            })
            .collect();
        // EWKB carries the SRID with the bytes, so the database is told 4326
        // explicitly rather than inferring it from the column type. A geometry
        // that fails to encode is dropped to NULL rather than failing the whole
        // batch: one malformed polygon must not cost the other ten thousand
        // aircraft in the same write.
        let geom: Vec<Option<Vec<u8>>> = rows
            .iter()
            .map(|o| {
                o.geom.as_ref().and_then(|g| {
                    g.to_ewkb(geozero::CoordDimensions::xy(), Some(4326))
                        .map_err(|err| {
                            tracing::warn!(
                                entity = %o.entity,
                                "dropping unencodable geometry: {err}"
                            );
                        })
                        .ok()
                })
            })
            .collect();

        let inserted = sqlx::query(
            r#"
            INSERT INTO observations (
                observed_at, ingested_at, source_id, entity_kind, entity_key,
                position, geom, alt_m, alt_datum,
                course_deg, heading_deg, speed_mps, vrate_mps,
                quality, label, attrs
            )
            SELECT
                u.observed_at, u.ingested_at, u.source_id, u.entity_kind, u.entity_key,
                CASE WHEN u.lon IS NULL OR u.lat IS NULL THEN NULL
                     ELSE ST_SetSRID(ST_MakePoint(u.lon, u.lat), 4326) END,
                CASE WHEN u.geom IS NULL THEN NULL ELSE ST_GeomFromEWKB(u.geom) END,
                u.alt_m, u.alt_datum,
                u.course_deg, u.heading_deg, u.speed_mps, u.vrate_mps,
                u.quality, u.label, u.attrs
            FROM UNNEST(
                $1::timestamptz[], $2::timestamptz[], $3::text[], $4::text[], $5::text[],
                $6::float8[], $7::float8[], $8::float8[], $9::text[],
                $10::real[], $11::real[], $12::real[], $13::real[],
                $14::text[], $15::text[], $16::jsonb[], $17::bytea[]
            ) AS u(
                observed_at, ingested_at, source_id, entity_kind, entity_key,
                lon, lat, alt_m, alt_datum,
                course_deg, heading_deg, speed_mps, vrate_mps,
                quality, label, attrs, geom
            )
            -- Re-polling an immutable event rewrites the same row; suppress it.
            -- See 0003_observation_dedupe.sql.
            ON CONFLICT (entity_kind, entity_key, observed_at, source_id) DO NOTHING
            "#,
        )
        .bind(&observed_at)
        .bind(&ingested_at)
        .bind(&source_id)
        .bind(&entity_kind)
        .bind(&entity_key)
        .bind(&lon)
        .bind(&lat)
        .bind(&alt_m)
        .bind(&alt_datum)
        .bind(&course)
        .bind(&heading)
        .bind(&speed)
        .bind(&vrate)
        .bind(&quality)
        .bind(&label)
        .bind(&attrs)
        .bind(&geom)
        .execute(&mut *tx)
        .await?;
        let inserted = inserted.rows_affected();

        // Live state. `WHERE EXCLUDED.observed_at > entities.observed_at` is
        // load-bearing: batches can contain out-of-order samples, and two
        // sources can describe the same aircraft at different lags. Without the
        // guard, a late-arriving stale fix overwrites a newer one and the
        // contact visibly jumps backwards.
        //
        // That guard alone is too strict for immutable events, whose
        // `observed_at` is an origin or issue time and never advances. Two
        // things were silently lost to it:
        //
        //   * A weather alert first seen without a polygon — 94% of them, since
        //     NWS issues by zone id — could never gain one once its zones
        //     resolved. It was frozen shapeless for life.
        //   * Every revision an upstream publishes. USGS revises magnitude and
        //     location for hours after an event; `0003_observation_dedupe.sql`
        //     names that as the reason those feeds must be re-polled at all. The
        //     re-polls happened and the revisions were then thrown away here, so
        //     an M4.0 later corrected to M4.6 stayed 4.0 forever.
        //
        // So a same-instant write also wins when it is either a revision from
        // the same source, or another source supplying a shape this row lacks.
        // Neither can resurrect a stale position: a revision is by definition
        // the newest word from the source that owns the row, and the geometry
        // clause changes nothing about a row that already has a shape.
        //
        // The content comparison is what keeps this from being expensive. A
        // re-poll that says exactly what the store already holds updates
        // nothing, so `updated_at` does not move and the delta stream does not
        // resend several hundred unchanged events every poll.
        sqlx::query(
            r#"
            INSERT INTO entities (
                entity_kind, entity_key, source_id, layer_id, observed_at,
                position, geom, alt_m, alt_datum,
                course_deg, heading_deg, speed_mps, vrate_mps,
                quality, label, attrs
            )
            SELECT DISTINCT ON (u.entity_kind, u.entity_key)
                u.entity_kind, u.entity_key, u.source_id,
                COALESCE(s.layer_id, u.source_id), u.observed_at,
                CASE WHEN u.lon IS NULL OR u.lat IS NULL THEN NULL
                     ELSE ST_SetSRID(ST_MakePoint(u.lon, u.lat), 4326) END,
                CASE WHEN u.geom IS NULL THEN NULL ELSE ST_GeomFromEWKB(u.geom) END,
                u.alt_m, u.alt_datum,
                u.course_deg, u.heading_deg, u.speed_mps, u.vrate_mps,
                u.quality, u.label, u.attrs
            FROM UNNEST(
                $1::timestamptz[], $2::text[], $3::text[], $4::text[],
                $5::float8[], $6::float8[], $7::float8[], $8::text[],
                $9::real[], $10::real[], $11::real[], $12::real[],
                $13::text[], $14::text[], $15::jsonb[], $16::bytea[]
            ) AS u(
                observed_at, source_id, entity_kind, entity_key,
                lon, lat, alt_m, alt_datum,
                course_deg, heading_deg, speed_mps, vrate_mps,
                quality, label, attrs, geom
            )
            LEFT JOIN sources s ON s.source_id = u.source_id
            ORDER BY u.entity_kind, u.entity_key, u.observed_at DESC
            ON CONFLICT (entity_kind, entity_key) DO UPDATE SET
                source_id   = EXCLUDED.source_id,
                layer_id    = EXCLUDED.layer_id,
                observed_at = EXCLUDED.observed_at,
                updated_at  = now(),
                position    = EXCLUDED.position,
                alt_m       = EXCLUDED.alt_m,
                alt_datum   = EXCLUDED.alt_datum,
                course_deg  = EXCLUDED.course_deg,
                heading_deg = EXCLUDED.heading_deg,
                speed_mps   = EXCLUDED.speed_mps,
                vrate_mps   = EXCLUDED.vrate_mps,
                quality     = EXCLUDED.quality,
                label       = EXCLUDED.label,
                attrs       = EXCLUDED.attrs,
                geom        = EXCLUDED.geom
            WHERE EXCLUDED.observed_at > entities.observed_at
               OR (EXCLUDED.observed_at = entities.observed_at
                   AND (
                     -- A revision from the source that owns this row.
                     (EXCLUDED.source_id = entities.source_id
                      AND (EXCLUDED.attrs IS DISTINCT FROM entities.attrs
                           OR EXCLUDED.label IS DISTINCT FROM entities.label))
                     -- Or a shape for a row that had none. Compared as NULL /
                     -- NOT NULL rather than by value: PostGIS `=` is a bounding
                     -- box test, so two genuinely different polygons with the
                     -- same extent would compare equal.
                     OR (entities.geom IS NULL AND EXCLUDED.geom IS NOT NULL)
                   ))
            "#,
        )
        .bind(&observed_at)
        .bind(&source_id)
        .bind(&entity_kind)
        .bind(&entity_key)
        .bind(&lon)
        .bind(&lat)
        .bind(&alt_m)
        .bind(&alt_datum)
        .bind(&course)
        .bind(&heading)
        .bind(&speed)
        .bind(&vrate)
        .bind(&quality)
        .bind(&label)
        .bind(&attrs)
        .bind(&geom)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(WriteOutcome {
            inserted,
            deduped: rows.len() as u64 - inserted,
            skipped,
        })
    }

    /// Everything currently inside a box.
    ///
    /// Antimeridian-crossing boxes are split before querying: PostGIS
    /// `ST_MakeEnvelope` cannot express a wrapped extent, and passing one
    /// silently returns the complement of what was asked for.
    pub async fn entities_in_bbox(
        &self,
        bbox: BoundingBox,
        filter: &EntityFilter,
        limit: i64,
    ) -> Result<Vec<EntityRow>, StoreError> {
        let parts = bbox.split_at_antimeridian();
        let mut out = Vec::new();
        for part in parts {
            let rows = sqlx::query_as::<_, EntityRow>(&format!(
                r#"
                SELECT entity_kind, entity_key, source_id, layer_id, observed_at,
                       ST_X(position) AS lon, ST_Y(position) AS lat,
                       ST_AsGeoJSON(geom)::jsonb AS geom,
                       alt_m, alt_datum,
                       course_deg, heading_deg, speed_mps, vrate_mps,
                       quality, label, attrs
                FROM entities
                WHERE (position && ST_MakeEnvelope($1, $2, $3, $4, 4326)
                       OR geom && ST_MakeEnvelope($1, $2, $3, $4, 4326))
                  AND ($5::text[] IS NULL OR layer_id = ANY($5))
                  AND ($6::text[] IS NULL OR entity_kind = ANY($6))
                  AND {horizon}
                ORDER BY observed_at DESC
                LIMIT $7
                "#,
                horizon = crate::horizon::within_horizon("entity_kind", "observed_at", "now()"),
            ))
            .bind(part.west)
            .bind(part.south)
            .bind(part.east)
            .bind(part.north)
            .bind(filter.layers_arg())
            .bind(filter.kinds_arg())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
            out.extend(rows);
        }
        out.truncate(limit as usize);
        Ok(out)
    }

    /// Everything that was inside a box at a past instant — the DVR query.
    ///
    /// Resolves against `tracks_1m`, picking each entity's newest bucket at or
    /// before `at`. The horizon bounds how far back a single stale sample can be
    /// dragged forward: without it, an aircraft that landed hours earlier keeps
    /// appearing in every later snapshot forever. It is
    /// [`crate::horizon`]'s per-kind table rather than a flat window, and the
    /// same one the live query uses — so rewinding shows the population that
    /// *was* live then, rather than a differently-filtered one.
    pub async fn entities_at(
        &self,
        bbox: BoundingBox,
        at: DateTime<Utc>,
        filter: &EntityFilter,
        limit: i64,
    ) -> Result<Vec<EntityRow>, StoreError> {
        let parts = bbox.split_at_antimeridian();
        let mut out = Vec::new();
        for part in parts {
            let rows = sqlx::query_as::<_, EntityRow>(&format!(
                r#"
                SELECT DISTINCT ON (t.entity_kind, t.entity_key)
                       t.entity_kind, t.entity_key, t.source_id,
                       COALESCE(s.layer_id, t.source_id) AS layer_id,
                       t.bucket AS observed_at,
                       ST_X(t.position) AS lon, ST_Y(t.position) AS lat,
                       ST_AsGeoJSON(t.geom)::jsonb AS geom,
                       t.alt_m, t.alt_datum,
                       t.course_deg, t.heading_deg, t.speed_mps, t.vrate_mps,
                       t.quality, t.label,
                       '{{}}'::jsonb AS attrs
                FROM tracks_1m t
                LEFT JOIN sources s ON s.source_id = t.source_id
                WHERE t.bucket <= $5
                  AND {horizon}
                  AND t.position && ST_MakeEnvelope($1, $2, $3, $4, 4326)
                  AND ($6::text[] IS NULL OR COALESCE(s.layer_id, t.source_id) = ANY($6))
                  AND ($7::text[] IS NULL OR t.entity_kind = ANY($7))
                ORDER BY t.entity_kind, t.entity_key, t.bucket DESC
                LIMIT $8
                "#,
                horizon = crate::horizon::within_horizon("t.entity_kind", "t.bucket", "$5"),
            ))
            .bind(part.west)
            .bind(part.south)
            .bind(part.east)
            .bind(part.north)
            .bind(at)
            .bind(filter.layers_arg())
            .bind(filter.kinds_arg())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
            out.extend(rows);
        }
        out.truncate(limit as usize);
        Ok(out)
    }

    /// One entity's history over a window, at one-minute resolution.
    pub async fn track(
        &self,
        entity: &EntityId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<model::TrackPoint>, StoreError> {
        let rows = sqlx::query_as::<_, model::TrackPoint>(
            r#"
            SELECT bucket AS at,
                   ST_X(position) AS lon, ST_Y(position) AS lat,
                   alt_m, alt_datum, course_deg, speed_mps
            FROM tracks_1m
            WHERE entity_kind = $1 AND entity_key = $2
              AND bucket >= $3 AND bucket <= $4
              AND position IS NOT NULL
            ORDER BY bucket ASC
            "#,
        )
        .bind(entity.kind.as_str())
        .bind(&entity.key)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Register a driver, or refresh its static metadata if it already exists.
    ///
    /// Health columns are deliberately untouched on conflict: a restart must not
    /// reset a source's observation count or wipe the record of why it was
    /// failing.
    /// Register a source, saying whether it stands on its own.
    ///
    /// `member_of` is `None` for a plain source or a failover chain, and names
    /// the chain for a provider inside one. Without it a chain and the provider
    /// currently serving it are indistinguishable rows, which showed up as the
    /// same feed listed twice and as layer observation totals that counted the
    /// chain's work and each member's contribution to it. See migration 0007.
    pub async fn register_source(
        &self,
        descriptor: &argus_core::SourceDescriptor,
        member_of: Option<&argus_core::SourceId>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO sources (source_id, layer_id, display_name, entity_kind,
                                 cost_class, attribution, member_of)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (source_id) DO UPDATE SET
                layer_id     = EXCLUDED.layer_id,
                display_name = EXCLUDED.display_name,
                entity_kind  = EXCLUDED.entity_kind,
                cost_class   = EXCLUDED.cost_class,
                attribution  = EXCLUDED.attribution,
                member_of    = EXCLUDED.member_of,
                updated_at   = now()
            "#,
        )
        .bind(descriptor.id.as_str())
        .bind(descriptor.layer_id.as_str())
        .bind(&descriptor.display_name)
        .bind(descriptor.kind.as_str())
        .bind(model::cost_class_str(descriptor.cost))
        .bind(serde_json::to_value(&descriptor.attribution).unwrap_or_default())
        .bind(member_of.map(argus_core::SourceId::as_str))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persist a source's current health so both clients can render it honestly.
    ///
    /// `state_since` only moves when the state actually changes — a feed that
    /// has been stale for an hour should say so, not claim it went stale on the
    /// most recent retry.
    pub async fn update_source_health(
        &self,
        source_id: &argus_core::SourceId,
        health: &argus_core::SourceHealth,
        observations_delta: u64,
    ) -> Result<(), StoreError> {
        self.update_source_health_inner(source_id, health, Some(observations_delta), None)
            .await
    }

    /// As above, but sets the observation count outright rather than adding to
    /// it. Chain members report a running total they own, so accumulating it
    /// here would square the count on every poll.
    pub async fn set_source_health(
        &self,
        source_id: &argus_core::SourceId,
        health: &argus_core::SourceHealth,
        observations_total: u64,
    ) -> Result<(), StoreError> {
        self.update_source_health_inner(source_id, health, None, Some(observations_total))
            .await
    }

    async fn update_source_health_inner(
        &self,
        source_id: &argus_core::SourceId,
        health: &argus_core::SourceHealth,
        observations_delta: Option<u64>,
        observations_total: Option<u64>,
    ) -> Result<(), StoreError> {
        let (state, error, lag_ms) = model::health_columns(health);
        sqlx::query(
            r#"
            UPDATE sources SET
                state        = $2,
                state_since  = CASE WHEN state IS DISTINCT FROM $2 THEN now() ELSE state_since END,
                last_error   = $3,
                last_lag_ms  = $4,
                last_success = CASE WHEN $2 IN ('live', 'delayed', 'degraded')
                                    THEN now() ELSE last_success END,
                observations = COALESCE($6, observations + $5),
                updated_at   = now()
            WHERE source_id = $1
            "#,
        )
        .bind(source_id.as_str())
        .bind(state)
        .bind(error)
        .bind(lag_ms)
        .bind(observations_delta.unwrap_or(0) as i64)
        .bind(observations_total.map(|n| n as i64))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every registered source with its current health, for `GET /v1/sources`.
    pub async fn list_sources(&self) -> Result<Vec<model::SourceRow>, StoreError> {
        let rows = sqlx::query_as::<_, model::SourceRow>(
            r#"
            SELECT source_id, layer_id, display_name, entity_kind, cost_class,
                   state, state_since, last_success, last_error, last_lag_ms,
                   observations, attribution, member_of
            FROM sources
            -- Each chain immediately followed by its own providers, so a client
            -- that renders this in order gets the hierarchy for free: group by
            -- COALESCE(member_of, source_id), parent first.
            ORDER BY layer_id,
                     COALESCE(member_of, source_id),
                     (member_of IS NOT NULL),
                     source_id
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Bytes on disk attributable to Argus. Drives the capture-degradation
    /// guard: filling the filesystem must not be a way this daemon fails.
    pub async fn total_bytes(&self) -> Result<i64, StoreError> {
        let bytes: Option<i64> = sqlx::query_scalar(
            "SELECT sum(total_bytes)::bigint FROM timescaledb_information.hypertables h
             JOIN LATERAL hypertable_detailed_size(
                 format('%I.%I', h.hypertable_schema, h.hypertable_name)::regclass) ON true",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(bytes.unwrap_or(0))
    }
}

/// Refuse an observation whose timestamp cannot be real, and say so loudly.
///
/// The store is the last place this can be caught. Past this point a bad
/// timestamp is permanent: the live-state upsert only accepts a row newer than
/// the one it holds, so a reading from the far future locks that entity out of
/// every subsequent update. Logging at warn rather than debug is deliberate —
/// this only fires on a driver bug, and a driver bug that silently drops rows
/// is worse than one that is noisy about it.
fn temporally_plausible(o: &Observation) -> bool {
    if o.is_temporally_plausible() {
        return true;
    }
    tracing::warn!(
        source = %o.source_id,
        entity = %o.entity,
        observed_at = %o.observed_at,
        "refusing an observation with an implausible timestamp; this is a driver bug"
    );
    false
}

/// Convert a stored row back into the core types.
impl EntityRow {
    pub fn entity_id(&self) -> Option<EntityId> {
        model::parse_entity_kind(&self.entity_kind)
            .map(|kind| EntityId::new(kind, self.entity_key.clone()))
    }

    pub fn position(&self) -> Option<Position> {
        let (lon, lat) = (self.lon?, self.lat?);
        Some(Position {
            lon,
            lat,
            alt_m: self.alt_m,
            datum: self
                .alt_datum
                .as_deref()
                .and_then(model::parse_alt_datum)
                .unwrap_or(AltitudeDatum::Geoid),
        })
    }

    pub fn quality(&self) -> Quality {
        model::parse_quality(&self.quality).unwrap_or(Quality::Stale)
    }

    pub fn kind(&self) -> Option<EntityKind> {
        model::parse_entity_kind(&self.entity_kind)
    }
}
