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
        if row.is_some() || id.kind != argus_core::EntityKind::Feature {
            return Ok(row);
        }
        // Features live in their own versioned table; the current version
        // is the card. The point is a point on the surface, so a card for
        // a wind farm anchors inside the farm.
        let row = sqlx::query_as::<_, model::EntityRow>(
            r#"
            SELECT 'feature' AS entity_kind, feature_key AS entity_key, source_id, layer_id,
                   valid_from AS observed_at,
                   ST_X(ST_PointOnSurface(geom)) AS lon, ST_Y(ST_PointOnSurface(geom)) AS lat,
                   CASE WHEN ST_GeometryType(geom) = 'ST_Point' THEN NULL
                        ELSE ST_AsGeoJSON(geom)::jsonb END AS geom,
                   NULL::double precision AS alt_m, NULL::alt_datum AS alt_datum,
                   NULL::real AS course_deg, NULL::real AS heading_deg,
                   NULL::real AS speed_mps, NULL::real AS vrate_mps,
                   'live'::quality AS quality, label, attrs
            FROM features
            WHERE feature_key = $1 AND valid_to IS NULL
            ORDER BY valid_from DESC
            LIMIT 1
            "#,
        )
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
        let rows = sqlx::query_as::<_, model::LayerRow>(&format!(
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
                       END AS rank,
                       -- 0 for a chain or a standalone source, 1 for a provider
                       -- inside a chain. See migration 0007.
                       (s.member_of IS NOT NULL)::int AS top
                FROM sources s
            ),
            counts AS (
                -- Counted under the same horizon the map draws under, or the
                -- rail says 1,528 aircraft over a map showing 212.
                SELECT layer_id, count(*) AS live_entities
                FROM entities
                WHERE {live_horizon}
                GROUP BY layer_id
            )
            SELECT r.layer_id,
                   -- `top` sorts a chain (or a standalone source) ahead of the
                   -- providers inside it, so the row that *represents* the
                   -- layer wins these regardless of how a member is faring.
                   -- Reading the name off whichever row happened to rank best
                   -- meant the layer could be titled after a fallback that was
                   -- merely healthier than the chain it sits in.
                   (array_agg(r.entity_kind  ORDER BY r.top, r.rank, r.source_id))[1] AS entity_kind,
                   (array_agg(r.display_name ORDER BY r.top, r.rank, r.source_id))[1] AS display_name,
                   (array_agg(r.state        ORDER BY r.top, r.rank, r.source_id))[1] AS state,
                   array_agg(r.source_id ORDER BY r.top, r.rank, r.source_id)         AS source_ids,
                   max(r.last_success)                                          AS last_success,
                   -- Top-level rows only. A chain's members each carry their
                   -- own contribution to the same work, so summing everything
                   -- counts it twice: the flights layer reported 327,601
                   -- observations where the chain alone had 321,983.
                   COALESCE(sum(r.observations) FILTER (WHERE r.member_of IS NULL), 0)::bigint
                                                                                AS observations,
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
            live_horizon = crate::horizon::within_horizon("entity_kind", "observed_at", "now()"),
        ))
        .fetch_all(&self.pool)
        .await?;
        // Features are counted from their own table: the live query above
        // only sees the time-series entities.
        let mut rows = rows;
        for (layer_id, n) in self.feature_counts().await? {
            if let Some(row) = rows.iter_mut().find(|r| r.layer_id == layer_id) {
                row.live_entities += n;
            }
        }
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
                    sqlx::query_as::<_, model::TileRow>(&format!(
                        r#"
                        SELECT entity_kind, entity_key, source_id, layer_id, observed_at,
                               -- Clipped to the (buffered) tile box: a country-sized
                               -- polygon encoded whole into every tile it touches
                               -- overflows the renderer's 16-bit vertex coordinates
                               -- at the zooms where one tile is a county.
                               ST_ClipByBox2D(ST_Simplify(COALESCE(geom, position), $5),
                                              ST_MakeEnvelope($1, $2, $3, $4, 4326)) AS geometry,
                               alt_m, course_deg, heading_deg, speed_mps, vrate_mps,
                               quality, label
                        FROM entities
                        WHERE (position && ST_MakeEnvelope($1, $2, $3, $4, 4326)
                               OR geom && ST_MakeEnvelope($1, $2, $3, $4, 4326))
                          AND ($6::text[] IS NULL OR layer_id = ANY($6))
                          AND ($7::text[] IS NULL OR entity_kind = ANY($7))
                          AND COALESCE(geom, position) IS NOT NULL
                          AND {horizon}
                        ORDER BY observed_at DESC
                        LIMIT $8
                        "#,
                        horizon =
                            crate::horizon::within_horizon("entity_kind", "observed_at", "now()"),
                    ))
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
                    sqlx::query_as::<_, model::TileRow>(&format!(
                        r#"
                        SELECT DISTINCT ON (t.entity_kind, t.entity_key)
                               t.entity_kind, t.entity_key, t.source_id,
                               COALESCE(s.layer_id, t.source_id) AS layer_id,
                               t.bucket AS observed_at,
                               ST_ClipByBox2D(ST_Simplify(COALESCE(t.geom, t.position), $5),
                                              ST_MakeEnvelope($1, $2, $3, $4, 4326)) AS geometry,
                               t.alt_m, t.course_deg, t.heading_deg,
                               t.speed_mps, t.vrate_mps, t.quality, t.label
                        FROM tracks_1m t
                        LEFT JOIN sources s ON s.source_id = t.source_id
                        WHERE t.bucket <= $9
                          AND {horizon}
                          AND t.position && ST_MakeEnvelope($1, $2, $3, $4, 4326)
                          AND ($6::text[] IS NULL OR COALESCE(s.layer_id, t.source_id) = ANY($6))
                          AND ($7::text[] IS NULL OR t.entity_kind = ANY($7))
                          AND COALESCE(t.geom, t.position) IS NOT NULL
                        ORDER BY t.entity_kind, t.entity_key, t.bucket DESC
                        LIMIT $8
                        "#,
                        horizon = crate::horizon::within_horizon("t.entity_kind", "t.bucket", "$9"),
                    ))
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

            // Features, from their own table: the current version live, or
            // the version that was current at the DVR instant.
            let features = sqlx::query_as::<_, model::TileRow>(
                r#"
                SELECT 'feature' AS entity_kind, feature_key AS entity_key, source_id, layer_id,
                       valid_from AS observed_at,
                       ST_ClipByBox2D(ST_Simplify(geom, $5),
                                      ST_MakeEnvelope($1, $2, $3, $4, 4326)) AS geometry,
                       NULL::double precision AS alt_m,
                       NULL::real AS course_deg, NULL::real AS heading_deg,
                       NULL::real AS speed_mps, NULL::real AS vrate_mps,
                       'live' AS quality, label
                FROM features
                WHERE geom && ST_MakeEnvelope($1, $2, $3, $4, 4326)
                  AND valid_from <= COALESCE($9, now())
                  AND (valid_to IS NULL OR valid_to > COALESCE($9, now()))
                  AND ($6::text[] IS NULL OR layer_id = ANY($6))
                  AND ($7::text[] IS NULL OR 'feature' = ANY($7))
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
            .await?;
            out.extend(features);
        }
        out.truncate(limit as usize);
        Ok(out)
    }
}

