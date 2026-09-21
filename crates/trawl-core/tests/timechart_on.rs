// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `timechart on <column>`: what the buckets are cut from.
//!
//! The stage's default — and an explicit `on _time` — buckets the
//! envelope timestamp through its `TRY_CAST` (ADR-0008). An explicit
//! column is bucketed as stored: no cast, so a column that is not a
//! timestamp is refused by `DuckDB`'s binder instead of coercing to NULL
//! and collapsing every row into one empty bucket.
//!
//! Every assertion here runs the emitted SQL through the bundled
//! `DuckDB`, because the claim under test is about what `DuckDB` does
//! with the emitted text, not about the text alone.

mod timechart_on {
    use duckdb::Connection;

    use trawl_core::context::EvalContext;
    use trawl_core::emitter::{self, EmittedQuery};
    use trawl_core::parser;

    /// Emit `dsl` against `source` with a fixed anchor.
    ///
    /// Fixed rather than captured so a `last=` window in a test query
    /// means one interval, not one per emission.
    fn emit(dsl: &str, source: &str) -> EmittedQuery {
        let query = parser::parse(dsl).expect("dsl parses");
        let anchor = EvalContext::at(
            "2026-01-01T12:00:00Z"
                .parse::<chrono::DateTime<chrono::Utc>>()
                .expect("anchor parses"),
        );
        emitter::emit(&query, source, anchor).expect("emit succeeds")
    }

    /// Write a parquet file from a `SELECT`, returning its path. The
    /// directory has to outlive the returned path, so the caller keeps it.
    fn write_parquet(dir: &tempfile::TempDir, name: &str, select: &str) -> String {
        let path = dir
            .path()
            .join(name)
            .to_str()
            .expect("temp path is valid UTF-8")
            .to_string();
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        conn.execute_batch(&format!("COPY ({select}) TO '{path}' (FORMAT PARQUET)"))
            .expect("parquet write succeeds");
        path
    }

    /// The `INTERVAL '…'` literal inside the emitted bucket expression.
    fn bucket_interval(sql: &str) -> &str {
        let after = sql
            .split_once("time_bucket(INTERVAL '")
            .expect("the emission buckets")
            .1;
        after.split_once('\'').expect("the literal closes").0
    }

    /// A relation whose only timestamp is `_run_time` — the shape
    /// `from saved <name> run=all` produces — buckets on that column, and
    /// the buckets collect every row that falls in them.
    #[test]
    fn buckets_run_time_across_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = write_parquet(
            &dir,
            "runs.parquet",
            "SELECT * FROM (VALUES \
             (TIMESTAMP '2026-01-01 00:05:00', 1.0), \
             (TIMESTAMP '2026-01-01 00:20:00', 2.0), \
             (TIMESTAMP '2026-01-01 00:59:59', 3.0), \
             (TIMESTAMP '2026-01-01 01:10:00', 10.0), \
             (TIMESTAMP '2026-01-01 01:30:00', 20.0)) \
             AS t(\"_run_time\", \"x\")",
        );

        let emitted = emit("* | timechart on _run_time span=1h sum(x)", &source);
        assert!(
            emitted.params.is_empty(),
            "no parameters to bind: {:?}",
            emitted.params
        );

        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        let mut stmt = conn
            .prepare(&format!(
                "SELECT strftime(\"_time\", '%Y-%m-%d %H:%M:%S') AS b, \
                 * EXCLUDE (\"_time\") FROM ({}) ORDER BY b",
                emitted.sql
            ))
            .expect("the emitted SQL binds");
        let rows: Vec<(String, f64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("the emitted SQL runs")
            .collect::<Result<_, _>>()
            .expect("rows read");

        assert_eq!(
            rows,
            vec![
                ("2026-01-01 00:00:00".to_string(), 6.0),
                ("2026-01-01 01:00:00".to_string(), 30.0),
            ],
            "one bucket per hour of _run_time, summing every row in it"
        );
    }

    /// Omitting `span=` derives the bucket from the `last=` window exactly
    /// as it does without `on`: the clause chooses the column, never the
    /// interval.
    #[test]
    fn default_span_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = write_parquet(
            &dir,
            "spans.parquet",
            "SELECT TIMESTAMP '2026-01-01 00:05:00' AS \"_time\", \
             TIMESTAMP '2026-01-01 00:05:00' AS \"_run_time\", 1.0 AS \"x\"",
        );

