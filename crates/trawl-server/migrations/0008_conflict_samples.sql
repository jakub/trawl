-- Misfit sample values on the conflict evidence (ADR-0011 slice C1, issue
-- #69): WHICH values a pin is costing an operator, not just how many.
--
-- Captured by compaction at null-time (`ConformPlan::tally_conflicts`) — the
-- only moment the misfit value is in hand; anything later means re-scanning
-- `_raw`. Written in the same transaction as the row it annotates.
--
-- Every constraint is NAMED, per the 0001 convention.

-- TEXT[], not JSONB: the payload is opaque client text with no structure
-- worth querying, every catalog write in this store already binds
-- `Vec<&str>` through `UNNEST($n::text[])`, and JSONB would invite the
-- analyzer to depend on a shape the capture does not promise.
--
-- Cardinality (at most `MAX_CONFLICT_SAMPLES` distinct values) and per-value
-- byte length (`MAX_CONFLICT_SAMPLE_BYTES`, truncated on a char boundary and
-- control-sanitised) are enforced by the WRITER, deliberately without a
-- CHECK constraint: a violation here would wedge compaction's bookkeeping
-- forever on a condition no retry can clear, exactly as the unstorable-name
-- filter in `store::catalog` reasons.
ALTER TABLE field_conflicts
    ADD COLUMN samples TEXT[] NOT NULL DEFAULT '{}';
