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
