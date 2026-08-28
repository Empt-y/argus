//! Persistence for Argus: the DVR write path and the queries that read it back.
//!
//! Everything here speaks `argus_core` types. No SQL escapes this crate.

use argus_core::entity::{AltitudeDatum, EntityId, EntityKind, Observation, Position, Quality};
use argus_core::geo::BoundingBox;
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;

pub mod model;

pub use model::EntityRow;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("required extension {0} is not installed in the target database")]
    MissingExtension(&'static str),
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
    ) -> Result<u64, StoreError> {
        let rows: Vec<&Observation> = observations
            .iter()
            .filter(|o| o.is_meaningful() && o.entity.kind.is_timeseries())
            .collect();
        if rows.is_empty() {
            return Ok(0);
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
        let attrs: Vec<serde_json::Value> = rows.iter().map(|o| o.attrs.clone()).collect();

        sqlx::query(
            r#"
            INSERT INTO observations (
                observed_at, ingested_at, source_id, entity_kind, entity_key,
                position, alt_m, alt_datum,
                course_deg, heading_deg, speed_mps, vrate_mps,
                quality, label, attrs
            )
            SELECT
                u.observed_at, u.ingested_at, u.source_id, u.entity_kind, u.entity_key,
                CASE WHEN u.lon IS NULL OR u.lat IS NULL THEN NULL
                     ELSE ST_SetSRID(ST_MakePoint(u.lon, u.lat), 4326) END,
                u.alt_m, u.alt_datum,
                u.course_deg, u.heading_deg, u.speed_mps, u.vrate_mps,
                u.quality, u.label, u.attrs
            FROM UNNEST(
                $1::timestamptz[], $2::timestamptz[], $3::text[], $4::text[], $5::text[],
                $6::float8[], $7::float8[], $8::float8[], $9::text[],
                $10::real[], $11::real[], $12::real[], $13::real[],
                $14::text[], $15::text[], $16::jsonb[]
            ) AS u(
                observed_at, ingested_at, source_id, entity_kind, entity_key,
                lon, lat, alt_m, alt_datum,
                course_deg, heading_deg, speed_mps, vrate_mps,
                quality, label, attrs
            )
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
        .execute(&mut *tx)
        .await?;

        // Live state. `WHERE EXCLUDED.observed_at > entities.observed_at` is
        // load-bearing: batches can contain out-of-order samples, and two
        // sources can describe the same aircraft at different lags. Without the
        // guard, a late-arriving stale fix overwrites a newer one and the
        // contact visibly jumps backwards.
        sqlx::query(
            r#"
            INSERT INTO entities (
                entity_kind, entity_key, source_id, layer_id, observed_at,
                position, alt_m, alt_datum,
                course_deg, heading_deg, speed_mps, vrate_mps,
                quality, label, attrs
            )
            SELECT DISTINCT ON (u.entity_kind, u.entity_key)
                u.entity_kind, u.entity_key, u.source_id,
                COALESCE(s.layer_id, u.source_id), u.observed_at,
                CASE WHEN u.lon IS NULL OR u.lat IS NULL THEN NULL
                     ELSE ST_SetSRID(ST_MakePoint(u.lon, u.lat), 4326) END,
                u.alt_m, u.alt_datum,
                u.course_deg, u.heading_deg, u.speed_mps, u.vrate_mps,
                u.quality, u.label, u.attrs
            FROM UNNEST(
                $1::timestamptz[], $2::text[], $3::text[], $4::text[],
                $5::float8[], $6::float8[], $7::float8[], $8::text[],
                $9::real[], $10::real[], $11::real[], $12::real[],
                $13::text[], $14::text[], $15::jsonb[]
            ) AS u(
                observed_at, source_id, entity_kind, entity_key,
                lon, lat, alt_m, alt_datum,
                course_deg, heading_deg, speed_mps, vrate_mps,
                quality, label, attrs
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
                attrs       = EXCLUDED.attrs
            WHERE EXCLUDED.observed_at > entities.observed_at
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
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(rows.len() as u64)
    }

    /// Everything currently inside a box.
    ///
    /// Antimeridian-crossing boxes are split before querying: PostGIS
    /// `ST_MakeEnvelope` cannot express a wrapped extent, and passing one
    /// silently returns the complement of what was asked for.
    pub async fn entities_in_bbox(
        &self,
        bbox: BoundingBox,
        layers: &[String],
        limit: i64,
    ) -> Result<Vec<EntityRow>, StoreError> {
        let parts = bbox.split_at_antimeridian();
        let mut out = Vec::new();
        for part in parts {
            let rows = sqlx::query_as::<_, EntityRow>(
                r#"
                SELECT entity_kind, entity_key, source_id, layer_id, observed_at,
                       ST_X(position) AS lon, ST_Y(position) AS lat,
                       alt_m, alt_datum,
                       course_deg, heading_deg, speed_mps, vrate_mps,
                       quality, label, attrs
                FROM entities
                WHERE position && ST_MakeEnvelope($1, $2, $3, $4, 4326)
                  AND ($5::text[] IS NULL OR layer_id = ANY($5))
                ORDER BY observed_at DESC
                LIMIT $6
                "#,
            )
            .bind(part.west)
            .bind(part.south)
            .bind(part.east)
            .bind(part.north)
            .bind(if layers.is_empty() { None } else { Some(layers) })
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
    /// before `at`. The `INTERVAL '15 minutes'` floor bounds how far back a
    /// single stale sample can be dragged forward: without it, an aircraft that
    /// landed hours earlier keeps appearing in every later snapshot forever.
    pub async fn entities_at(
        &self,
        bbox: BoundingBox,
        at: DateTime<Utc>,
        layers: &[String],
        limit: i64,
    ) -> Result<Vec<EntityRow>, StoreError> {
        let parts = bbox.split_at_antimeridian();
        let mut out = Vec::new();
        for part in parts {
            let rows = sqlx::query_as::<_, EntityRow>(
                r#"
                SELECT DISTINCT ON (t.entity_kind, t.entity_key)
                       t.entity_kind, t.entity_key, t.source_id,
                       COALESCE(s.layer_id, t.source_id) AS layer_id,
                       t.bucket AS observed_at,
                       ST_X(t.position) AS lon, ST_Y(t.position) AS lat,
                       t.alt_m, t.alt_datum,
                       t.course_deg, t.heading_deg, t.speed_mps, t.vrate_mps,
                       t.quality, t.label,
                       '{}'::jsonb AS attrs
                FROM tracks_1m t
                LEFT JOIN sources s ON s.source_id = t.source_id
                WHERE t.bucket <= $5
                  AND t.bucket > $5 - INTERVAL '15 minutes'
                  AND t.position && ST_MakeEnvelope($1, $2, $3, $4, 4326)
                  AND ($6::text[] IS NULL OR COALESCE(s.layer_id, t.source_id) = ANY($6))
                ORDER BY t.entity_kind, t.entity_key, t.bucket DESC
                LIMIT $7
                "#,
            )
            .bind(part.west)
            .bind(part.south)
            .bind(part.east)
            .bind(part.north)
            .bind(at)
            .bind(if layers.is_empty() { None } else { Some(layers) })
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
