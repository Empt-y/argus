-- A cache for geography that feeds point at rather than carry.
--
-- The case that forced it: 181 of 193 active NWS alerts carry no inline
-- polygon. They name the forecast and county zones they cover and expect the
-- consumer to resolve them. Argus was storing those alerts with their area
-- description and nothing to draw, so ~94% of active weather alerts existed in
-- the database and could not appear on a map.
--
-- Separate from `features` on purpose. That table is a versioned record of
-- geography Argus has *ingested as a layer* — cables, dams, datacentres — with
-- a source it belongs to and a validity window. This is a cache: keyed by the
-- upstream's own identifier, owned by nobody, and safe to delete in its
-- entirety at any time, at the cost of re-fetching. Conflating the two would
-- put cache eviction into a table that is meant to be a permanent record.
CREATE TABLE reference_geometry (
    cache_key   text PRIMARY KEY,
    geom        geometry(Geometry, 4326) NOT NULL,
    fetched_at  timestamptz NOT NULL DEFAULT now()
);

-- Deliberately no spatial index. Nothing ever asks this table "what is near
-- here" — every read is a primary-key lookup by the upstream's own zone id,
-- and a GiST index on a few thousand county polygons would be pure cost.

-- No retention policy either. A county boundary does not expire, and a zone
-- that stops being referenced costs a few kilobytes to keep. `fetched_at` is
-- recorded so a future refresh can find stale entries, not so anything expires
-- them today.
COMMENT ON TABLE reference_geometry IS
    'Cache of static geography referenced by id from upstream feeds (NWS zones, '
    'and whatever else needs it later). Safe to TRUNCATE; it refills on demand.';
