-- A kind for things on roads and rails.
--
-- Buses were the first ground vehicles to arrive, and they are neither
-- aircraft nor vessels: they report a position and a bearing the way an
-- aircraft does, but they go stale in minutes rather than hours, and a
-- client draws them as a different shape. The domain is the closed list of
-- kinds the store accepts, so it grows here, in step with
-- argus_core::entity::EntityKind.
ALTER DOMAIN entity_kind DROP CONSTRAINT entity_kind_check;
ALTER DOMAIN entity_kind ADD CONSTRAINT entity_kind_check
    CHECK (VALUE IN ('aircraft', 'vessel', 'vehicle', 'satellite', 'event', 'station', 'feature', 'measure'));
