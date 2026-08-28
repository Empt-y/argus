-- Argus core schema: sources, live entity state, static features, and the
-- control tables for geofencing, alerting and device pairing.
--
-- The time-series side (the hypertable, the rollup ladder, retention) lives in
-- 0002 so that this file can be read on its own as "the shape of the data".
--
-- A note on types used throughout: enumerated values are stored as `text` with
-- CHECK constraints rather than PostgreSQL ENUMs. On the compressed hypertable
-- Timescale dictionary-encodes repeated short strings down to near nothing, so
-- the space argument for ENUMs largely evaporates, and text avoids `ALTER TYPE`
-- migrations every time a new source or quality state is added. Adding a layer
-- should not require a schema change.

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS timescaledb;

-- ---------------------------------------------------------------------------
-- Shared value domains
-- ---------------------------------------------------------------------------

-- Kept in step with argus_core::entity::EntityKind.
CREATE DOMAIN entity_kind AS text
    CHECK (VALUE IN ('aircraft', 'vessel', 'satellite', 'event', 'station', 'feature', 'measure'));

-- Kept in step with argus_core::entity::Quality.
CREATE DOMAIN quality AS text
    CHECK (VALUE IN ('live', 'delayed', 'modeled', 'estimated', 'stale'));

-- Kept in step with argus_core::entity::AltitudeDatum.
--
-- This column is not decoration. Mixing pressure altitude with geometric height
-- is what buries aircraft under terrain, so altitude is never stored without the
-- datum it was measured against. Note also that altitude is deliberately NOT
-- packed into the geometry's Z ordinate: the datum varies per row, and a bare Z
-- would silently claim they were all comparable.
CREATE DOMAIN alt_datum AS text
    CHECK (VALUE IN ('wgs84_ellipsoid', 'geoid', 'above_ground', 'barometric'));

-- ---------------------------------------------------------------------------
-- Source registry and health
-- ---------------------------------------------------------------------------

CREATE TABLE sources (
    source_id       text PRIMARY KEY,
    layer_id        text        NOT NULL,
    display_name    text        NOT NULL,
    entity_kind     entity_kind NOT NULL,
    cost_class      text        NOT NULL CHECK (cost_class IN ('free', 'metered', 'local')),

    -- Health, rewritten after every poll. `state` mirrors
    -- argus_core::source::SourceHealth; 'unknown' is the startup state and is
    -- deliberately distinct from a successful poll that returned zero rows.
    state           text        NOT NULL DEFAULT 'unknown'
                        CHECK (state IN ('live', 'delayed', 'stale', 'degraded',
                                         'key_required', 'hardware_absent',
                                         'unknown', 'failed')),
    state_since     timestamptz NOT NULL DEFAULT now(),
    last_success    timestamptz,
    last_error      text,
    last_lag_ms     integer,
    observations    bigint      NOT NULL DEFAULT 0,

    -- Budget accounting for metered sources, reset by the scheduler on the
    -- provider's own cycle boundary.
    budget_used     integer     NOT NULL DEFAULT 0,
    budget_limit    integer,
    budget_reset_at timestamptz,

    -- Verbatim licence/credit text, surfaced in both clients' attribution panel.
    attribution     jsonb       NOT NULL DEFAULT '{}'::jsonb,
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX sources_layer_idx ON sources (layer_id);

-- ---------------------------------------------------------------------------
-- Live entity state
-- ---------------------------------------------------------------------------

-- One row per tracked thing, upserted on every observation. This is the hot
-- read path: "what is in this box right now" answers from here, never from the
-- hypertable.
CREATE TABLE entities (
    entity_kind entity_kind NOT NULL,
    entity_key  text        NOT NULL,
    source_id   text        NOT NULL REFERENCES sources (source_id) ON DELETE CASCADE,
    layer_id    text        NOT NULL,

    observed_at timestamptz NOT NULL,
    first_seen  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),

    position    geometry(Point, 4326),
    alt_m       double precision,
    alt_datum   alt_datum,

    course_deg  real,
    heading_deg real,
    speed_mps   real,
    vrate_mps   real,

    quality     quality NOT NULL,
    label       text,
    attrs       jsonb   NOT NULL DEFAULT '{}'::jsonb,

    PRIMARY KEY (entity_kind, entity_key)
);

