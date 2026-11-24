// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Advice never changes what a query returns (ADR-0068).

use trawl_core::advice::{AdviceKind, advise};
use trawl_core::parser;
use trawl_core::schema::FieldTypes;
use trawl_engine::executor::Executor;
use trawl_engine::value::Value;

fn source() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let path = dir.path().join("events.parquet");
    conn.execute_batch(&format!(
        r#"
        COPY (
            SELECT * FROM (VALUES
                (1, TIMESTAMP '2026-11-13 10:00:00', 'coredns', 'q', 'query 10.244.0.83 A NOERROR',            '{{"service":"coredns","message":"query 10.244.0.83 A NOERROR"}}'),
                (2, TIMESTAMP '2026-11-13 10:00:01', 'web',     'q', 'proxy 10.244.0.83 service=coredns ok',    NULL),
                (3, TIMESTAMP '2026-11-13 10:00:02', 'trawld',  'a', 'flush done',                              NULL),
                (4, TIMESTAMP '2026-11-13 10:00:03', 'trawld',  'c', 'event_type in a, b',                      NULL),
                (5, TIMESTAMP '2026-11-13 10:00:04', 'trawld',  'b', 'event_type in b only',                    NULL),
                (6, TIMESTAMP '2026-11-13 10:00:05', 'web',     'q', 'GET 10.244.0.83 200',                     NULL),
                (7, TIMESTAMP '2026-11-13 10:00:06', NULL,      'q', 'orphan 10.244.0.83',                      NULL)
            ) AS t(id, _time, service, event_type, message, _raw)
        ) TO '{path}' (FORMAT PARQUET);
        "#,
        path = path.display(),
    ))
    .unwrap();
    (dir, path.display().to_string())
}

fn ids(executor: &Executor, source: &str, dsl: &str) -> Vec<i64> {
    let query = format!("{dsl} | table id | sort id");
    let result = executor
        .run_query(&query, source, &FieldTypes::new(), usize::MAX)
        .unwrap_or_else(|error| panic!("{query:?} runs: {error}"));
    result
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Integer(id) => *id,
            other => panic!("{query:?}: id cell {other:?}"),
        })
        .collect()
}

#[test]
fn footgun_hints_do_not_change_results() {
    let (_dir, source) = source();
    let executor = Executor::new().unwrap();

    let survey: [(&str, AdviceKind, &[i64]); 2] = [
        (
            "\"10.244.0.83\" -service=coredns",
            AdviceKind::NegatedFieldShape,
            &[1, 6, 7],
        ),
        (
            "service=trawld event_type in (a, b)",
            AdviceKind::SearchStageIn,
            &[4],
        ),
    ];
    for (dsl, kind, expected) in survey {
        let before = ids(&executor, &source, dsl);
        assert_eq!(before, expected, "{dsl:?} before advice");

        let parsed = parser::parse(dsl).unwrap_or_else(|errors| panic!("{dsl:?}: {errors:?}"));
        let report = advise(&parsed);
        let kinds: Vec<_> = report.findings.iter().map(|f| f.kind).collect();
        assert_eq!(kinds, [kind], "{dsl:?}");
        assert_eq!(
            parsed,
            parser::parse(dsl).unwrap(),
            "{dsl:?}: advice leaves the tree"
        );

        let after = ids(&executor, &source, dsl);
        assert_eq!(after, before, "{dsl:?} after advice");
    }

    for (dsl, expected) in [
        ("\"10.244.0.83\" service!=coredns", &[2, 6, 7][..]),
        ("\"10.244.0.83\" NOT service=coredns", &[2, 6][..]),
        ("\"10.244.0.83\" NOT \"service=coredns\"", &[1, 6, 7][..]),
        ("service=trawld event_type=a,b", &[3, 5][..]),
        ("service=trawld event_type!=a,b", &[4][..]),
        (
            "service=trawld | where event_type in (\"a\", \"b\")",
            &[3, 5][..],
        ),
        (
            "service=trawld | where not event_type in (\"a\", \"b\")",
            &[4][..],
        ),
    ] {
        assert_eq!(ids(&executor, &source, dsl), expected, "{dsl:?}");
        let parsed = parser::parse(dsl).unwrap_or_else(|errors| panic!("{dsl:?}: {errors:?}"));
        assert!(advise(&parsed).findings.is_empty(), "{dsl:?}");
    }
}
