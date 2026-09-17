//! Features: the things that do not move.
//!
//! A cable, a platform, a wind farm, a boundary. `EntityKind::Feature` is
//! excluded from the time-series store by design — re-recording a submarine
//! cable every poll would be pure noise — and lives in the `features` table
//! instead, versioned: a row is current while `valid_to` is NULL, and a
//! change to its geometry, label or attributes closes it and opens a new
//! row, so a DVR query at a past instant still resolves the geography that
//! was live then.
//!
//! The write is therefore a comparison, not an insert. A poll that reads
//! the same register it read yesterday writes nothing, and the outcome
//! reports every unchanged feature as a duplicate, which is what it is.

use crate::{EntityFilter, EntityRow, Store, StoreError, WriteOutcome};
use argus_core::entity::{EntityKind, Observation};
use argus_core::geo::BoundingBox;
use chrono::{DateTime, Utc};
use geozero::ToWkb;

impl Store {
    /// Record the current state of every feature-kind observation, opening a
    /// new version only where something changed.
    ///
    /// The layer comes from the source registry, so a source must be
    /// registered before its features are written — which the runtime does
    /// at startup.
    pub async fn write_features(
        &self,
        observations: &[Observation],
    ) -> Result<WriteOutcome, StoreError> {
        let mut outcome = WriteOutcome::default();
        let mut tx = self.pool.begin().await?;
        for o in observations {
            if o.entity.kind != EntityKind::Feature {
                outcome.skipped += 1;
                continue;
            }
            // The geometry is the shape if there is one, else the point.
            let geometry = match (&o.geom, o.position) {
                (Some(g), _) => g.clone(),
                (None, Some(p)) if p.is_plausible() => {
                    geo_types::Geometry::Point(geo_types::Point::new(p.lon, p.lat))
                }
                _ => {
                    outcome.skipped += 1;
                    continue;
                }
            };
            let Ok(ewkb) = geometry.to_ewkb(geozero::CoordDimensions::xy(), Some(4326)) else {
                outcome.skipped += 1;
                continue;
            };
            let attrs = if o.attrs.is_null() {
                serde_json::json!({})
            } else {
                o.attrs.clone()
            };

            // The comparison happens in the database, in the database's own
            // terms: jsonb normalises numbers (41.0 becomes 41) and PostGIS
            // normalises EWKB, so comparing either in Rust says "changed"
            // for every feature every poll.
            let current: Option<(i64, bool)> = sqlx::query_as(
                r#"
                SELECT f.feature_id,
                       ST_OrderingEquals(f.geom, ST_GeomFromEWKB($3))
                       AND f.attrs = $4::jsonb
                       AND f.label IS NOT DISTINCT FROM $5 AS unchanged
                FROM features f
                WHERE f.layer_id = (SELECT layer_id FROM sources WHERE source_id = $1)
                  AND f.feature_key = $2
                  AND f.valid_to IS NULL
                "#,
            )
            .bind(o.source_id.as_str())
            .bind(&o.entity.key)
            .bind(&ewkb)
            .bind(&attrs)
            .bind(&o.label)
            .fetch_optional(&mut *tx)
            .await?;

            if let Some((feature_id, unchanged)) = current {
                if unchanged {
                    outcome.deduped += 1;
                    continue;
                }
                sqlx::query("UPDATE features SET valid_to = $2 WHERE feature_id = $1")
                    .bind(feature_id)
                    .bind(o.observed_at)
                    .execute(&mut *tx)
                    .await?;
            }

            sqlx::query(
                r#"
                INSERT INTO features (layer_id, source_id, feature_key, geom, label, attrs, valid_from)
                SELECT s.layer_id, s.source_id, $2, ST_GeomFromEWKB($3), $4, $5, $6
                FROM sources s WHERE s.source_id = $1
                "#,
            )
            .bind(o.source_id.as_str())
            .bind(&o.entity.key)
            .bind(&ewkb)
            .bind(&o.label)
            .bind(&attrs)
            .bind(o.observed_at)
            .execute(&mut *tx)
            .await?;
            outcome.inserted += 1;
        }
        tx.commit().await?;
        Ok(outcome)
    }

    /// Current feature versions per layer, for the catalogue's live count.
    /// The features inside one box as entity rows — the version current now,
    /// or the one current at `at` — so a viewport read and a WebSocket
    /// snapshot carry the wind farms and the food businesses alongside the
    /// aircraft. The tiler reads the same table with its own clipping; this
    /// is the row shape the list route and the stream already speak. A
    /// polygon's row also gets a point on its surface, for a client that
    /// places a label.
    pub(crate) async fn features_in_bbox(
        &self,
        part: &BoundingBox,
        at: Option<DateTime<Utc>>,
        filter: &EntityFilter,
        limit: i64,
    ) -> Result<Vec<EntityRow>, StoreError> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, EntityRow>(
            r#"
            SELECT 'feature' AS entity_kind, feature_key AS entity_key, source_id, layer_id,
                   valid_from AS observed_at,
                   ST_X(ST_PointOnSurface(geom)) AS lon, ST_Y(ST_PointOnSurface(geom)) AS lat,
                   CASE WHEN GeometryType(geom) = 'POINT' THEN NULL
                        ELSE ST_AsGeoJSON(geom)::jsonb END AS geom,
                   NULL::double precision AS alt_m, NULL::text AS alt_datum,
                   NULL::real AS course_deg, NULL::real AS heading_deg,
                   NULL::real AS speed_mps, NULL::real AS vrate_mps,
                   'live' AS quality, label, attrs
            FROM features
            WHERE geom && ST_MakeEnvelope($1, $2, $3, $4, 4326)
              AND valid_from <= COALESCE($8, now())
              AND (valid_to IS NULL OR valid_to > COALESCE($8, now()))
              AND ($5::text[] IS NULL OR layer_id = ANY($5))
              AND ($6::text[] IS NULL OR 'feature' = ANY($6))
            ORDER BY valid_from DESC
            LIMIT $7
            "#,
        )
        .bind(part.west)
        .bind(part.south)
        .bind(part.east)
        .bind(part.north)
        .bind(filter.layers_arg())
        .bind(filter.kinds_arg())
        .bind(limit)
        .bind(at)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn feature_counts(&self) -> Result<Vec<(String, i64)>, StoreError> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT layer_id, count(*) FROM features WHERE valid_to IS NULL GROUP BY layer_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
