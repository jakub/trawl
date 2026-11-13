// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The kv batch tail runs its stages in the order the query writes them
//! (ADR-0064), against real `DuckDB`.
//!
//! Every query fixes the order of the tail's input with a SQL-prefix
//! `sort seq` before `extract kv`, and every expectation names rows by
//! `seq` or by a group key, never by scan order. The fixture files are
//! written out of `seq` order, so a test that leaned on scan order would
//! read the wrong rows. Batch SQL is the oracle only for `sort | head` and
//! `stats | sort | limit`; every other expectation is computed by hand.

use std::io::Write as _;

use trawl_core::emitter::EmitError;
use trawl_core::schema::FieldTypes;
use trawl_engine::error::EngineError;
use trawl_engine::executor::Executor;
use trawl_engine::value::{QueryResult, Value};

/// The ndjson rows, written where `read_json` can reach them.
fn source(rows: &[String]) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.ndjson");
    let mut file = std::fs::File::create(&path).unwrap();
    for row in rows {
        writeln!(file, "{row}").unwrap();
    }
    file.flush().unwrap();
    let glob = path.display().to_string();
    (dir, glob)
}

fn try_run(dsl: &str, glob: &str) -> Result<QueryResult, EngineError> {
    Executor::new()
        .unwrap()
        .run_query(dsl, glob, &FieldTypes::new(), usize::MAX)
}

fn run(dsl: &str, glob: &str) -> QueryResult {
    try_run(dsl, glob).unwrap_or_else(|error| panic!("{dsl:?} must run: {error}"))
}

/// The 400-class sentence a refused tail answers with.
fn refusal(dsl: &str, glob: &str) -> String {
    match try_run(dsl, glob) {
        Err(EngineError::Emit(EmitError::UnsupportedOperation { message })) => message,
        other => panic!("{dsl:?} must be refused as an unsupported operation: {other:?}"),
    }
}

fn column(result: &QueryResult, name: &str) -> usize {
    result
        .columns
        .iter()
        .position(|column| column.name == name)
        .unwrap_or_else(|| panic!("missing {name}: {:?}", result.columns))
}

/// Every row's `name` cell, in answer order.
fn cells(result: &QueryResult, name: &str) -> Vec<Value> {
    let at = column(result, name);
    result.rows.iter().map(|row| row[at].clone()).collect()
}

/// Every row's `seq`, in answer order.
fn seqs(result: &QueryResult) -> Vec<i64> {
    cells(result, "seq")
        .into_iter()
        .map(|cell| match cell {
            Value::Integer(seq) => seq,
            other => panic!("seq must be an integer: {other:?}"),
        })
        .collect()
}

/// Rows `seq` 1..=n, each with message `k=v{seq}` and the given extra
/// JSON fields, written in a fixed scramble of `seq` order.
fn scrambled(fields: &[String]) -> Vec<String> {
    let rows: Vec<(String, String)> = fields
        .iter()
        .enumerate()
        .map(|(i, extra)| (format!("k=v{}", i + 1), extra.clone()))
        .collect();
    scrambled_messages(&rows)
}

/// Rows `seq` 1..=n, each a `(message, extra JSON fields)` pair, written
/// in a fixed scramble of `seq` order.
fn scrambled_messages(rows: &[(String, String)]) -> Vec<String> {
    let mut order: Vec<usize> = (0..rows.len()).collect();
    // A fixed scramble: odd positions from the back, then even ones.
    order.sort_by_key(|&i| (i % 2 == 0, std::cmp::Reverse(i)));
    order
        .into_iter()
        .map(|i| {
            let seq = i + 1;
            let (message, extra) = &rows[i];
            let sep = if extra.is_empty() { "" } else { "," };
            format!(r#"{{"seq":{seq},"message":"{message}"{sep}{extra}}}"#)
        })
        .collect()
}

/// `bytes` 10, 20 and 100 in `seq` order.
fn three_sizes() -> (tempfile::TempDir, String) {
    source(&scrambled(&[
        r#""bytes":10"#.to_owned(),
        r#""bytes":20"#.to_owned(),
        r#""bytes":100"#.to_owned(),
    ]))
}

/// Six rows: `bytes` 30, 10, 20, 60, 50, 40 and `grp` a, b, a, b, a, b in
/// `seq` order. The three largest are the last three rows.
fn six_sizes() -> (tempfile::TempDir, String) {
    let bytes = [30, 10, 20, 60, 50, 40];
    source(&scrambled(
        &bytes
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let grp = if i % 2 == 0 { "a" } else { "b" };
                format!(r#""bytes":{b},"grp":"{grp}""#)
            })
            .collect::<Vec<_>>(),
    ))
}

