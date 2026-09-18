SET plan_cache_mode = 'auto';
-- SQL from HistoryStore::get_user_history and its shared history_query! predicate.
-- Parameter types correspond to SQLx's i64 / optional text / i64 / i64 binds.
PREPARE history_count(bigint, text) AS
SELECT COUNT(*) FROM query_history WHERE key_id = $1 AND ($2::text IS NULL OR strpos(lower(query), lower($2::text)) > 0);
PREPARE history_page(bigint, text, bigint, bigint) AS
SELECT id, key_id, query, executed_at, duration_ms, row_count, status
FROM query_history
WHERE key_id = $1 AND ($2::text IS NULL OR strpos(lower(query), lower($2::text)) > 0)
ORDER BY executed_at DESC, id DESC
LIMIT $3 OFFSET $4;

BEGIN;
SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY;
SHOW transaction_isolation;
SHOW transaction_read_only;
SELECT pg_backend_pid();
COMMIT;
BEGIN;
SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY;
EXECUTE history_count(19201, 'marker_absent_192');
EXECUTE history_page(19201, 'marker_absent_192', 50, 0);
COMMIT;
BEGIN;
SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY;
EXECUTE history_count(19201, 'marker_absent_192');
EXECUTE history_page(19201, 'marker_absent_192', 50, 0);
COMMIT;
BEGIN;
SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY;
EXECUTE history_count(19201, 'marker_absent_192');
EXECUTE history_page(19201, 'marker_absent_192', 50, 0);
COMMIT;
BEGIN;
SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY;
EXECUTE history_count(19201, 'marker_absent_192');
EXECUTE history_page(19201, 'marker_absent_192', 50, 0);
COMMIT;
BEGIN;
SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY;
EXECUTE history_count(19201, 'marker_absent_192');
EXECUTE history_page(19201, 'marker_absent_192', 50, 0);
COMMIT;
BEGIN;
SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY;
EXECUTE history_count(19201, 'marker_absent_192');
EXECUTE history_page(19201, 'marker_absent_192', 50, 0);
COMMIT;
\timing on
\echo SAMPLE 1
BEGIN;
SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY;
EXECUTE history_count(19201, 'marker_absent_192');
EXECUTE history_page(19201, 'marker_absent_192', 50, 0);
COMMIT;
\echo SAMPLE 2
BEGIN;
SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY;
EXECUTE history_count(19201, 'marker_absent_192');
EXECUTE history_page(19201, 'marker_absent_192', 50, 0);
COMMIT;
\echo SAMPLE 3
BEGIN;
SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY;
EXECUTE history_count(19201, 'marker_absent_192');
EXECUTE history_page(19201, 'marker_absent_192', 50, 0);
COMMIT;
\echo SAMPLE 4
BEGIN;
SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY;
EXECUTE history_count(19201, 'marker_absent_192');
EXECUTE history_page(19201, 'marker_absent_192', 50, 0);
COMMIT;
\echo SAMPLE 5
BEGIN;
SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY;
EXECUTE history_count(19201, 'marker_absent_192');
EXECUTE history_page(19201, 'marker_absent_192', 50, 0);
COMMIT;
\timing off
\echo END_SAMPLES
SELECT name, generic_plans, custom_plans FROM pg_prepared_statements ORDER BY name;
BEGIN;
SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY;
EXPLAIN (ANALYZE, BUFFERS) EXECUTE history_count(19201, 'marker_absent_192');
EXPLAIN (ANALYZE, BUFFERS) EXECUTE history_page(19201, 'marker_absent_192', 50, 0);
COMMIT;
SELECT name, generic_plans, custom_plans FROM pg_prepared_statements ORDER BY name;
