// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Whole-pipeline parity for the values that have no JSON spelling.
//!
//! Each case also states the answer, because two lanes can agree on the
//! wrong one.

use std::io::Write as _;

use duckdb::Connection;
use serde_json::{Value, json};
use trawl_core::emitter::{self, SqlValue};
use trawl_core::pin_scope::PinScope;
use trawl_core::row::{self, Row};
use trawl_core::schema::FieldTypes;
use trawl_core::stream::{StageResult, StreamPlan, compile_stream_plan};

fn apply_stage(
    stage: &mut trawl_core::stream::CompiledStage,
    event: &mut trawl_core::row::Row,
    ctx: &trawl_core::context::EvalContext,
) -> StageResult {
    trawl_core::stream::apply_stage(stage, event, ctx)
        .expect("a compiled plan prepares every pattern")
}

type TextRow = Vec<(String, String)>;

/// A lane's whole answer: rows sorted, so the comparison ignores order,
/// and a `Vec` rather than a set, so it does not ignore multiplicity.
type TextRows = Vec<TextRow>;

/// `_time` does not take part: the live lane carries the sender's wire text
/// while the batch lane renders the parsed instant (`2026-01-15T09:00:00Z` vs
/// `2026-01-15 09:00:00`).
fn compared(column: &str) -> bool {
    column != "_time"
}

fn bind_params(params: &[SqlValue]) -> Vec<Box<dyn duckdb::ToSql>> {
    params
        .iter()
        .map(|v| -> Box<dyn duckdb::ToSql> {
            match v {
                SqlValue::String(s) => Box::new(s.clone()),
                SqlValue::Int(i) => Box::new(*i),
                SqlValue::Float(f) => Box::new(*f),
                SqlValue::Bool(b) => Box::new(*b),
                SqlValue::Timestamp(at) => Box::new(duckdb::types::Value::Timestamp(
                    duckdb::types::TimeUnit::Microsecond,
                    at.and_utc().timestamp_micros(),
                )),
            }
        })
        .collect()
}

/// Run the pipeline as SQL and read every cell back as `DuckDB`'s own
/// VARCHAR rendering, the one form that can carry `inf`/`nan`, where
/// `to_json` would null them out and hide exactly what is under test.
fn batch_rows(conn: &Connection, dsl: &str, events: &[Value]) -> TextRows {
    batch_outcome(conn, dsl, events)
        .unwrap_or_else(|error| panic!("batch must run for {dsl:?}: {error}"))
}

fn batch_outcome(conn: &Connection, dsl: &str, events: &[Value]) -> Result<TextRows, String> {
    let query = trawl_core::parser::parse(dsl).expect("dsl parses");
    let mut tmp = tempfile::Builder::new()
        .suffix(".ndjson")
        .tempfile()
        .unwrap();
    for event in events {
        writeln!(tmp, "{event}").unwrap();
    }
    tmp.flush().unwrap();
    let source = tmp.path().to_str().unwrap().to_owned();
    let emitted = emitter::emit_with_pins(
        &query,
        &source,
        &FieldTypes::new(),
        &emitter::InputColumns::unprobed(),
        trawl_core::context::EvalContext::capture(),
    )
    .expect("emit succeeds");
    let params = bind_params(&emitted.params);
    let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();

    let names: Vec<String> = {
        let describe = format!("DESCRIBE ({})", emitted.sql);
        let mut stmt = conn.prepare(&describe).map_err(|error| error.to_string())?;
        stmt.query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<String>, _>>()
            .map_err(|error| error.to_string())?
    };
    let projection: Vec<String> = names
        .iter()
        .map(|name| {
            format!(
                r#"CAST("{0}" AS VARCHAR) AS "{0}""#,
                name.replace('"', "\"\"")
            )
        })
        .collect();
    let sql = format!(
        "SELECT {} FROM ({}) AS _sub",
        projection.join(", "),
        emitted.sql
    );

    let mut stmt = conn.prepare(&sql).map_err(|error| error.to_string())?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            let mut cells: TextRow = Vec::new();
            for (index, name) in names.iter().enumerate() {
                if !compared(name) {
                    continue;
                }
                let text: Option<String> = row.get(index)?;
                cells.push((name.clone(), text.unwrap_or_else(|| "null".to_owned())));
            }
            cells.sort();
            Ok(cells)
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<TextRows, _>>()
        .map_err(|error| error.to_string())?;
    Ok(sorted(rows))
}