/// The store as a geometry cache.
///
/// See `argus_core::cache` for why this is an implementation of a narrow trait
/// rather than drivers being handed a `Store`: a driver that can reach the
/// database can invent its own persistence, and then nobody knows where the
/// data lives.
#[async_trait::async_trait]
impl argus_core::GeometryCache for Store {
    async fn get(&self, key: &str) -> Option<geo_types::Geometry<f64>> {
        let row: Option<(geozero::wkb::Decode<geo_types::Geometry<f64>>,)> =
            sqlx::query_as("SELECT geom FROM reference_geometry WHERE cache_key = $1")
                .bind(key)
                .fetch_optional(&self.pool)
                .await
                .unwrap_or_else(|err| {
                    // A cache that cannot be read is not a reason to fail a
                    // poll; the driver will fetch instead, which is exactly
                    // what it would do on a miss.
                    tracing::warn!(key, "reference geometry lookup failed: {err}");
                    None
                });
        row.and_then(|(decoded,)| decoded.geometry)
    }

    async fn put(&self, key: &str, geometry: &geo_types::Geometry<f64>) {
        use geozero::ToWkb;
        let Ok(bytes) = geometry.to_ewkb(geozero::CoordDimensions::xy(), Some(4326)) else {
            tracing::warn!(key, "reference geometry could not be encoded; not cached");
            return;
        };
        if let Err(err) = sqlx::query(
            "INSERT INTO reference_geometry (cache_key, geom)
             VALUES ($1, ST_GeomFromEWKB($2))
             ON CONFLICT (cache_key) DO UPDATE
               SET geom = EXCLUDED.geom, fetched_at = now()",
        )
        .bind(key)
        .bind(&bytes)
        .execute(&self.pool)
        .await
        {
            tracing::warn!(key, "could not cache reference geometry: {err}");
        }
    }
}

/// The catalogue Argus already tracks, read out of its own history.
///
/// Used by a failover satellites provider that has no way of its own to decide
/// which objects matter — see [`argus_core::TrackedCatalogue`]. Reading it from
/// the entities table rather than a config list means the fallback follows
/// whatever the primary has actually been collecting, including objects added
/// since this code was written.
#[async_trait::async_trait]
impl argus_core::TrackedCatalogue for Store {
    async fn tracked_norad_ids(&self) -> Vec<u64> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT attrs->>'norad_id'
               FROM entities
              WHERE entity_kind = 'satellite'
                AND attrs ? 'norad_id'
              ORDER BY 1",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap_or_else(|err| {
            // An unreadable catalogue is a fallback that cannot start, not a
            // daemon that should stop. The chain reports it either way.
            tracing::warn!("tracked catalogue lookup failed: {err}");
            Vec::new()
        });

        let mut ids: Vec<u64> = rows
            .into_iter()
            .filter_map(|(s,)| s.parse::<u64>().ok())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}
