-- The namespace cutover (ADR-0013 slice 1, #60): the declared envelope
-- shrinks from ten fields to nine, and the derived severity slot moves
-- into trawl's `_` namespace as `_severity`, typed SEVERITY.
--
-- Storage is set aside wholesale at boot (epoch 2 → 3), so there is no
-- corpus to reconcile here — but the catalog is postgres state that
-- SURVIVES the epoch bump, and it must describe the new envelope.

-- SEVERITY joins the catalog vocabulary. It is a SEMANTIC type over the
-- physical BIGINT: `trawl_core::schema::CanonicalType::as_catalog` writes
-- this spelling, `as_duckdb` writes BIGINT, and nothing but the seed
-- below can ever install it (DESCRIBE never reports SEVERITY, so
-- inference cannot mint it and `repin --to severity` parses through the
-- physical door, which has no such spelling).
ALTER TABLE field_types DROP CONSTRAINT field_types_type_check;
ALTER TABLE field_types ADD CONSTRAINT field_types_type_check
    CHECK (duckdb_type IN ('BOOLEAN', 'BIGINT', 'DOUBLE', 'TIMESTAMP', 'VARCHAR', 'SEVERITY'));

-- Reshape the SEED, and only the seed. Pin permanence is standing
-- doctrine — a pin slot is spent permanently, and a live sender's own
-- `severity` field is exactly the ordinary sender data ADR-0013 says it
-- is — so the DELETE is scoped to the rows migration 0002 declared.
-- A sender-set `severity`/`severity_text` pin (pinned_from = a service
-- name) is left standing, and keeps meaning what it always did.
DELETE FROM field_conflicts
 WHERE field IN ('severity', 'severity_text')
   AND EXISTS (
       SELECT 1 FROM field_types t
        WHERE t.field = field_conflicts.field AND t.pinned_from = '_declared'
   );
DELETE FROM field_conflict_stats
 WHERE field IN ('severity', 'severity_text')
   AND EXISTS (
       SELECT 1 FROM field_types t
        WHERE t.field = field_conflict_stats.field AND t.pinned_from = '_declared'
   );
DELETE FROM field_services
 WHERE field IN ('severity', 'severity_text')
   AND EXISTS (
       SELECT 1 FROM field_types t
        WHERE t.field = field_services.field AND t.pinned_from = '_declared'
   );
DELETE FROM field_types
 WHERE field IN ('severity', 'severity_text') AND pinned_from = '_declared';

INSERT INTO field_types (field, duckdb_type, pinned_from)
VALUES ('_severity', 'SEVERITY', '_declared')
ON CONFLICT (field) DO NOTHING;

-- Re-arm the boot conformance pass: the data root is about to be set
-- aside for epoch 3, and the fresh root has to prove itself conformant
-- against THIS catalog before it carries the identity marker.
UPDATE catalog_state SET conformed_at = NULL;
