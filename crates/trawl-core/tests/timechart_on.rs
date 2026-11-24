// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `timechart on <column>`: what the buckets are cut from.

mod timechart_on {
    use duckdb::Connection;

    use trawl_core::context::EvalContext;
    use trawl_core::emitter::{self, EmittedQuery};
    use trawl_core::parser;

    fn emit(dsl: &str, source: &str) -> EmittedQuery {
        let query = parser::parse(dsl).expect("dsl parses");
        let anchor = EvalContext::at(
            "2026-01-01T12:00:00Z"
                .parse::<chrono::DateTime<chrono::Utc>>()
                .expect("anchor parses"),
        );
        emitter::emit(&query, source, anchor).expect("emit succeeds")
    }

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

    fn bucket_interval(sql: &str) -> &str {
        let after = sql
            .split_once("time_bucket(INTERVAL '")
            .expect("the emission buckets")
            .1;
        after.split_once('\'').expect("the literal closes").0
    }

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

        // `DuckDB` folds an identifier over ASCII, so every spelling of `_time`
        // is that same column.
        for spelling in ["_time", "_TIME", "_Time"] {
            let explicit = emit(
                &format!("* | timechart on {spelling} span=5m count()"),
                "/data/**/*.parquet",
            );
            assert_eq!(explicit.sql, BEFORE, "on {spelling}");
            assert!(
                explicit.timechart_input_checks.is_empty(),
                "on {spelling} probes nothing"
            );
        }
    }
}
