-- Staged per-service null tallies on the repin job row (issue #137): the
-- evidence a completing cutover materialises into `field_conflicts` inside
-- the very transaction that flips the pin, so a job that reads `succeeded`
-- has already had its evidence written.
--
-- The tallies are the rewrite's own per-service counts, staged by
-- `RepinStore::stage_cutover_input` after the final excluded pass and
-- BEFORE the force gate and the `data/REPIN` Cutover marker. The staging
-- commit is what makes the marker meaningful: past that marker the engine
-- is forward-only, so the evidence has to be durable before it, not after.
--
-- NULL vs empty is the whole contract:
--   both columns NULL  = staging never ran. A legacy row, a job killed
--                        before the staging commit, or any job that never
--                        reached the cutover. A completing call reads this
--                        as "no evidence was ever staged" and completes
--                        forward with a warn.
--   both columns '{}'  = staging ran and found no nulled row anywhere. The
--                        repin was lossless, and that is a PROVEN fact, not
--                        an absence.
--
-- Hence NULLABLE with NO DEFAULT. A `DEFAULT '{}'` would stamp the
-- lossless-proof value on every row the moment the migration applies and on
-- every claim afterwards, so a job that crashed between the shadow build
-- and the staging commit would report, in the same shape, that it had
-- proven itself lossless. The distinction only survives if the unstaged
-- state has its own value.
--
-- Parallel arrays rather than JSONB: workspace sqlx is built without the
-- json feature, and 0012's `unmapped_samples TEXT[]` is the idiom. The
-- cardinality CHECK below is what answers the usual objection to a pair of
-- arrays. A row can never carry a service without its count.
--
-- Every constraint is NAMED, per the 0001 convention.

ALTER TABLE repin_jobs ADD COLUMN nulled_services     TEXT[];
ALTER TABLE repin_jobs ADD COLUMN nulled_service_rows BIGINT[];

-- The two columns are one fact, so they are present or absent together.
ALTER TABLE repin_jobs ADD CONSTRAINT repin_jobs_nulled_tallies_paired_check
    CHECK ((nulled_services IS NULL) = (nulled_service_rows IS NULL));

-- Element i of one column belongs to element i of the other. `cardinality`
-- rather than `array_length`, which answers NULL for an empty array and
-- would let '{}' pair with a populated column.
ALTER TABLE repin_jobs ADD CONSTRAINT repin_jobs_nulled_tallies_cardinality_check
    CHECK (cardinality(nulled_services) = cardinality(nulled_service_rows));

-- A dry run never writes a shadow and never cuts over, so it has no
-- rewrite tallies to stage. Anything staged on a dry-run row would be a
-- projection wearing the outcome's clothes.
ALTER TABLE repin_jobs ADD CONSTRAINT repin_jobs_nulled_tallies_scope_check
    CHECK (nulled_services IS NULL OR NOT dry_run);
