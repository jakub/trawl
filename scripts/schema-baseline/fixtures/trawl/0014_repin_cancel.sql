-- Operator-triggered repin cancellation (issue #109): the `cancelled`
-- terminal status and the two columns that record who asked and when.
--
-- RETRACTS migration 0007's status comment for `blocked`, which reads
-- "shadow retained for a retry". It never was: every pre-cutover stop,
-- `blocked` included, routes through `RepinEngine::abandon_build`, which
-- sweeps both staging roots and drops the `data/REPIN` marker once the
-- sweep succeeds. A retry re-scans and rebuilds from scratch. 0007 itself
-- is immutable history (sqlx checksums applied migrations), so the
-- correction lives here, the way 0012's header corrected 0010.
--
-- `cancelled` is LIVE-PROCESS-ONLY: it means a running job observed the
-- cancel token at a file boundary and its unwind actually ran. A daemon
-- that dies between the request and its effect leaves a `running` row that
-- boot reconciliation terminalizes as `failed`, request fields preserved —
-- recovery never infers `cancelled` from a populated `cancel_requested_at`,
-- because nothing observed the token.

ALTER TABLE repin_jobs DROP CONSTRAINT repin_jobs_status_check;
ALTER TABLE repin_jobs ADD CONSTRAINT repin_jobs_status_check
    CHECK (status IN ('running', 'succeeded', 'failed',
                      'refused_needs_force', 'blocked', 'cancelled'));

-- When the cancel was accepted in process, and the requesting key's display
-- name — the same identity source as `requested_by`, so one row's two
-- actors are named the same way. First writer wins: a repeat request over a
-- job already cancelling preserves both values (see
-- `RepinStore::record_cancel_request`), so the pair always names the asker
-- whose request took effect.
ALTER TABLE repin_jobs ADD COLUMN cancel_requested_at TIMESTAMPTZ;
ALTER TABLE repin_jobs ADD COLUMN cancelled_by TEXT;

-- The two columns are one fact, written in one statement: neither half can
-- stand without the other.
ALTER TABLE repin_jobs ADD CONSTRAINT repin_jobs_cancel_request_check
    CHECK ((cancel_requested_at IS NULL) = (cancelled_by IS NULL));

-- A `cancelled` row must name the request that cancelled it. The converse
-- is deliberately NOT constrained: a `failed` row with both fields
-- populated is the crash state above, and a `succeeded` one is the cancel
-- that lost its race with the point of no return.
ALTER TABLE repin_jobs ADD CONSTRAINT repin_jobs_cancelled_request_check
    CHECK (status <> 'cancelled' OR cancel_requested_at IS NOT NULL);
