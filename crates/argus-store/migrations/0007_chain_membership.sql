-- Tell a failover chain apart from the providers inside it.
--
-- Both have been rows in `sources` since chains were introduced, and nothing
-- recorded the relationship between them. Two consequences, both visible:
--
--   `/v1/sources` returned "Aircraft (adsb.lol)" twice — once as the chain and
--   once as the provider currently serving it — with different observation
--   counts, so the Android sources sheet read as though there were two feeds.
--
--   `/v1/layers` summed observations across every row in a layer, which counts
--   the chain's total *and* each member's contribution to it. The flights layer
--   reported 327,601 observations where the chain alone had 321,983.
--
-- `member_of` is NULL for anything that stands on its own — a plain source, or
-- a chain — and names the chain for a provider inside one. That is the whole
-- distinction: a row with a parent is not a feed in its own right, it is one
-- way the parent gets its data.
--
-- ON DELETE SET NULL rather than CASCADE: if a chain is ever removed, its
-- providers' health history is still true and worth keeping.

ALTER TABLE sources
    ADD COLUMN member_of text REFERENCES sources (source_id) ON DELETE SET NULL;

-- The common query is "top-level sources" and "the members of this chain".
CREATE INDEX sources_member_of_idx ON sources (member_of) WHERE member_of IS NOT NULL;

-- Backfill for the rows that already exist. A chain registers itself under its
-- own layer id, which is what made `source_id = layer_id` a usable heuristic in
-- the first place; using it once, here, is fine — from now on the ingest
-- runtime states the relationship explicitly rather than implying it.
UPDATE sources m
SET member_of = c.source_id
FROM sources c
WHERE c.source_id = c.layer_id
  AND m.layer_id = c.layer_id
  AND m.source_id <> c.source_id;