        for window in ["", "last=6h ", "last=7d "] {
            let plain = emit(&format!("{window}* | timechart sum(x)"), &source);
            let named = emit(
                &format!("{window}* | timechart on _run_time sum(x)"),
                &source,
            );
            assert_eq!(
                bucket_interval(&named.sql),
                bucket_interval(&plain.sql),
                "window {window:?} must derive one interval for both lanes"
            );

            // And it is SQL `DuckDB` runs, not just matching text.
            let conn = Connection::open_in_memory().expect("in-memory duckdb");
            conn.prepare(&named.sql).expect("the emitted SQL binds");
        }

        let derived = emit("* | timechart on _run_time sum(x)", &source);
        assert_eq!(
            bucket_interval(&derived.sql),
            "1 minutes",
            "no last= window still means the one-minute default"
        );
    }

    /// A column that is not a timestamp is bucketed as stored, and the
    /// emission carries a probe that reads that column, with that
    /// stage's own prefix in force, so the executor can say which
    /// column and which type
    /// (`executor::timechart_input_refuses_each_non_timestamp` in
    /// trawl-engine asserts the 400 the probe composes).
    #[test]
    fn refuses_varchar_naming_type() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = write_parquet(
            &dir,
            "hosts.parquet",
            "SELECT 'web-1' AS \"hostname\", TIMESTAMP '2026-01-01 00:05:00' AS \"_time\"",
        );

        let emitted = emit("* | timechart on hostname count()", &source);
        assert!(
            emitted
                .sql
                .contains("time_bucket(INTERVAL '1 minutes', \"hostname\")"),
            "the named column is bucketed as stored: {}",
            emitted.sql
        );
        assert!(
            !emitted.sql.contains("TRY_CAST(\"hostname\""),
            "no cast may rescue a non-timestamp column: {}",
            emitted.sql
        );

        let [check] = emitted.timechart_input_checks.as_slice() else {
            panic!(
                "one timechart, one probe: {:?}",
                emitted.timechart_input_checks
            );
        };
        assert_eq!(check.column, "hostname");
        assert!(
            check.sql.starts_with("SELECT \"hostname\" FROM (") && check.sql.ends_with("LIMIT 0"),
            "the probe reads the named column and no rows: {}",
            check.sql
        );
        assert!(
            check.sql.contains(&source),
            "and reads it from this stage's own relation: {}",
            check.sql
        );
        assert!(
            !check.sql.contains("time_bucket("),
            "the probe is the stage's input, not the stage: {}",
            check.sql
        );

        // And it is a statement DuckDB answers, with the type the
        // executor refuses on.
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        let mut stmt = conn.prepare(&check.sql).expect("the probe binds");
        assert!(check.params.is_empty(), "no parameters in this prefix");
        let rows = stmt.query([]).expect("the probe runs");
        let bound = rows.as_ref().expect("statement outlives the rows");
        assert_eq!(
            bound.column_logical_type(0).id(),
            duckdb::core::LogicalTypeId::Varchar,
            "the probe reports the stored type"
        );
    }

    /// The emission for a `timechart` without `on` is byte-for-byte what
    /// it was before the clause existed — the text below is the committed
    /// `pipe_timechart_explicit_span` snapshot at the parent commit.
    #[test]
    fn plain_timechart_unchanged() {
        const BEFORE: &str = "SELECT time_bucket(INTERVAL '5 minutes', TRY_CAST(\"_time\" AS TIMESTAMP)) AS \"_time\", COUNT(*) AS \"count\"\n\
             FROM read_parquet('/data/**/*.parquet', union_by_name=true)\n\
             GROUP BY time_bucket(INTERVAL '5 minutes', TRY_CAST(\"_time\" AS TIMESTAMP))\n\
             ORDER BY time_bucket(INTERVAL '5 minutes', TRY_CAST(\"_time\" AS TIMESTAMP)) ASC";

        let emitted = emit("* | timechart span=5m count()", "/data/**/*.parquet");
        assert_eq!(emitted.sql, BEFORE);
        assert!(
            emitted.timechart_input_checks.is_empty(),
            "an omitted clause probes nothing"
        );

        // `on _time` is the same bucket source spelled out, so it emits
        // the same SQL — and still names no column, because `_time` is
        // the default the executor needs no help with.
        let explicit = emit(
            "* | timechart on _time span=5m count()",
            "/data/**/*.parquet",
        );
        assert_eq!(explicit.sql, BEFORE);
        assert!(explicit.timechart_input_checks.is_empty());
    }
}