fn live_rows(dsl: &str, events: &[Value]) -> TextRows {
    let anchor = trawl_core::context::EvalContext::capture();
    let query = trawl_core::parser::parse(dsl).expect("dsl parses");
    let plan =
        compile_stream_plan(&query.pipeline, &PinScope::unpinned(), None).expect("plan compiles");
    let rows: Vec<Row> = events
        .iter()
        .map(|event| row::from_json(event.as_object().unwrap()))
        .collect();

    let emitted: Vec<Row> = match plan {
        StreamPlan::PassThrough(mut stages) => rows
            .into_iter()
            .filter_map(|mut event| {
                stages
                    .iter_mut()
                    .all(|stage| apply_stage(stage, &mut event, &anchor) == StageResult::Pass)
                    .then_some(event)
            })
            .collect(),
        StreamPlan::Aggregate {
            mut pre_stages,
            mut aggregation,
            mut post_stages,
        } => {
            for mut event in rows {
                if pre_stages
                    .iter_mut()
                    .all(|stage| apply_stage(stage, &mut event, &anchor) == StageResult::Pass)
                {
                    aggregation.feed_event(&event, &anchor);
                }
            }
            aggregation
                .snapshot()
                .1
                .into_iter()
                .filter_map(|mut event| {
                    post_stages
                        .iter_mut()
                        .all(|stage| apply_stage(stage, &mut event, &anchor) == StageResult::Pass)
                        .then_some(event)
                })
                .collect()
        }
    };

    let rendered: TextRows = emitted
        .into_iter()
        .map(|event| {
            let mut cells: TextRow = event
                .iter()
                .filter(|(name, _)| compared(name))
                .map(|(name, cell)| (name.clone(), row::cell_text(cell)))
                .collect();
            cells.sort();
            cells
        })
        .collect();
    sorted(rendered)
}

fn sorted(mut rows: TextRows) -> TextRows {
    rows.sort();
    rows
}

/// The comparison is byte-exact, in every column. A normalization applied
/// here would be applied to every cell of every case, and a non-injective
/// one (folding `-nan` onto `nan`, say) would then launder a real VARCHAR
/// divergence, an ordinary string cell carrying `-nan` through a stage,
/// into agreement. Cases whose cell text is genuinely not comparable opt
/// in, by column, through [`agreed_folding_nan_in`].
fn agreed(conn: &Connection, dsl: &str, events: &[Value]) -> TextRows {
    assert_lanes_agree(conn, dsl, events, None)
}

/// The opt-in exists for exactly one shape: a cell holding a computed NaN
/// (`0.0 / 0`), whose sign is the hardware's rather than either engine's,
/// read by two different renderers. The live side goes through the
/// identity path, which ties the two signs deliberately so a group is one
/// group ([`row::cell_text`]); the batch side through `::VARCHAR`, which
/// prints the sign the value carries.
fn agreed_folding_nan_in(conn: &Connection, dsl: &str, events: &[Value], column: &str) -> TextRows {
    assert_lanes_agree(conn, dsl, events, Some(column))
}

/// Both lanes over a case where one column holds a nondeterministic
/// representative: which of several equal rows each lane kept, a choice
/// `dedup` and a group-by both make for themselves.
///
/// Deliberately not [`agreed_folding_nan_in`]: that one exists for a
/// value whose sign is the hardware's, and stretching it to cover a
/// representative choice would make its contract mean two things.
fn agreed_apart_from(
    conn: &Connection,
    dsl: &str,
    events: &[Value],
    representative: &str,
) -> (Vec<String>, Vec<String>) {
    let strip = |rows: &TextRows| -> TextRows {
        sorted(
            rows.iter()
                .map(|row| {
                    row.iter()
                        .filter(|(name, _)| name != representative)
                        .cloned()
                        .collect()
                })
                .collect(),
        )
    };
    let batch = batch_rows(conn, dsl, events);
    let live = live_rows(dsl, events);
    assert_eq!(
        strip(&live),
        strip(&batch),
        "lane divergence outside {representative:?}\ndsl: {dsl:?}\nlive: {live:?}\nbatch: {batch:?}"
    );
    (
        column(&live, representative),
        column(&batch, representative),
    )
}

