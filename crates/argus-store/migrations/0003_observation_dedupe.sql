-- Stop re-recording observations that have not changed.
--
-- Immutable events — earthquakes, fire detections, launches, conflict reports —
-- keep the same `observed_at` forever, but their feeds must still be re-polled
-- because the upstream revises magnitudes and locations for hours afterwards.
-- Without a guard, every poll rewrites the whole set: measured at a 5-minute
-- cadence, one USGS feed alone wrote ~74,000 identical rows a day.
--
-- Moving entities are unaffected. An aircraft's `observed_at` advances with
-- every transponder return, so its rows never collide.
--
-- `source_id` is part of the key on purpose: two feeds describing the same
-- aircraft at the same instant (OpenSky and a local dongle, say) are genuinely
-- two observations and both are kept for provenance. Only a *re-poll of the
-- same source* is suppressed.

-- The index this replaces was already carrying the entity-track query, so
-- making it unique costs nothing extra — it is the same btree with a
-- constraint attached, not a second index on the highest-write table.
DROP INDEX IF EXISTS observations_entity_idx;

-- Any deployment upgrading through this migration already has duplicates.
-- Keep the earliest ingest of each and drop the rest; they are byte-identical
-- apart from `ingested_at`, so which survives does not matter, but choosing
-- deterministically keeps the operation repeatable.
DELETE FROM observations o
USING (
    SELECT entity_kind, entity_key, observed_at, source_id,
           min(ingested_at) AS keep_ingested_at
    FROM observations
    GROUP BY entity_kind, entity_key, observed_at, source_id
    HAVING count(*) > 1
) dup
WHERE o.entity_kind = dup.entity_kind
  AND o.entity_key  = dup.entity_key
  AND o.observed_at = dup.observed_at
  AND o.source_id   = dup.source_id
  AND o.ingested_at > dup.keep_ingested_at;

-- TimescaleDB requires the partitioning column in any unique index; observed_at
-- is present, so this is accepted on the hypertable.
CREATE UNIQUE INDEX observations_entity_idx
    ON observations (entity_kind, entity_key, observed_at DESC, source_id);
