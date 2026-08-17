-- Producer profiles (ADR-0013 slice 2, #75): the declared envelope grows
-- from nine fields to ten. `_producer` records which door an event
-- entered through (`http` | `syslog` | `trawld`) so provenance is
-- queryable data instead of an untraceable config effect.
--
-- No epoch bump and no conformance re-arm: `_producer` is a NEW column on
-- a corpus that never held one, `UNION ALL BY NAME` tolerates a column
-- absent from older files, and every pre-#75 row simply reads
-- `_producer IS NULL`. Nothing on disk needs rewriting, so nothing here
-- touches `catalog_state`.

-- `_producer` is the name this slice CLAIMS. Before ADR-0013 sealed the
-- `_` prefix a sender could have pinned `_producer` from its own data;
-- leaving that pin standing would type the SERVER-STAMPED column by the
-- old sender's shape. The declared pin therefore REPLACES whatever was
-- there — pin permanence protects sender-owned names, and `_` is no
-- longer one — and the evidence under the old pin describes a column
-- that no longer exists, so it goes with it.
DELETE FROM field_conflicts WHERE field = '_producer';
DELETE FROM field_conflict_stats WHERE field = '_producer';
DELETE FROM field_services WHERE field = '_producer';

INSERT INTO field_types (field, duckdb_type, pinned_from)
VALUES ('_producer', 'VARCHAR', '_declared')
ON CONFLICT (field) DO UPDATE
    SET duckdb_type = 'VARCHAR',
        pinned_from = '_declared',
        pinned_at   = now();