fn assert_lanes_agree(
    conn: &Connection,
    dsl: &str,
    events: &[Value],
    fold_nan_in: Option<&str>,
) -> TextRows {
    let fold = |rows: TextRows| -> TextRows {
        let Some(target) = fold_nan_in else {
            return rows;
        };
        sorted(
            rows.into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|(name, text)| {
                            let text = if name == target && text == "-nan" {
                                "nan".to_owned()
                            } else {
                                text
                            };
                            (name, text)
                        })
                        .collect()
                })
                .collect(),
        )
    };
    let batch = fold(batch_rows(conn, dsl, events));
    let live = fold(live_rows(dsl, events));
    assert_eq!(
        live, batch,
        "lane divergence\ndsl: {dsl:?}\nlive: {live:?}\nbatch: {batch:?}"
    );
    batch
}

fn column(rows: &TextRows, name: &str) -> Vec<String> {
    let mut values: Vec<String> = rows
        .iter()
        .map(|row| {
            row.iter()
                .find(|(column, _)| column == name)
                .unwrap_or_else(|| panic!("missing column {name:?} in {row:?}"))
                .1
                .clone()
        })
        .collect();
    values.sort();
    values
}

fn conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .unwrap();
    conn
}

fn one_event() -> Vec<Value> {
    vec![json!({"service": "nginx", "n": 1})]
}

#[test]
fn a_computed_nan_survives_the_stage_boundary() {
    let conn = conn();
    let kept = agreed_folding_nan_in(
        &conn,
        "* | let x = 0.0 / 0 | where x == x",
        &one_event(),
        "x",
    );
    assert_eq!(kept.len(), 1, "the row must be kept: {kept:?}");
}

#[test]
fn an_infinity_compares_in_a_later_stage() {
    let conn = conn();
    let rows = agreed(
        &conn,
        "* | let x = 1.0 / 0 | let y = x > 0 | table y",
        &one_event(),
    );
    assert_eq!(column(&rows, "y"), vec!["true"]);

    // NaN outranks every finite value, so a comparison against a large
    // one is true. (Spelled as a plain decimal: the DSL float grammar has
    // no exponent form.)
    let rows = agreed(
        &conn,
        "* | let x = 0.0 / 0 | let y = x > 1000000.0 | table y",
        &one_event(),
    );
    assert_eq!(column(&rows, "y"), vec!["true"]);
}

#[test]
fn a_nan_group_key_is_one_group() {
    let conn = conn();
    let events = vec![
        json!({"service": "nginx", "n": 1}),
        json!({"service": "nginx", "n": 2}),
    ];
    let dsl = "* | let x = 0.0 / 0 | stats count() by x";
    let live = live_rows(dsl, &events);
    let batch = batch_rows(&conn, dsl, &events);

    assert_eq!(live.len(), 1, "one group live: {live:?}");
    assert_eq!(batch.len(), 1, "one group in batch: {batch:?}");
    assert_eq!(column(&live, "count"), column(&batch, "count"));
    assert_eq!(column(&live, "count"), vec!["2"]);

    assert_eq!(column(&live, "x"), vec!["nan"], "the live key is unsigned");
    let batch_key = column(&batch, "x");
    assert!(
        batch_key == vec!["nan"] || batch_key == vec!["-nan"],
        "the batch key is whichever NaN DuckDB kept: {batch_key:?}"
    );
}

#[test]
fn negative_zero_groups_with_positive_zero() {
    let conn = conn();
    let events = vec![
        json!({"service": "nginx", "n": 1}),
        json!({"service": "nginx", "n": 2}),
    ];
    let dsl = "* | let x = if(n == 1, -1.0 * 0.0, 0.0) | stats count() by x";
    let live = live_rows(dsl, &events);
    let batch = batch_rows(&conn, dsl, &events);

    assert_eq!(live.len(), 1, "one group live: {live:?}");
    assert_eq!(batch.len(), 1, "one group in batch: {batch:?}");
    assert_eq!(column(&live, "count"), column(&batch, "count"));
    assert_eq!(column(&live, "count"), vec!["2"]);
    assert_eq!(column(&live, "x"), vec!["0.0"], "the live key is unsigned");
    let batch_key = column(&batch, "x");
    assert!(
        batch_key == vec!["-0.0"] || batch_key == vec!["0.0"],
        "the batch key is whichever zero DuckDB kept: {batch_key:?}"
    );
}

