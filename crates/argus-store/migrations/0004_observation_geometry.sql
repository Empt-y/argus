-- Carry non-point geography on observations.
--
-- `Observation::geom` has existed since the core model was written, but there
-- was nowhere to put it: `observations` and `entities` held only a Point. Any
-- observation whose shape was its whole meaning — a weather alert area, a fire
-- perimeter, a hurricane forecast cone, an airspace closure — was accepted by
-- `is_meaningful()` and then written with the geometry silently discarded,
-- which is worse than rejecting it, because the row looks fine.
--
-- Kept separate from `position` rather than widening that column. The two
-- answer different questions and both are often present: a cyclone has a
-- current centre AND a forecast cone, an alert has a centroid to label AND an
-- area to shade. Collapsing them would force every consumer to compute a
-- centroid to find out where to put a marker.

ALTER TABLE observations ADD COLUMN geom geometry(Geometry, 4326);
ALTER TABLE entities     ADD COLUMN geom geometry(Geometry, 4326);

-- Only entities get a spatial index on it. `entities` is the live viewport
-- query and is small; `observations` is the highest-write table in the system
-- and its historical shape queries resolve through the rollup, exactly as the
-- point ones do.
CREATE INDEX entities_geom_idx ON entities USING GIST (geom) WHERE geom IS NOT NULL;

-- The 1-minute rollup predates this column, so it cannot simply be altered —
-- a continuous aggregate's shape is fixed at creation. Drop and recreate it
-- with the geometry carried through.
--
-- Recreating loses the materialised history, so the definition below is
-- refreshed over the full retention window on first run. On a young store that
-- is instant; on an established one it is a one-off backfill.
-- Retire the background jobs BEFORE dropping the view. TimescaleDB's scheduler
-- refreshes continuous aggregates on its own timer, and dropping one out from
-- under a running refresh fails with "tuple concurrently deleted" — an error
-- that looks like corruption and is really just a race with the worker.
SELECT remove_continuous_aggregate_policy('tracks_1m', if_exists => true);
SELECT remove_retention_policy('tracks_1m', if_exists => true);

DROP MATERIALIZED VIEW IF EXISTS tracks_1m CASCADE;

CREATE MATERIALIZED VIEW tracks_1m
WITH (timescaledb.continuous) AS
SELECT
    time_bucket(INTERVAL '1 minute', observed_at) AS bucket,
    entity_kind,
    entity_key,
    last(source_id,   observed_at) AS source_id,
    last(position,    observed_at) AS position,
    last(geom,        observed_at) AS geom,
    last(alt_m,       observed_at) AS alt_m,
    last(alt_datum,   observed_at) AS alt_datum,
    last(course_deg,  observed_at) AS course_deg,
    last(heading_deg, observed_at) AS heading_deg,
    last(speed_mps,   observed_at) AS speed_mps,
    last(vrate_mps,   observed_at) AS vrate_mps,
    last(quality,     observed_at) AS quality,
    last(label,       observed_at) AS label,
    count(*)                       AS samples
FROM observations
GROUP BY bucket, entity_kind, entity_key
WITH NO DATA;

SELECT add_continuous_aggregate_policy('tracks_1m',
    start_offset      => INTERVAL '90 days',
    end_offset        => INTERVAL '2 minutes',
    schedule_interval => INTERVAL '1 minute');

SELECT add_retention_policy('tracks_1m', INTERVAL '90 days');

CREATE INDEX tracks_1m_position_idx ON tracks_1m USING GIST (position);
CREATE INDEX tracks_1m_entity_idx   ON tracks_1m (entity_kind, entity_key, bucket DESC);
