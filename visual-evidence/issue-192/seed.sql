-- Run only in the explicit, empty disposable database selected by measure.py.
-- No DROP/TRUNCATE: an existing query_history table makes this transaction fail.
BEGIN;
CREATE TABLE query_history (
    id BIGSERIAL PRIMARY KEY,
    key_id BIGINT NOT NULL,
    query TEXT NOT NULL,
    executed_at TIMESTAMPTZ NOT NULL,
    duration_ms BIGINT NOT NULL,
    row_count BIGINT NOT NULL,
    status TEXT NOT NULL
        CONSTRAINT query_history_status_check
        CHECK (status IN ('success', 'error', 'timeout'))
);
CREATE INDEX query_history_key_executed_idx
    ON query_history(key_id, executed_at DESC, id DESC);

-- Two interleaved keys, 100,000 rows each; pairs of sequence numbers share
-- timestamps. Every query exceeds 200 bytes and has deterministic varied text.
-- Per key: common = 50,000; sparse = 100; absent = 0; unfiltered = 100,000.
INSERT INTO query_history (key_id, query, executed_at, duration_ms, row_count, status)
SELECT key_id,
       'last=1h service=fixture-' || (n % 31)::text
       || ' message="row-' || n::text || ' key-' || key_id::text
       || CASE WHEN n % 2 = 0 THEN ' marker_common_192' ELSE '' END
       || CASE WHEN n % 1000 = 0 THEN ' marker_sparse_192' ELSE '' END
       || ' payload-' || md5(key_id::text || ':' || n::text)
       || md5(n::text || ':a') || md5(n::text || ':b')
       || md5(n::text || ':c') || md5(n::text || ':d')
       || md5(n::text || ':e') || md5(n::text || ':f') || '"',
       TIMESTAMPTZ '2026-01-01 00:00:00+00' + (n / 2) * INTERVAL '1 second',
       n % 1000, n % 10000, 'success'
FROM generate_series(1, 100000) AS series(n)
CROSS JOIN (VALUES (19201::bigint), (19202::bigint)) AS keys(key_id)
ORDER BY n, key_id;
COMMIT;
ANALYZE query_history;