#[test]
fn the_aggregates_read_an_infinity() {
    let conn = conn();
    let events = vec![
        json!({"service": "nginx", "n": 1}),
        json!({"service": "nginx", "n": 2}),
    ];
    let rows = agreed(
        &conn,
        "* | let x = if(n == 1, 1.0 / 0, 2.5) \
         | stats max(x) as hi, min(x) as lo, sum(x) as total",
        &events,
    );
    assert_eq!(column(&rows, "hi"), vec!["inf"]);
    assert_eq!(column(&rows, "lo"), vec!["2.5"]);
    assert_eq!(column(&rows, "total"), vec!["inf"]);
}

/// The two events are identical because dedup's SQL ranks by `_time` and
/// the lanes pick their survivor differently (first-seen live, newest in
/// batch), so identical rows keep the case on its own subject.
#[test]
fn two_nan_rows_dedup_to_one() {
    let conn = conn();
    let event = json!({"service": "nginx", "_time": "2026-01-15T09:00:00Z"});
    let events = vec![event.clone(), event];
    let rows = agreed_folding_nan_in(&conn, "* | let x = 0.0 / 0 | dedup x", &events, "x");
    assert_eq!(rows.len(), 1, "one row survives dedup: {rows:?}");
}

#[test]
fn finite_floats_render_the_same_in_both_lanes() {
    let conn = conn();
    let rows = agreed(
        &conn,
        "* | let a = 2.5, b = 100.0 / 4, c = 0.0 - 1.5 | table a, b, c",
        &one_event(),
    );
    assert_eq!(column(&rows, "a"), vec!["2.5"]);
    assert_eq!(column(&rows, "b"), vec!["25.0"]);
    assert_eq!(column(&rows, "c"), vec!["-1.5"]);
}

