SELECT key_id, count(*) AS rows, min(octet_length(query)) AS min_bytes, max(octet_length(query)) AS max_bytes, count(*) FILTER (WHERE strpos(query, 'marker_common_192') > 0) AS common, count(*) FILTER (WHERE strpos(query, 'marker_sparse_192') > 0) AS sparse, count(*) FILTER (WHERE strpos(query, 'marker_absent_192') > 0) AS absent FROM query_history GROUP BY key_id ORDER BY key_id;
SELECT indexname, indexdef FROM pg_indexes WHERE tablename='query_history';
SELECT pg_relation_size('query_history') AS heap_bytes, pg_indexes_size('query_history') AS index_bytes;
