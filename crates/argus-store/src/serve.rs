//! Queries that exist to serve clients rather than to run ingest.
//!
//! Split out from the write path deliberately: these are the ones whose cost
//! scales with the number of people watching rather than with the number of
//! feeds, and they have a different failure mode — a slow read here stalls a
//! map pan, not a data capture.

use crate::{EntityFilter, Store, StoreError, model};
use argus_core::entity::EntityId;
use argus_core::geo::BoundingBox;
use chrono::{DateTime, Utc};

/// A live-state row paired with the moment Argus learned it, which is what the
/// delta stream advances its cursor on.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeltaRow {
    #[sqlx(flatten)]
    pub entity: model::EntityRow,
    pub updated_at: DateTime<Utc>,
}

impl Store {
    /// One entity's current state, for the detail card.
    pub async fn entity(&self, id: &EntityId) -> Result<Option<model::EntityRow>, StoreError> {
        let row = sqlx::query_as::<_, model::EntityRow>(
            r#"
            SELECT entity_kind, entity_key, source_id, layer_id, observed_at,
                   ST_X(position) AS lon, ST_Y(position) AS lat,
                   ST_AsGeoJSON(geom)::jsonb AS geom,
                   alt_m, alt_datum,
                   course_deg, heading_deg, speed_mps, vrate_mps,
                   quality, label, attrs
            FROM entities
            WHERE entity_kind = $1 AND entity_key = $2
            "#,
        )
        .bind(id.kind.as_str())
        .bind(&id.key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// The layer catalogue, rolled up from the source registry.
    ///
    /// A layer's state is the best of its sources', not the worst: a failover
    /// chain running on its backup is a working layer, and saying otherwise
    /// would train the operator to ignore the chip. The per-source detail is
    /// still one call away at `/v1/sources`, which is where "why is this
    /// degraded" gets answered.
    pub async fn layers(&self) -> Result<Vec<model::LayerRow>, StoreError> {
        let rows = sqlx::query_as::<_, model::LayerRow>(
            r#"
            WITH ranked AS (
                SELECT s.*,
                       -- Ordering, best first. Mirrors argus_core::SourceHealth:
                       -- the configured-off states sort below real failures
                       -- because a layer nobody enabled is less interesting than
                       -- one that is trying and failing.
                       CASE state
                           WHEN 'live'     THEN 0
                           WHEN 'delayed'  THEN 1
                           WHEN 'degraded' THEN 2
                           WHEN 'stale'    THEN 3
                           WHEN 'failed'   THEN 4
                           WHEN 'unknown'  THEN 5
                           ELSE 6
                       END AS rank
                FROM sources s
            ),
            counts AS (
                SELECT layer_id, count(*) AS live_entities
                FROM entities GROUP BY layer_id
            )
            SELECT r.layer_id,
                   (array_agg(r.entity_kind  ORDER BY r.rank, r.source_id))[1] AS entity_kind,
                   (array_agg(r.display_name ORDER BY r.rank, r.source_id))[1] AS display_name,
                   (array_agg(r.state        ORDER BY r.rank, r.source_id))[1] AS state,
                   array_agg(r.source_id ORDER BY r.rank, r.source_id)         AS source_ids,
                   max(r.last_success)                                          AS last_success,
                   sum(r.observations)::bigint                                  AS observations,
                   -- Every contributing source's credit line, not just the
                   -- winner's: an ODbL or CC BY-NC-SA feed must be attributed
                   -- even on the days its data came from the other member.
                   jsonb_agg(DISTINCT r.attribution)                            AS attribution,
                   COALESCE(max(c.live_entities), 0)::bigint                    AS live_entities
            FROM ranked r
            LEFT JOIN counts c ON c.layer_id = r.layer_id
            GROUP BY r.layer_id
            ORDER BY r.layer_id
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Everything Argus has learned since `since`, for the delta stream.
    ///
    /// Keyed on `updated_at` rather than `observed_at`: a client that dropped
    /// its connection wants what the server *learned* while it was away, and a
    /// late-arriving fix with an old source timestamp is exactly the case that
    /// an `observed_at` cursor would silently skip.
    pub async fn entities_changed_since(
        &self,
        bbox: BoundingBox,
        filter: &EntityFilter,
        since: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<DeltaRow>, StoreError> {
        let mut out = Vec::new();
        for part in bbox.split_at_antimeridian() {
            let rows = sqlx::query_as::<_, DeltaRow>(
                r#"
                SELECT entity_kind, entity_key, source_id, layer_id, observed_at,
                       ST_X(position) AS lon, ST_Y(position) AS lat,
                       ST_AsGeoJSON(geom)::jsonb AS geom,
                       alt_m, alt_datum,
                       course_deg, heading_deg, speed_mps, vrate_mps,
                       quality, label, attrs, updated_at
                FROM entities
                WHERE updated_at > $5
                  AND (position && ST_MakeEnvelope($1, $2, $3, $4, 4326)
                       OR geom && ST_MakeEnvelope($1, $2, $3, $4, 4326))
                  AND ($6::text[] IS NULL OR layer_id = ANY($6))
                  AND ($7::text[] IS NULL OR entity_kind = ANY($7))
                ORDER BY updated_at ASC
                LIMIT $8
                "#,
            )
            .bind(part.west)
            .bind(part.south)
            .bind(part.east)
            .bind(part.north)
            .bind(since)
            .bind(filter.layers_arg())
            .bind(filter.kinds_arg())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
            out.extend(rows);
        }
        out.sort_by_key(|r| r.updated_at);
        out.truncate(limit as usize);
        Ok(out)
    }

    /// Features for one vector tile.
    ///
    /// `simplify_deg` is the tile's own resolution — a vertex that would land
    /// on the same integer tile coordinate as its neighbour cannot be seen, so
    /// dropping it in PostGIS saves both the transfer and the encode. Points
    /// pass through `ST_Simplify` untouched.
    ///
    /// `at` selects the DVR: `None` reads live state, `Some(t)` reads the
    /// one-minute rollup. This is the whole reason the phone gets time travel
    /// for free — the tiler honours it, so nothing above the tile request has
    /// to know time is involved.
    pub async fn tile_rows(
        &self,
        bbox: BoundingBox,
        filter: &EntityFilter,
        at: Option<DateTime<Utc>>,
        simplify_deg: f64,
        limit: i64,
    ) -> Result<Vec<model::TileRow>, StoreError> {
        let mut out = Vec::new();
        for part in bbox.split_at_antimeridian() {
            let rows = match at {
                None => {
                    sqlx::query_as::<_, model::TileRow>(
                        r#"
                        SELECT entity_kind, entity_key, source_id, layer_id, observed_at,
                               ST_Simplify(COALESCE(geom, position), $5) AS geometry,
                               alt_m, course_deg, heading_deg, speed_mps, vrate_mps,
                               quality, label
                        FROM entities
                        WHERE (position && ST_MakeEnvelope($1, $2, $3, $4, 4326)
                               OR geom && ST_MakeEnvelope($1, $2, $3, $4, 4326))
                          AND ($6::text[] IS NULL OR layer_id = ANY($6))
                          AND ($7::text[] IS NULL OR entity_kind = ANY($7))
                          AND COALESCE(geom, position) IS NOT NULL
                        ORDER BY observed_at DESC
                        LIMIT $8
                        "#,
                    )
                    .bind(part.west)
                    .bind(part.south)
                    .bind(part.east)
                    .bind(part.north)
                    .bind(simplify_deg)
                    .bind(filter.layers_arg())
                    .bind(filter.kinds_arg())
                    .bind(limit)
                    .fetch_all(&self.pool)
                    .await?
                }
                Some(at) => {
                    sqlx::query_as::<_, model::TileRow>(
                        r#"
                        SELECT DISTINCT ON (t.entity_kind, t.entity_key)
                               t.entity_kind, t.entity_key, t.source_id,
                               COALESCE(s.layer_id, t.source_id) AS layer_id,
                               t.bucket AS observed_at,
                               ST_Simplify(COALESCE(t.geom, t.position), $5) AS geometry,
                               t.alt_m, t.course_deg, t.heading_deg,
                               t.speed_mps, t.vrate_mps, t.quality, t.label
                        FROM tracks_1m t
                        LEFT JOIN sources s ON s.source_id = t.source_id
                        WHERE t.bucket <= $9
                          AND t.bucket > $9 - INTERVAL '15 minutes'
                          AND t.position && ST_MakeEnvelope($1, $2, $3, $4, 4326)
                          AND ($6::text[] IS NULL OR COALESCE(s.layer_id, t.source_id) = ANY($6))
                          AND ($7::text[] IS NULL OR t.entity_kind = ANY($7))
                          AND COALESCE(t.geom, t.position) IS NOT NULL
                        ORDER BY t.entity_kind, t.entity_key, t.bucket DESC
                        LIMIT $8
                        "#,
                    )
                    .bind(part.west)
                    .bind(part.south)
                    .bind(part.east)
                    .bind(part.north)
                    .bind(simplify_deg)
                    .bind(filter.layers_arg())
                    .bind(filter.kinds_arg())
                    .bind(limit)
                    .bind(at)
                    .fetch_all(&self.pool)
                    .await?
                }
            };
            out.extend(rows);
        }
        out.truncate(limit as usize);
        Ok(out)
    }
}