#[test]
fn a_signed_nan_string_crosses_a_stage_byte_for_byte() {
    let conn = conn();
    let events = vec![json!({"service": "nginx", "n": 1})];
    for (expression, want) in [
        (r#"tostring(tonumber("-nan"))"#, "-nan"),
        (r#"tostring(tonumber("nan"))"#, "nan"),
    ] {
        let rows = agreed(
            &conn,
            &format!("* | let s = {expression} | table s"),
            &events,
        );
        assert_eq!(column(&rows, "s"), vec![want], "{expression}");
    }

    let rows = agreed(
        &conn,
        r#"* | let s = tostring(tonumber("-nan")) | let t = s | where t != "" | table s, t"#,
        &events,
    );
    assert_eq!(column(&rows, "s"), vec!["-nan"]);
    assert_eq!(column(&rows, "t"), vec!["-nan"]);
}

#[test]
fn the_two_nan_signs_are_one_identity() {
    let conn = conn();
    let events = vec![
        json!({"service": "nginx", "_time": "2026-01-15T09:00:00Z", "flag": true}),
        json!({"service": "nginx", "_time": "2026-01-15T09:00:00Z", "flag": false}),
    ];
    // `tonumber` carries the sign from the text in both lanes, so the
    // two rows really do hold differently-signed NaNs.
    let signed = r#"let x = if(flag, tonumber("nan"), tonumber("-nan"))"#;

    for (stage, column_name, want) in [
        ("stats count() by x", "count", "2"),
        ("stats dc(x) as d", "d", "1"),
    ] {
        let dsl = format!("* | {signed} | {stage}");
        let live = live_rows(&dsl, &events);
        let batch = batch_rows(&conn, &dsl, &events);
        assert_eq!(live.len(), 1, "one live row: {live:?}");
        assert_eq!(batch.len(), 1, "one batch row: {batch:?}");
        assert_eq!(column(&live, column_name), vec![want], "{dsl}");
        assert_eq!(column(&batch, column_name), vec![want], "{dsl}");
    }

    let grouped = format!("* | {signed} | stats count() by x");
    assert_eq!(column(&live_rows(&grouped, &events), "x"), vec!["nan"]);
    let batch_key = column(&batch_rows(&conn, &grouped, &events), "x");
    assert!(
        batch_key == vec!["nan"] || batch_key == vec!["-nan"],
        "the batch key is whichever NaN DuckDB kept: {batch_key:?}"
    );

    let listed = format!("* | {signed} | stats values(x) as vs");
    assert_eq!(
        column(&live_rows(&listed, &events), "vs"),
        vec![r#"["nan"]"#]
    );

    for dedup in ["dedup x", "drop flag | dedup"] {
        let dsl = format!("* | {signed} | {dedup}");
        let (live_x, batch_x) = agreed_apart_from(&conn, &dsl, &events, "x");
        assert_eq!(live_x.len(), 1, "one row survives {dedup} live: {live_x:?}");
        assert_eq!(
            batch_x.len(),
            1,
            "one row survives {dedup} in batch: {batch_x:?}"
        );
        assert_eq!(live_x, vec!["nan"], "the live cell is canonically unsigned");
        assert!(
            batch_x == vec!["nan"] || batch_x == vec!["-nan"],
            "the batch cell is the survivor's own spelling: {batch_x:?}"
        );
    }
}

#[test]
fn a_timestamp_group_key_is_the_instants_own_text() {
    let conn = conn();
    let events = vec![
        json!({"service": "nginx", "n": 1}),
        json!({"service": "nginx", "n": 2}),
    ];
    let rows = agreed(
        &conn,
        "* | let t = strptime(\"2026-01-15 09:00:00\", \"%Y-%m-%d %H:%M:%S\") \
         | stats count() by t",
        &events,
    );
    assert_eq!(rows.len(), 1, "one group: {rows:?}");
    assert_eq!(column(&rows, "t"), vec!["2026-01-15 09:00:00"]);
    assert_eq!(column(&rows, "count"), vec!["2"]);
}

#[test]
fn an_unsigned_number_keeps_its_identity_across_stages() {
    let conn = conn();
    let events = vec![
        json!({"service": "nginx", "_time": "2026-01-15T09:00:00Z", "request_id": u64::MAX}),
        json!({"service": "nginx", "_time": "2026-01-15T09:00:01Z", "request_id": u64::MAX - 1}),
    ];

    let deduped = agreed(&conn, "* | dedup request_id", &events);
    assert_eq!(
        deduped.len(),
        2,
        "two ids one apart are two rows: {deduped:?}"
    );

    let groups = agreed(&conn, "* | stats count() by request_id", &events);
    assert_eq!(groups.len(), 2, "…and two groups: {groups:?}");
    assert_eq!(column(&groups, "count"), vec!["1", "1"]);
    assert_eq!(
        column(&groups, "request_id"),
        vec!["18446744073709551614", "18446744073709551615"],
        "the group key carries the digits the sender sent"
    );
}

#[test]
fn a_computed_timestamp_agrees_across_the_lanes() {
    let conn = conn();
    let events = vec![json!({"service": "nginx", "n": 1})];
    let strptime = r#"strptime("2026-01-15 09:00:00", "%Y-%m-%d %H:%M:%S")"#;

    for expression in [
        format!(r#"* | let t = {strptime} | where t > "2020-01-01" | table service"#),
        format!(r#"* | let t = {strptime} | where "2020-01-01" < t | table service"#),
        format!(r#"* | let t = {strptime} | where t < "infinity" | table service"#),
        format!(r#"* | let t = {strptime} | where t > "-infinity" | table service"#),
    ] {
        let kept = agreed(&conn, &expression, &events);
        assert_eq!(kept.len(), 1, "the row must be kept: {expression}");
    }

    let dropped = agreed(
        &conn,
        &format!(r#"* | let t = {strptime} | where t > "2030-01-01" | table service"#),
        &events,
    );
    assert!(dropped.is_empty(), "the row must be dropped: {dropped:?}");

    let rendered = agreed(
        &conn,
        &format!("* | let t = {strptime} | let s = tostring(t) | table s"),
        &events,
    );
    assert_eq!(column(&rendered, "s"), vec!["2026-01-15 09:00:00"]);

    let unequal = agreed(
        &conn,
        &format!(r#"* | let t = {strptime} | let e = t == "infinity" | table e"#),
        &events,
    );
    assert_eq!(column(&unequal, "e"), vec!["false"]);
}

/// Stripping the malformed offset and comparing the rest would match the
/// row where the equivalent query returns no rows at all.
#[test]
fn a_malformed_offset_drops_the_row_where_batch_refuses_the_query() {
    let conn = conn();
    let events = vec![json!({"service": "nginx", "n": 1})];
    let dsl = r#"* | let t = strptime("2026-01-15 10:20:30", "%Y-%m-%d %H:%M:%S") | where t == "2026-01-15 10:20:30+ab:cd" | table service"#;

    let error = batch_outcome(&conn, dsl, &events)
        .expect_err("batch must refuse a malformed timestamp literal");
    assert!(
        error.contains("timestamp"),
        "the refusal must be about the timestamp: {error}"
    );
    assert!(
        live_rows(dsl, &events).is_empty(),
        "the live lane must drop the row where batch returns nothing at all"
    );
}

#[test]
fn extract_reads_a_computed_timestamp_where_batch_refuses_the_query() {
    let conn = conn();
    let events = one_event();
    let dsl = r#"* | let t = strptime("2026-01-15 09:00:00", "%Y-%m-%d %H:%M:%S") | extract "(?P<y>\d{4})" from t | table y"#;

    let error = batch_outcome(&conn, dsl, &events)
        .expect_err("batch must refuse regexp_extract over a TIMESTAMP");
    assert!(
        error.contains("regexp_extract") && error.contains("TIMESTAMP"),
        "the refusal must be the missing TIMESTAMP overload: {error}"
    );

    let live = live_rows(dsl, &events);
    assert_eq!(column(&live, "y"), vec!["2026"]);
}

/// Batch only: the live compiler has no scalar head to run, and refuses one.
#[test]
fn scalar_around_aggregate_output_runs_in_duckdb() {
    let dsl = "* | stats coalesce(sum(x), 0)";

    let query = trawl_core::parser::parse(dsl).expect("dsl parses");
    let emitted = emitter::emit(
        &query,
        "/data/**/*.parquet",
        trawl_core::context::EvalContext::capture(),
    )
    .expect("a scalar around an aggregate output emits");
    assert_eq!(
        emitted.sql,
        "SELECT COALESCE(SUM(\"x\"), ?) AS \"coalesce_x\"\n\
         FROM read_parquet('/data/**/*.parquet', union_by_name=true)",
    );
    assert_eq!(emitted.params, vec![SqlValue::Int(0)]);

    let conn = conn();
    let events = vec![
        json!({"service": "nginx", "x": 1}),
        json!({"service": "nginx", "x": 2}),
        json!({"service": "api", "x": 4}),
    ];
    let populated = batch_outcome(&conn, dsl, &events).expect("batch runs");
    assert_eq!(
        populated,
        vec![vec![("coalesce_x".to_owned(), "7".to_owned())]],
        "1 + 2 + 4 under the default name, in one row"
    );

    // No event reaches the stage. An empty file would give `DuckDB` no
    // `x` column to bind, so the search matches nothing instead, and the
    // stage is the same text. `SUM` of no rows is NULL, so the answer is
    // the zero `coalesce` supplies, still in one row.
    let empty = batch_outcome(&conn, "service=absent | stats coalesce(sum(x), 0)", &events)
        .expect("batch runs");
    assert_eq!(
        empty,
        vec![vec![("coalesce_x".to_owned(), "0".to_owned())]],
        "an aggregation over no rows still answers one row"
    );
}

#[test]
fn aggregate_position_refusals_match() {
    let row = "is an aggregation; use it in stats, timechart, eventstats or pivot";
    for (dsl, sentence) in [
        (
            "| where count(x) > 1",
            format!("invalid aggregation: count() {row}"),
        ),
        (
            "| let y = avg(x)",
            format!("invalid aggregation: avg() {row}"),
        ),
        (
            r#"| where lower(count(x)) == "a""#,
            format!("invalid aggregation: count() {row}"),
        ),
        (
            "| stats sum(count(x))",
            "invalid aggregation: count() cannot be nested inside sum()".to_owned(),
        ),
        (
            "| stats sum(coalesce(count(x), 0))",
            "invalid aggregation: count() cannot be nested inside sum()".to_owned(),
        ),
    ] {
        let query = trawl_core::parser::parse(dsl).expect("dsl parses");
        let batch = emitter::emit(
            &query,
            "/data/**/*.parquet",
            trawl_core::context::EvalContext::capture(),
        )
        .expect_err("batch refuses the aggregate's position")
        .to_string();
        let live = compile_stream_plan(&query.pipeline, &PinScope::unpinned(), None)
            .expect_err("live refuses the aggregate's position")
            .to_string();
        assert_eq!(batch, live, "{dsl}: the two doors disagree");
        assert_eq!(batch, sentence, "{dsl}");
    }

    for dsl in [
        "| where count == 1",
        r#"| where lower(service) == "nginx" | let y = coalesce(n, 0)"#,
        "| stats coalesce(sum(x), 0)",
        "| stats sum(coalesce(x, 0)) as total",
    ] {
        let query = trawl_core::parser::parse(dsl).expect("dsl parses");
        emitter::emit(
            &query,
            "/data/**/*.parquet",
            trawl_core::context::EvalContext::capture(),
        )
        .unwrap_or_else(|error| panic!("{dsl}: batch must accept it: {error}"));
    }
    // Live, where nothing scalar heads the slot: the scalar head is the
    // live lane's own refusal (`live_scalar_heads_are_refused`).
    for dsl in [
        "| where count == 1",
        r#"| where lower(service) == "nginx" | let y = coalesce(n, 0)"#,
    ] {
        let query = trawl_core::parser::parse(dsl).expect("dsl parses");
        compile_stream_plan(&query.pipeline, &PinScope::unpinned(), None)
            .unwrap_or_else(|error| panic!("{dsl}: live must accept it: {error}"));
    }
}

/// The literal below pins the same thing a second time, so a change to
/// both sides at once is still visible.
#[test]
fn the_row_doors_keep_an_event_maps_bytes() {
    let event = json!({
        "service": "nginx",
        "_time": "2026-01-15T09:00:00Z",
        "status": 200,
        "rate": 1.5,
        "flag": true,
        "missing": null,
        "message": "hello",
        "request_id": u64::MAX,
    });
    let map = event.as_object().unwrap();

    let before = serde_json::to_string(map).unwrap();
    let after = serde_json::to_string(&row::to_json(row::from_json(map))).unwrap();
    assert_eq!(after, before, "the map's bytes changed");
    assert_eq!(
        after,
        r#"{"_time":"2026-01-15T09:00:00Z","flag":true,"message":"hello","missing":null,"rate":1.5,"request_id":18446744073709551615,"service":"nginx","status":200}"#,
        "the map's key order or cell rendering changed"
    );
}

fn twenty_events() -> Vec<Value> {
    (0..20)
        .map(|id| {
            json!({
                "_time": format!("2026-01-15T09:00:{id:02}Z"),
                "id": id,
                "service": if id % 2 == 0 { "a" } else { "b" },
                "message": format!("k=v{id}"),
            })
        })
        .collect()
}

fn ids(rows: &TextRows) -> Vec<i64> {
    let mut ids: Vec<i64> = column(rows, "id")
        .iter()
        .map(|id| id.parse().expect("an integer id"))
        .collect();
    ids.sort_unstable();
    ids
}

fn columns_of(rows: &TextRows) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| row.iter().map(|(name, _)| name.clone()).collect())
        .collect()
}

fn only_row(dsl: &str, rows: &TextRows) -> Vec<(String, String)> {
    assert_eq!(rows.len(), 1, "{dsl:?} must answer one row: {rows:?}");
    rows[0].clone()
}

fn cells(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut cells: Vec<(String, String)> = pairs
        .iter()
        .map(|(name, text)| ((*name).to_owned(), (*text).to_owned()))
        .collect();
    cells.sort();
    cells
}

#[test]
fn sample_reads_completed_input() {
    let conn = conn();
    let events = twenty_events();
    let even_ids: Vec<i64> = (0..20).filter(|id| id % 2 == 0).collect();

    for dsl in [
        "service=a | sample 100",
        r#"* | where service == "a" | sample 100"#,
    ] {
        assert_eq!(ids(&batch_rows(&conn, dsl, &events)), even_ids, "{dsl}");
    }
    let dsl = "* | sort _time | head 5 | sample 5";
    assert_eq!(
        ids(&batch_rows(&conn, dsl, &events)),
        vec![0, 1, 2, 3, 4],
        "{dsl}"
    );
    let dsl = "* | tail 5 | sample 5";
    assert_eq!(
        ids(&batch_rows(&conn, dsl, &events)),
        vec![15, 16, 17, 18, 19],
        "{dsl}"
    );
    let dsl = "service=a | sample 100% | stats count() as n";
    assert_eq!(
        only_row(dsl, &batch_rows(&conn, dsl, &events)),
        cells(&[("n", "10")]),
    );
    for dsl in ["service=c | sample 5", "service=c | sample 50%"] {
        assert_eq!(batch_rows(&conn, dsl, &events), TextRows::new(), "{dsl}");
    }
}

#[test]
fn sample_output_stage_composition() {
    let conn = conn();
    let events = twenty_events();
    let all: Vec<i64> = (0..20).collect();
    let run = |dsl: &str| batch_rows(&conn, dsl, &events);

    let dsl = r#"* | sample 100 | where service == "a""#;
    assert_eq!(
        ids(&run(dsl)),
        (0..20).filter(|id| id % 2 == 0).collect::<Vec<_>>(),
        "{dsl}"
    );
    let dsl = "* | sample 1 | sample 100 | stats count() as n";
    assert_eq!(only_row(dsl, &run(dsl)), cells(&[("n", "1")]));
    let dsl = "* | sample 100 | stats count() as n by service";
    assert_eq!(
        run(dsl),
        vec![
            cells(&[("n", "10"), ("service", "a")]),
            cells(&[("n", "10"), ("service", "b")]),
        ],
        "{dsl}"
    );
    let dsl = "* | sample 100 | stats count() as n";
    assert_eq!(only_row(dsl, &run(dsl)), cells(&[("n", "20")]));

    let dsl = "* | sample 100 | sort id | head 3";
    assert_eq!(ids(&run(dsl)), vec![0, 1, 2], "{dsl}");
    let dsl = "* | sample 100 | tail 2";
    assert_eq!(ids(&run(dsl)), vec![18, 19], "{dsl}");

    let dsl = "* | sample 100 | table id, service";
    let rows = run(dsl);
    assert_eq!(ids(&rows), all, "{dsl}");
    assert!(
        columns_of(&rows).iter().all(|c| c == &["id", "service"]),
        "{dsl}: {rows:?}"
    );
    let dsl = "* | sample 100 | fields id";
    let rows = run(dsl);
    assert_eq!(ids(&rows), all, "{dsl}");
    assert!(columns_of(&rows).iter().all(|c| c == &["id"]), "{dsl}");
    let dsl = "* | sample 100 | drop message";
    let rows = run(dsl);
    assert_eq!(ids(&rows), all, "{dsl}");
    assert!(
        columns_of(&rows).iter().all(|c| c == &["id", "service"]),
        "{dsl}: {rows:?}"
    );
    let dsl = "* | sample 100 | rename service as svc";
    let rows = run(dsl);
    assert_eq!(ids(&rows), all, "{dsl}");
    assert_eq!(column(&rows, "svc").len(), 20, "{dsl}");

    let dsl = "* | sample 100 | timechart span=1h count()";
    let rows = run(dsl);
    assert_eq!(only_row(dsl, &rows), cells(&[("count", "20")]));

    let dsl = "* | sample 100 | top 10 service";
    assert_eq!(column(&run(dsl), "count"), vec!["10", "10"], "{dsl}");
    let dsl = "* | sample 100 | rare 10 service";
    assert_eq!(column(&run(dsl), "count"), vec!["10", "10"], "{dsl}");

    let dsl = "* | sample 100 | let x = id + 1";
    let mut xs: Vec<i64> = column(&run(dsl), "x")
        .iter()
        .map(|x| x.parse().unwrap())
        .collect();
    xs.sort_unstable();
    assert_eq!(xs, (1..=20).collect::<Vec<_>>(), "{dsl}");

    let dsl = r#"* | sample 100 | extract "k=(?P<k>\w+)" from message"#;
    let mut ks = column(&run(dsl), "k");
    let mut want: Vec<String> = (0..20).map(|id| format!("v{id}")).collect();
    want.sort();
    ks.sort();
    assert_eq!(ks, want, "{dsl}");

    let dsl = "* | sample 100 | dedup service";
    assert_eq!(column(&run(dsl), "service"), vec!["a", "b"], "{dsl}");
    let dsl = "* | sample 100 | eventstats count() as n by service";
    let rows = run(dsl);
    assert_eq!(ids(&rows), all, "{dsl}");
    assert!(column(&rows, "n").iter().all(|n| n == "10"), "{dsl}");
    let dsl = "* | sample 100 | pivot count() on service";
    assert_eq!(only_row(dsl, &run(dsl)), cells(&[("a", "10"), ("b", "10")]),);
}

/// The kv-tail half of this boundary lives in the engine's `kv_tail_order`
/// tests.
#[test]
fn sample_lane_boundaries() {
    let query = trawl_core::parser::parse("* | sample 5").expect("dsl parses");
    let err = compile_stream_plan(&query.pipeline, &PinScope::unpinned(), None)
        .expect_err("the live lane refuses sample");
    assert!(
        matches!(
            &err,
            trawl_core::stream::StreamPlanError::UnsupportedStage { stage, .. } if stage == "sample"
        ),
        "{err}"
    );
}
