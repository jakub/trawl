-- `repin --to severity` (issue #79): the operator surface for the pin
-- ADR-0013 declared but left unreachable.
--
-- RETRACTS migration 0010's header claim that "`repin --to severity` parses
-- through the physical door, which has no such spelling". Admission moved
-- to `CanonicalType::from_catalog` — the injective door — so SEVERITY is a
-- target an operator can NAME. What 0010 was really protecting still holds:
-- inference cannot mint the pin, because `normalize_duckdb_type` resolves
-- through the PHYSICAL spelling and `DESCRIBE` never reports SEVERITY. 0010
-- itself is immutable (an applied migration is history); this header is
-- where the correction lives.

-- BOTH type CHECKs widen, not just the target one: the first repin AWAY
-- from a severity pin (`level` was repinned to SEVERITY, the operator
-- reconsiders) claims a job whose FROM type is SEVERITY, and 0007's
-- from_type CHECK would refuse it.
ALTER TABLE repin_jobs DROP CONSTRAINT repin_jobs_from_type_check;
ALTER TABLE repin_jobs ADD CONSTRAINT repin_jobs_from_type_check
    CHECK (from_type IN ('BOOLEAN', 'BIGINT', 'DOUBLE', 'TIMESTAMP',
                         'VARCHAR', 'SEVERITY'));
ALTER TABLE repin_jobs DROP CONSTRAINT repin_jobs_to_type_check;
ALTER TABLE repin_jobs ADD CONSTRAINT repin_jobs_to_type_check
    CHECK (to_type IN ('BOOLEAN', 'BIGINT', 'DOUBLE', 'TIMESTAMP',
                       'VARCHAR', 'SEVERITY'));

-- The asserted dialect of the corpus's NUMERALS, for a SEVERITY target
-- only. NULLABLE and CHECKed to be present exactly when the target is
-- SEVERITY: a legacy job and a non-severity job report NULL rather than a
-- backfilled 'otel' they never asserted. The dialect is not a property of
-- the PIN — the catalog stores canonical OTel ladder positions whatever
-- wire dialect they were read from — so it lives on the job row that
-- asserted it and nowhere else (never on `field_types`, never on the
-- `data/REPIN` marker: no replay path re-runs a cast).
ALTER TABLE repin_jobs ADD COLUMN dialect TEXT;
ALTER TABLE repin_jobs ADD CONSTRAINT repin_jobs_dialect_check
    CHECK (dialect IN ('otel', 'syslog'));
ALTER TABLE repin_jobs ADD CONSTRAINT repin_jobs_dialect_scope_check
    CHECK ((dialect IS NOT NULL) = (to_type = 'SEVERITY'));

-- Ambiguity evidence: rows whose numeral reads as a DIFFERENT severity in
-- each dialect (the integers 1-7, where the OTel and syslog ladders
-- overlap). Counted on every scan, whatever the dialect — the count is the
-- report; the force GATE is what the dialect governs.
ALTER TABLE repin_jobs ADD COLUMN ambiguous_numerals BIGINT NOT NULL DEFAULT 0;

-- Up to five distinct sanitised samples of values the new pin cannot read
-- at all (`_raw` resurrection included) — the same capture rules as
-- `field_conflicts.samples` (ADR-0011 slice C1). The job row IS the
-- report, so the evidence lives with it.
ALTER TABLE repin_jobs ADD COLUMN unmapped_samples TEXT[] NOT NULL DEFAULT '{}';

-- Liveness at scan time: is anything still WRITING this field? A repin
-- translates history, and a field a live sender still feeds in the other
-- dialect will keep arriving in the ingest-time reading — the operator
-- needs `[ingest] severity_from`, not a repin. Facts only (the last
-- observation and one service that made it); the consumer writes the
-- words.
ALTER TABLE repin_jobs ADD COLUMN field_last_seen TIMESTAMPTZ;
ALTER TABLE repin_jobs ADD COLUMN field_last_service TEXT;
