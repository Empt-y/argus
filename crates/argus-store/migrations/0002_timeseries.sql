-- The DVR: the raw observation hypertable and the rollup ladder that makes
-- "scrub the planet back through time" affordable.
--
--   observations   raw      7d,  compressed after 1d
--        │  1-minute rollup
--   tracks_1m      cagg    90d
--        │  daily rollup
--   entity_daily   cagg    forever
--
-- Retention windows here are the defaults; argusd overrides them from config at
-- startup, so changing your mind later is a config edit rather than a migration.

-- ---------------------------------------------------------------------------
-- Raw observations
-- ---------------------------------------------------------------------------

CREATE TABLE observations (
    observed_at timestamptz NOT NULL,
    ingested_at timestamptz NOT NULL DEFAULT now(),

    source_id   text        NOT NULL,
    entity_kind entity_kind NOT NULL,
    entity_key  text        NOT NULL,

    position    geometry(Point, 4326),
    alt_m       double precision,
    alt_datum   alt_datum,

    course_deg  real,
    heading_deg real,
    speed_mps   real,
    vrate_mps   real,

    quality     quality NOT NULL,
    label       text,
    attrs       jsonb   NOT NULL DEFAULT '{}'::jsonb
);

-- One-hour chunks: small enough that retention and compression act at a useful
-- granularity, large enough that a day is only 24 chunks to plan over.
SELECT create_hypertable('observations', 'observed_at',
                         chunk_time_interval => INTERVAL '1 hour');

-- Track detail for one entity ("where has this aircraft been") is the query
-- this table actually serves, so it gets the index.
CREATE INDEX observations_entity_idx
    ON observations (entity_kind, entity_key, observed_at DESC);

-- There is deliberately NO spatial index on this table. A GiST index on the
-- highest-write table in the system would cost on every insert, and it would be
-- serving a query that belongs elsewhere: "what was in this box at time T"
-- resolves against tracks_1m, which is ~1/30th the size and already
-- time-bucketed. Put the spatial index where the spatial queries run.

ALTER TABLE observations SET (
    timescaledb.compress,
    timescaledb.compress_segmentby = 'entity_kind, entity_key',
    timescaledb.compress_orderby   = 'observed_at DESC'
);

SELECT add_compression_policy('observations', INTERVAL '1 day');
SELECT add_retention_policy('observations', INTERVAL '7 days');

-- ---------------------------------------------------------------------------
-- One-minute tracks — the DVR scrubber's data source
-- ---------------------------------------------------------------------------

-- `last(...)` picks the newest sample in each bucket rather than averaging.
-- Averaging positions is wrong here: the mean of two fixes either side of a
-- turn is a point the aircraft was never at, and averaging across the
-- antimeridian produces a position in entirely the wrong hemisphere.
CREATE MATERIALIZED VIEW tracks_1m
WITH (timescaledb.continuous) AS
SELECT
    time_bucket(INTERVAL '1 minute', observed_at) AS bucket,
    entity_kind,
    entity_key,
    last(source_id,   observed_at) AS source_id,
    last(position,    observed_at) AS position,
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

-- Refresh trails 2 minutes behind so a bucket is closed before it is
-- materialised, and covers 90 days so a restarted daemon backfills rather than
-- leaving a hole in the scrubber.
SELECT add_continuous_aggregate_policy('tracks_1m',
    start_offset      => INTERVAL '90 days',
    end_offset        => INTERVAL '2 minutes',
    schedule_interval => INTERVAL '1 minute');

SELECT add_retention_policy('tracks_1m', INTERVAL '90 days');

-- This is the spatial index that matters: historical viewport queries.
CREATE INDEX tracks_1m_position_idx ON tracks_1m USING GIST (position);
CREATE INDEX tracks_1m_entity_idx   ON tracks_1m (entity_kind, entity_key, bucket DESC);

-- ---------------------------------------------------------------------------
-- Daily summary — kept forever
-- ---------------------------------------------------------------------------

-- The permanent record. Bounds are stored as four doubles rather than a
-- PostGIS box because continuous aggregates only accept plain aggregate
-- functions, and min/max over ordinates is both supported and cheap.
--
-- Caveat, stated rather than hidden: these bounds are meaningless for anything
-- that crosses the antimeridian, where min/max lon spans the wrong way round
-- the planet. Consumers must treat a full -180..180 lon span as "unknown"
-- rather than as a real extent. Pattern-of-life queries use tracks_1m for
-- anything needing true geometry.
CREATE MATERIALIZED VIEW entity_daily
WITH (timescaledb.continuous) AS
SELECT
    time_bucket(INTERVAL '1 day', observed_at) AS day,
    entity_kind,
    entity_key,
    count(*)                              AS samples,
    min(observed_at)                      AS first_seen,
    max(observed_at)                      AS last_seen,
    min(ST_X(position))                   AS min_lon,
    max(ST_X(position))                   AS max_lon,
    min(ST_Y(position))                   AS min_lat,
    max(ST_Y(position))                   AS max_lat,
    min(alt_m)                            AS min_alt_m,
    max(alt_m)                            AS max_alt_m,
    max(speed_mps)                        AS max_speed_mps
FROM observations
GROUP BY day, entity_kind, entity_key
WITH NO DATA;

-- End offset of one hour keeps the current day's partial bucket out of the
-- materialised set until it is settled.
SELECT add_continuous_aggregate_policy('entity_daily',
    start_offset      => INTERVAL '7 days',
    end_offset        => INTERVAL '1 hour',
    schedule_interval => INTERVAL '1 hour');

-- No retention policy: this is the forever tier.

CREATE INDEX entity_daily_entity_idx ON entity_daily (entity_kind, entity_key, day DESC);
CREATE INDEX entity_daily_day_idx    ON entity_daily (day DESC);