/// `(seq, bytes)` for each answer row, for comparing two answers whose
/// other columns differ.
fn seq_bytes(result: &QueryResult) -> Vec<(Value, Value)> {
    cells(result, "seq")
        .into_iter()
        .zip(cells(result, "bytes"))
        .collect()
}

#[test]
fn sort_then_head_takes_the_top() {
    let (_dir, glob) = three_sizes();

    let tail = run("* | sort seq | extract kv | sort -bytes | head 2", &glob);
    assert_eq!(
        cells(&tail, "bytes"),
        [Value::Integer(100), Value::Integer(20)],
        "the two largest rows, largest first"
    );
    assert_eq!(seqs(&tail), [3, 2]);

    let sql = run("* | sort seq | sort -bytes | head 2", &glob);
    assert_eq!(
        seq_bytes(&tail),
        seq_bytes(&sql),
        "the kv split changed the answer"
    );
}

#[test]
fn stats_sort_limit_is_top_n() {
    // Eight methods, `m1` once up to `m8` eight times: every count is
    // distinct, so the top five have one answer.
    let methods: Vec<String> = (1..=8)
        .flat_map(|m| std::iter::repeat_n(format!(r#""method":"m{m}""#), m))
        .collect();
    let (_dir, glob) = source(&scrambled(&methods));

    let tail = run(
        "* | sort seq | extract kv | stats count() by method | sort -count | limit 5",
        &glob,
    );
    let top: Vec<(Value, Value)> = (4..=8)
        .rev()
        .map(|m| (Value::String(format!("m{m}")), Value::Integer(m)))
        .collect();
    let answer = |result: &QueryResult| -> Vec<(Value, Value)> {
        cells(result, "method")
            .into_iter()
            .zip(cells(result, "count"))
            .collect()
    };
    assert_eq!(answer(&tail), top, "the five largest groups, largest first");

    let sql = run(
        "* | sort seq | stats count() by method | sort -count | limit 5",
        &glob,
    );
    assert_eq!(
        answer(&tail),
        answer(&sql),
        "the kv split changed the answer"
    );
}

#[test]
fn limit_then_sort_keeps_order() {
    let (_dir, glob) = six_sizes();

    let tail = run("* | sort seq | extract kv | head 3 | sort -bytes", &glob);
    assert_eq!(
        seqs(&tail),
        [1, 3, 2],
        "the first three rows (bytes 30, 10, 20), sorted; not the top three of all rows"
    );
}

#[test]
fn kv_sort_barrier_matrix() {
    let (_dir, glob) = six_sizes();
    let prefix = "* | sort seq | extract kv";

    // (stages after the prefix, the expected `seq`s, what the case shows)
    let cases: &[(&str, &[i64], &str)] = &[
        (
            "head 4 | sort -bytes",
            &[4, 1, 3, 2],
            "a limit before the sort picks the rows the sort orders",
        ),
        (
            "sort -bytes | head 4",
            &[4, 5, 6, 1],
            "a limit after the sort takes the top rows",
        ),
        (
            "where bytes > 15 | sort -bytes | head 2",
            &[4, 5],
            "a where before the sort narrows its input",
        ),
        (
            "sort -bytes | where bytes < 55 | head 2",
            &[5, 6],
            "a where after the sort filters sorted rows",
        ),
        (
            "drop bytes | sort -bytes | head 3",
            &[1, 2, 3],
            "a key dropped before the sort reads NULL for every row, so input order stays",
        ),
        (
            "sort -bytes | drop bytes | head 2",
            &[4, 5],
            "a key dropped after the sort ordered the rows first",
        ),
        (
            "rename bytes as size | sort -size | head 2",
            &[4, 5],
            "a renamed key sorts under its new name",
        ),
        (
            "rename bytes as size | sort -bytes | head 2",
            &[1, 2],
            "the old name binds nothing after a rename",
        ),
        (
            // 70, 90, 80, 40, 50, 60
            "let bytes = 100 - bytes | sort -bytes | head 2",
            &[2, 3],
            "a key overwritten by let sorts by its new value",
        ),
        (
            "sort -bytes | sort grp | head 4",
            &[5, 1, 3, 4],
            "the last sort is primary and the first breaks its ties",
        ),
        (
            "sort grp | sort -bytes | head 4",
            &[4, 5, 6, 1],
            "two sorts in the other order",
        ),
    ];
    for (stages, expected, shows) in cases {
        let dsl = format!("{prefix} | {stages}");
        assert_eq!(seqs(&run(&dsl, &glob)), *expected, "{dsl}: {shows}");
    }
}

#[test]
fn kv_barrier_protocols() {
    let (_dir, glob) = six_sizes();

    let none = run("* | sort seq | extract kv | head 0 | sort bytes", &glob);
    assert!(none.rows.is_empty(), "head 0 hands the sort no rows");

    // A source that matches no file answers with no columns; the tail is
    // still validated whole.
    let empty_dir = tempfile::tempdir().unwrap();
    let no_files = empty_dir.path().join("none-*.ndjson").display().to_string();
    assert!(
        run("* | extract kv | sort x", &no_files).columns.is_empty(),
        "a source with no file answers with no columns"
    );
    assert_eq!(
        refusal("* | extract kv | sort x | tail 5", &no_files),
        TAIL_REFUSAL,
        "a later refusal still fires over a result with no columns"
    );

    assert_eq!(
        refusal(
            "* | sort seq | extract kv | head 0 | sort bytes | tail 5",
            &glob
        ),
        TAIL_REFUSAL,
        "an empty earlier step never skips a later refusal"
    );

    // One more distinct `k` than the group cap.
    let groups = trawl_core::stream::AggregationCap::Groups.limit();
    let many: Vec<String> = (0..=groups).map(|_| String::new()).collect();
    let (_many_dir, many_glob) = source(&scrambled(&many));
    let capped = try_run(
        "* | sort seq | extract kv | stats count() by k | sort -count | head 1",
        &many_glob,
    );
    assert!(
        matches!(
            capped,
            Err(EngineError::AggregationCapped(
                trawl_core::stream::AggregationCap::Groups
            ))
        ),
        "a later head never hides an aggregation that dropped input: {:?}",
        capped.map(|result| result.rows.len())
    );
}

/// The sentence `tail` after `extract kv` is refused with.
const TAIL_REFUSAL: &str = "post-processing: tail is not supported after extract kv; \
                            use sort and head instead";

#[test]
fn kv_sort_binding_and_values() {
    let (_dir, glob) = six_sizes();
    let prefix = "* | sort seq | extract kv";

    assert_eq!(
        seqs(&run(&format!("{prefix} | sort -BYTES | head 2"), &glob)),
        [4, 5],
        "a key spelled BYTES binds the row's bytes"
    );

    assert_eq!(
        seqs(&run(&format!("{prefix} | sort grp"), &glob)),
        [1, 3, 5, 2, 4, 6],
        "equal keys keep input order ascending"
    );
    assert_eq!(
        seqs(&run(&format!("{prefix} | sort -grp"), &glob)),
        [2, 4, 6, 1, 3, 5],
        "equal keys keep input order descending"
    );

    // `n` is NULL in SQL for rows 2 and 5, and `ax` is a kv key that row
    // 4's `pairs` lacks.
    let (_nulls_dir, nulls) = source(&scrambled(&[
        r#""n":3,"pairs":"ax=3""#.to_owned(),
        r#""n":null,"pairs":"ax=1""#.to_owned(),
        r#""n":1,"pairs":"ax=2""#.to_owned(),
        r#""n":2,"pairs":"""#.to_owned(),
        r#""n":null,"pairs":"ax=5""#.to_owned(),
        r#""n":4,"pairs":"ax=4""#.to_owned(),
    ]));
    let nulls_prefix = "* | sort seq | extract kv from pairs";
    for (stages, expected, shows) in [
        ("sort n", [3, 4, 1, 6, 2, 5], "NULL sorts last ascending"),
        ("sort -n", [6, 1, 4, 3, 2, 5], "NULL sorts last descending"),
        (
            "sort ax",
            [2, 3, 1, 6, 5, 4],
            "an absent key sorts last ascending",
        ),
        (
            "sort -ax",
            [5, 6, 1, 3, 2, 4],
            "an absent key sorts last descending",
        ),
    ] {
        let dsl = format!("{nulls_prefix} | {stages}");
        assert_eq!(seqs(&run(&dsl, &nulls)), expected, "{dsl}: {shows}");
    }

    // NaN is the largest number, as DuckDB orders a DOUBLE, and NULL
    // still sorts after it both ways.
    let (_floats_dir, floats) = source(&scrambled(&[
        r#""f":1.5"#.to_owned(),
        r#""f":0.0"#.to_owned(),
        r#""f":-2.0"#.to_owned(),
        r#""f":null"#.to_owned(),
    ]));
    let nan_prefix = "* | sort seq | let v = if(seq == 2, 0.0 / 0, f) | extract kv";
    for (stages, expected) in [("sort v", [3, 1, 2, 4]), ("sort -v", [2, 1, 3, 4])] {
        let dsl = format!("{nan_prefix} | {stages}");
        assert_eq!(seqs(&run(&dsl, &floats)), expected, "{dsl}");
    }

    // `u64::MAX` and `u64::MAX - 1` are one DOUBLE; the sort keeps them
    // apart in both directions.
    let (_big_dir, big) = source(&scrambled(&[
        format!(r#""big":{}"#, u64::MAX),
        format!(r#""big":{}"#, u64::MAX - 1),
    ]));
    let big_prefix = "* | sort seq | extract kv";
    for (stages, expected) in [("sort big", [2, 1]), ("sort -big", [1, 2])] {
        let dsl = format!("{big_prefix} | {stages}");
        let sorted = run(&dsl, &big);
        assert_eq!(seqs(&sorted), expected, "{dsl}");
        let at = column(&sorted, "big");
        assert!(
            sorted
                .rows
                .iter()
                .all(|row| matches!(row[at], Value::UInt(_))),
            "{dsl}: the fixture must reach the tail as unsigned integers: {:?}",
            sorted.rows
        );
    }
}

#[test]
fn kv_stats_sort_stats_is_refused() {
    let (_dir, glob) = six_sizes();
    assert_eq!(
        refusal(
            "* | sort seq | extract kv | stats count() by grp | sort -count | stats count()",
            &glob
        ),
        "post-processing: multiple aggregations is not supported in streaming mode: \
         only one aggregation stage is supported in streaming mode",
        "a sort between two aggregations does not reset the one-aggregation rule"
    );
}

/// A scalar head in an aggregation slot is legal in the SQL prefix, where
/// it projects (ADR-0024). The tail compiles through the live lane's
/// accumulators, which have nothing to run for one, so it is refused
/// rather than counted (ADR-0001, Amendment 2026-11-08). The tail frames
/// the live sentence with `post-processing: `, so the reason is matched
/// inside it.
#[test]
fn kv_tail_refuses_scalar_heads() {
    let (_dir, glob) = three_sizes();
    for (dsl, sentence) in [
        (
            "* | extract kv | stats lower(x)",
            "stats is not supported in streaming mode: lower() is not an aggregate",
        ),
        (
            "* | sort seq | extract kv | timechart lower(k)",
            "timechart is not supported in streaming mode: lower() is not an aggregate",
        ),
        (
            "* | extract kv | stats coalesce(sum(bytes), 0)",
            "stats is not supported in streaming mode: coalesce() is not an aggregate",
        ),
    ] {
        let message = refusal(dsl, &glob);
        assert!(
            message.starts_with("post-processing: ") && message.contains(sentence),
            "{dsl}: {message}"
        );
    }
}

#[test]
fn kv_tail_is_refused() {
    let (_dir, glob) = six_sizes();
    assert_eq!(refusal("* | extract kv | tail 5", &glob), TAIL_REFUSAL);
}

#[test]
fn kv_dedup_remains_exact_past_live_capacity() {
    // 10,001 distinct keys, then the first key again: past the 10,000
    // keys the live stage holds.
    let distinct = 10_001;
    let rows: Vec<(String, String)> = (1..=distinct)
        .chain([1])
        .map(|key| (format!("k=key{key}"), String::new()))
        .collect();
    let (_dir, glob) = source(&scrambled_messages(&rows));

    let by_k = run("* | sort seq | extract kv | dedup k", &glob);
    let expected: Vec<i64> = (1..=10_001).collect();
    assert_eq!(
        seqs(&by_k),
        expected,
        "one row per key, the repeat of the first key dropped"
    );

    let bare = run("* | sort seq | extract kv | table k | dedup", &glob);
    assert_eq!(
        bare.rows.len(),
        distinct,
        "a bare dedup is exact past the live capacity too"
    );
}

#[test]
fn kv_dedup_keeps_newest_per_key() {
    // (key, _time, bytes) for seq 1..=11; `None` writes no `_time`.
    let rows: [(&str, Option<&str>, i64); 11] = [
        ("a", Some("10:00"), 5),
        ("b", Some("10:05"), 1),
        ("a", Some("10:10"), 2),
        ("c", None, 9),
        ("b", Some("10:01"), 7),
        ("c", Some("09:00"), 3),
        ("d", Some("10:20"), 4),
        ("d", Some("10:20"), 8),
        ("a", Some("10:10"), 6),
        ("e", Some("08:00"), 10),
        ("e", None, 11),
    ];
    let rows: Vec<(String, String)> = rows
        .iter()
        .map(|(key, time, bytes)| {
            let time = time.map_or_else(String::new, |hm| {
                format!(r#","_time":"2026-01-15T{hm}:00Z""#)
            });
            (format!("key={key}"), format!(r#""bytes":{bytes}{time}"#))
        })
        .collect();
    let (_dir, glob) = source(&scrambled_messages(&rows));

    // a: seq 3 is newer than 1 and ties 9, so the earlier 3 wins.
    // b: seq 2 is newer than 5. c: the timed seq 6 beats the untimed 4.
    // d: 7 and 8 tie, so 7. e: the timed 10 beats the untimed 11.
    // Winners come out in input order.
    assert_eq!(
        seqs(&run("* | sort seq | extract kv | dedup key", &glob)),
        [2, 3, 6, 7, 10]
    );

    // After the sort the step's input is bytes 11 down to 1: seqs 11, 10,
    // 4, 8, 5, 9, 1, 7, 6, 3, 2. a: 9 ties 3 and comes first. d: 8 ties 7
    // and comes first. The winners keep that sorted order.
    assert_eq!(
        seqs(&run(
            "* | sort seq | extract kv | sort -bytes | dedup key",
            &glob
        )),
        [10, 8, 9, 6, 2]
    );

    // Text `_time` the cast cannot read ranks after every readable time.
    let unreadable: Vec<(String, String)> = [
        ("a", "2026-01-15T10:00:00Z"),
        ("a", "soon"),
        ("b", "soon"),
        ("b", "2026-01-15T09:00:00Z"),
    ]
    .iter()
    .map(|(key, time)| (format!("key={key}"), format!(r#""_time":"{time}""#)))
    .collect();
    let (_text_dir, text) = source(&scrambled_messages(&unreadable));
    assert_eq!(
        seqs(&run("* | sort seq | extract kv | dedup key", &text)),
        [1, 4]
    );
}

/// The kv tail refuses a `sample` after `extract kv` with today's
/// result, the live lane's sentence framed by `post-processing: `. A
/// `sample` before `extract kv` stays in the SQL prefix, and one larger
/// than its input feeds every row's extracted values to the tail.
#[test]
fn sample_lane_boundaries() {
    let (_dir, glob) = three_sizes();
    assert_eq!(
        refusal("* | extract kv from message | sample 5", &glob),
        "post-processing: sample is not supported in streaming mode: \
         statistical sampling requires the full dataset",
    );

    let result = run(
        "* | sample 100 | extract kv from message | table seq, k",
        &glob,
    );
    let mut values: Vec<(Value, Value)> = cells(&result, "seq")
        .into_iter()
        .zip(cells(&result, "k"))
        .collect();
    values.sort_by_key(|(seq, _)| match seq {
        Value::Integer(seq) => *seq,
        other => panic!("seq must be an integer: {other:?}"),
    });
    assert_eq!(
        values,
        (1..=3)
            .map(|seq| (Value::Integer(seq), Value::String(format!("v{seq}"))))
            .collect::<Vec<_>>(),
    );
}