-- The live viewport query: bbox intersect, filtered by layer.
CREATE INDEX entities_position_idx ON entities USING GIST (position);
CREATE INDEX entities_layer_observed_idx ON entities (layer_id, observed_at DESC);
-- Sweeping entities that have gone quiet.
CREATE INDEX entities_observed_idx ON entities (observed_at);

-- ---------------------------------------------------------------------------
-- Static and slow-moving geography
-- ---------------------------------------------------------------------------

-- Submarine cables, dams, datacentres, transmission lines, boundaries. These
-- are versioned by ingest run rather than time-series recorded: re-writing a
-- cable route every poll would be pure noise, which is why EntityKind::Feature
-- reports `is_timeseries() == false`.
CREATE TABLE features (
    feature_id  bigserial PRIMARY KEY,
    layer_id    text NOT NULL,
    source_id   text NOT NULL REFERENCES sources (source_id) ON DELETE CASCADE,
    feature_key text NOT NULL,

    geom        geometry(Geometry, 4326) NOT NULL,
    label       text,
    attrs       jsonb NOT NULL DEFAULT '{}'::jsonb,

    valid_from  timestamptz NOT NULL DEFAULT now(),
    -- NULL means "current". Superseding a feature closes the old row rather
    -- than deleting it, so a DVR query at a past instant still resolves the
    -- geography that was live then.
    valid_to    timestamptz,

    UNIQUE (layer_id, feature_key, valid_from)
);

CREATE INDEX features_geom_idx ON features USING GIST (geom);
CREATE INDEX features_layer_current_idx ON features (layer_id) WHERE valid_to IS NULL;

-- ---------------------------------------------------------------------------
-- Areas of interest
-- ---------------------------------------------------------------------------

-- Capture is global at a low cadence and full-rate only inside these. Without
-- that gate, global aircraft alone is ~57M rows/day and the disk budget is gone
-- in a week.
CREATE TABLE areas_of_interest (
    aoi_id     bigserial PRIMARY KEY,
    name       text NOT NULL UNIQUE,
    geom       geometry(Polygon, 4326) NOT NULL,
    enabled    boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX aoi_geom_idx ON areas_of_interest USING GIST (geom);

-- ---------------------------------------------------------------------------
-- Geofences and alerts
-- ---------------------------------------------------------------------------

CREATE TABLE geofences (
    geofence_id bigserial PRIMARY KEY,
    name        text NOT NULL,
    geom        geometry(Polygon, 4326) NOT NULL,
    -- Rule shape is owned by argus-alert; kept as JSON so adding a predicate
    -- does not require a migration.
    rule        jsonb NOT NULL DEFAULT '{}'::jsonb,
    enabled     boolean NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX geofences_geom_idx ON geofences USING GIST (geom) WHERE enabled;

CREATE TABLE alerts (
    alert_id    bigserial PRIMARY KEY,
    geofence_id bigint REFERENCES geofences (geofence_id) ON DELETE SET NULL,
    entity_kind entity_kind NOT NULL,
    entity_key  text NOT NULL,
    fired_at    timestamptz NOT NULL DEFAULT now(),
    severity    text NOT NULL DEFAULT 'info'
                    CHECK (severity IN ('info', 'notice', 'warning', 'critical')),
    message     text NOT NULL,
    position    geometry(Point, 4326),
    attrs       jsonb NOT NULL DEFAULT '{}'::jsonb,

    acknowledged_at timestamptz,
    -- Delivery is tracked per device so an alert raised while the phone was off
    -- the network can be replayed on reconnect rather than lost.
    delivered_to    jsonb NOT NULL DEFAULT '[]'::jsonb
);

CREATE INDEX alerts_fired_idx ON alerts (fired_at DESC);
CREATE INDEX alerts_unacked_idx ON alerts (fired_at DESC) WHERE acknowledged_at IS NULL;

-- ---------------------------------------------------------------------------
-- Paired clients
-- ---------------------------------------------------------------------------

CREATE TABLE devices (
    device_id    uuid PRIMARY KEY,
    name         text NOT NULL,
    -- Argon2 hash. The plaintext token is shown once, in the pairing QR code,
    -- and never stored.
    token_hash   text NOT NULL,
    scopes       text[] NOT NULL DEFAULT ARRAY['read'],
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz,
    revoked_at   timestamptz
);

CREATE INDEX devices_active_idx ON devices (device_id) WHERE revoked_at IS NULL;
