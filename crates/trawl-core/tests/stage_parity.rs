// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Whole-pipeline parity for the values that have no JSON spelling.
//!
//! `filter_parity` proves the search stage agrees and `pin_stage_parity`
//! proves one `| where`/`| let` CELL agrees. Neither could see this
//! milestone's bug, because it was not in a comparison: it was in the
//! CARRIER. A row crossed each stage boundary as JSON, so a computed
//! `0/0` became NULL one stage before anything asked about it, and
//! `* | let x = 0/0 | where x == x` kept the row in batch while the live
//! tail dropped it.
//!
//! So these cases are whole PIPELINES, run through both lanes over the
//! same events, compared cell by cell as TEXT — the batch side rendered
//! by `DuckDB`'s own `::VARCHAR`, the live side by
//! [`trawl_core::row::cell_text`], which is what a group key and a
//! displayed cell already are. Each case also states the ANSWER, because
//! two lanes can agree on the wrong one.

use std::io::Write as _;

use duckdb::Connection;
use serde_json::{Value, json};
use trawl_core::emitter::{self, SqlValue};
use trawl_core::pin_scope::PinScope;
use trawl_core::row::{self, Row};
use trawl_core::schema::FieldTypes;
use trawl_core::stream::{StageResult, StreamPlan, apply_stage, compile_stream_plan};

/// One row of one lane: (column, rendered text), sorted by column so the
/// comparison never depends on projection order.
type TextRow = Vec<(String, String)>;

/// A lane's whole answer: rows SORTED, so the comparison ignores order,
/// and a `Vec` rather than a set, so it does not ignore MULTIPLICITY.
/// A set would have compared a lane emitting one row equal to a lane
/// emitting the same row twice — exactly the failure a broken `dedup`
/// produces.
type TextRows = Vec<TextRow>;

/// Whether a column takes part in the cell-by-cell comparison.
///
/// `_time` does not, for the reason `pin_stage_parity` excludes it too:
/// the live lane carries the sender's wire TEXT while the batch lane
/// renders the parsed instant (`2026-01-15T09:00:00Z` vs
/// `2026-01-15 09:00:00`). That is a timestamp-representation question
/// this milestone does not touch, and it would mask every case here.
fn compared(column: &str) -> bool {
    column != "_time"
}

/// The one text normalization the comparison applies, to BOTH lanes
/// alike: a NaN's SIGN.
///
/// The sign of a computed NaN is the hardware's, not the engine's
/// (`0.0 / 0` is `-nan` on x86 and need not be elsewhere), and the two
/// lanes reach their text through different renderers: the live side
/// through the IDENTITY path, which deliberately ties the two signs so a
/// group is one group ([`row::cell_text`]), the batch side through
/// `::VARCHAR`, which prints whichever sign the value carries. No
/// production path compares those bytes across lanes — the wire renders
/// a NaN `null` on both sides — so the harness folds the sign rather
/// than pinning this machine's.
///
/// Applied SYMMETRICALLY, so it can hide no divergence: whatever it
/// does to one lane it does to the other, and every other byte stays
/// exact.
fn fold_nan_sign(text: String) -> String {
    if text == "-nan" {
        "nan".to_owned()
    } else {
        text
    }
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
            }
        })
        .collect()
}

/// Run the pipeline as SQL and read every cell back as `DuckDB`'s own
/// VARCHAR rendering — the one form that can carry `inf`/`nan`, where
/// `to_json` would null them out and hide exactly what is under test.
fn batch_rows(conn: &Connection, dsl: &str, events: &[Value]) -> TextRows {
    batch_outcome(conn, dsl, events)
        .unwrap_or_else(|error| panic!("batch must run for {dsl:?}: {error}"))
}

/// The same, surfacing a `DuckDB` ERROR instead of panicking on it — the
/// shape a case needs when the batch lane REFUSES the query and the
/// streaming contract is "eval answers NULL / drops the row".
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
    let emitted =
        emitter::emit_with_pins(&query, &source, &FieldTypes::new()).expect("emit succeeds");
    let params = bind_params(&emitted.params);
    let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();

    // The emitted column names first, so each cell can be cast to
    // VARCHAR under its own name.
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
                cells.push((
                    name.clone(),
                    fold_nan_sign(text.unwrap_or_else(|| "null".to_owned())),
                ));
            }
            cells.sort();
            Ok(cells)
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<TextRows, _>>()
        .map_err(|error| error.to_string())?;
    Ok(sorted(rows))
}

/// Run the same pipeline through the live lane over the same events.
fn live_rows(dsl: &str, events: &[Value]) -> TextRows {
    let query = trawl_core::parser::parse(dsl).expect("dsl parses");
    let plan = compile_stream_plan(&query.pipeline, &PinScope::unpinned()).expect("plan compiles");
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
                    .all(|stage| apply_stage(stage, &mut event) == StageResult::Pass)
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
                    .all(|stage| apply_stage(stage, &mut event) == StageResult::Pass)
                {
                    aggregation.feed_event(&event);
                }
            }
            aggregation
                .snapshot()
                .1
                .into_iter()
                .filter_map(|mut event| {
                    post_stages
                        .iter_mut()
                        .all(|stage| apply_stage(stage, &mut event) == StageResult::Pass)
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
                .map(|(name, cell)| (name.clone(), fold_nan_sign(row::cell_text(cell))))
                .collect();
            cells.sort();
            cells
        })
        .collect();
    sorted(rendered)
}

/// Row order is an implementation detail of each lane; the CONTENT and
/// how many times it appears are not.
fn sorted(mut rows: TextRows) -> TextRows {
    rows.sort();
    rows
}

/// Both lanes, same events, same answer — returned so the caller can say
/// what the answer must BE.
fn agreed(conn: &Connection, dsl: &str, events: &[Value]) -> TextRows {
    let batch = batch_rows(conn, dsl, events);
    let live = live_rows(dsl, events);
    assert_eq!(
        live, batch,
        "lane divergence\ndsl: {dsl:?}\nlive: {live:?}\nbatch: {batch:?}"
    );
    batch
}

/// The value of one column across the agreed rows, sorted — one entry
/// per row, so a repeated row is a repeated value.
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

/// The case this milestone exists for: a computed NaN survives the stage
/// boundary, so `x == x` is TRUE in both lanes (`DuckDB` orders NaN equal
/// to itself — ADR-0011's total DOUBLE order).
#[test]
fn a_computed_nan_survives_the_stage_boundary() {
    let conn = conn();
    let kept = agreed(&conn, "* | let x = 0.0 / 0 | where x == x", &one_event());
    assert_eq!(kept.len(), 1, "the row must be kept: {kept:?}");
}

/// An infinity is an ordinary value to a later stage: it compares, and
/// the comparison's own result crosses the next boundary too.
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
    // one is TRUE. (Spelled as a plain decimal: the DSL float grammar has
    // no exponent form.)
    let rows = agreed(
        &conn,
        "* | let x = 0.0 / 0 | let y = x > 1000000.0 | table y",
        &one_event(),
    );
    assert_eq!(column(&rows, "y"), vec!["true"]);
}

/// A NaN group key is ONE group, rendered `nan` — not a null group, and
/// not one group per row.
///
/// The key's TEXT is the parked divergence `-0.0` has: the identity path
/// normalizes the sign the total order ties, while `DuckDB` displays
/// whichever representative row it kept (`0.0 / 0` produces a NaN whose
/// sign is the hardware's). So the GROUPING is compared across lanes and
/// the rendering is asserted per lane.
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

/// Negative zero groups WITH positive zero — they compare equal, so a key
/// that split them would contradict the comparison.
///
/// The two lanes agree on the GROUPING and diverge, deliberately, on the
/// key's TEXT: `DuckDB` shows whichever representative it kept (`-0.0`),
/// while [`row::cell_text`] normalizes the sign so that identity and
/// display say the same thing. That is the one enumerated rendering
/// delta of this milestone, pinned here rather than left to be noticed.
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

/// The accumulators see an infinity as the value it is — `max` does not
/// skip it, `sum` does not ignore it, `min` still finds the finite one.
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

/// Two NaN rows dedup to ONE: the identity a dedup key expresses is the
/// same identity the comparison does.
///
/// The two events are identical (dedup's SQL ranks by `_time`, and the
/// lanes pick their survivor differently — first-seen live, newest in
/// batch — so identical rows keep the case on its own subject).
#[test]
fn two_nan_rows_dedup_to_one() {
    let conn = conn();
    let event = json!({"service": "nginx", "_time": "2026-01-15T09:00:00Z"});
    let events = vec![event.clone(), event];
    let rows = agreed(&conn, "* | let x = 0.0 / 0 | dedup x", &events);
    assert_eq!(rows.len(), 1, "one row survives dedup: {rows:?}");
}

/// A finite float takes the same text in both lanes — the ordinary case
/// the specials above must not have broken.
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

/// The two NaN SIGNS are one identity: one group, one distinct value,
/// one row after `dedup`.
///
/// `cell_text` renders a float through `DuckDB`'s own text, which
/// carries the sign — right for `tostring()`, wrong for a KEY, because
/// the comparator ties the two signs (`NaN = NaN` is true). A signed key
/// split a group the engine does not split.
///
/// The GROUPING is asserted in both lanes; the key's TEXT is the same
/// parked divergence `-0.0` has, since `DuckDB` displays whichever
/// representative row it kept.
#[test]
fn the_two_nan_signs_are_one_identity() {
    let conn = conn();
    let events = vec![
        json!({"service": "nginx", "_time": "2026-01-15T09:00:00Z", "flag": true}),
        json!({"service": "nginx", "_time": "2026-01-15T09:00:00Z", "flag": false}),
    ];
    // `tonumber` carries the sign from the TEXT in both lanes, so the
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

    // The group key itself: unsigned live, whichever sign DuckDB kept in
    // batch.
    let grouped = format!("* | {signed} | stats count() by x");
    assert_eq!(column(&live_rows(&grouped, &events), "x"), vec!["nan"]);
    let batch_key = column(&batch_rows(&conn, &grouped, &events), "x");
    assert!(
        batch_key == vec!["nan"] || batch_key == vec!["-nan"],
        "the batch key is whichever NaN DuckDB kept: {batch_key:?}"
    );

    // `values()` collects ONE member — it used to list both signs.
    let listed = format!("* | {signed} | stats values(x) as vs");
    assert_eq!(
        column(&live_rows(&listed, &events), "vs"),
        vec![r#"["nan"]"#]
    );

    // …and both `dedup` forms keep ONE row: the by-field form, and the
    // whole-row one that reads the same cell through `cell_key`.
    for dedup in ["dedup x", "drop flag | dedup"] {
        let dsl = format!("* | {signed} | {dedup}");
        let deduped = agreed(&conn, &dsl, &events);
        assert_eq!(deduped.len(), 1, "one row survives {dedup}: {deduped:?}");
    }
}

/// A computed TIMESTAMP groups by the text `DuckDB` prints — not by the
/// JSON string it becomes on the wire, quote characters and all.
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

/// A number above `i64::MAX` is its OWN identity, not the double it
/// computes as.
///
/// The row's door reads every cell now, so a pass-through field carrying
/// `18446744073709551615` would have rounded on the way in — collapsing
/// it with its neighbour in a `dedup`, and merging two groups into one.
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

/// Whole pipelines over TIMESTAMP cells: the instant a stage computes
/// compares, renders and groups the same in both lanes.
///
/// Deliberately not covered by the specials matrix above, which EXCLUDES
/// `_time` — that exclusion is about the sender's wire text, and it would
/// have hidden every case here.
#[test]
fn a_computed_timestamp_agrees_across_the_lanes() {
    let conn = conn();
    let events = vec![json!({"service": "nginx", "n": 1})];
    let strptime = r#"strptime("2026-01-15 09:00:00", "%Y-%m-%d %H:%M:%S")"#;

    // Compared against a text, in both operand orders.
    for expression in [
        format!(r#"* | let t = {strptime} | where t > "2020-01-01" | table service"#),
        format!(r#"* | let t = {strptime} | where "2020-01-01" < t | table service"#),
        // …and against an INFINITY, which orders above every date.
        format!(r#"* | let t = {strptime} | where t < "infinity" | table service"#),
        format!(r#"* | let t = {strptime} | where t > "-infinity" | table service"#),
    ] {
        let kept = agreed(&conn, &expression, &events);
        assert_eq!(kept.len(), 1, "the row must be kept: {expression}");
    }

    // A comparison that does NOT hold drops the row in both lanes.
    let dropped = agreed(
        &conn,
        &format!(r#"* | let t = {strptime} | where t > "2030-01-01" | table service"#),
        &events,
    );
    assert!(dropped.is_empty(), "the row must be dropped: {dropped:?}");

    // The instant's TEXT is DuckDB's own cast text on both sides.
    let rendered = agreed(
        &conn,
        &format!("* | let t = {strptime} | let s = tostring(t) | table s"),
        &events,
    );
    assert_eq!(column(&rendered, "s"), vec!["2026-01-15 09:00:00"]);

    // An infinity is a value the comparison reaches, not a text it
    // fails on: equality against one is FALSE for a finite instant in
    // both lanes, where an unreadable text would have been UNKNOWN.
    let unequal = agreed(
        &conn,
        &format!(r#"* | let t = {strptime} | let e = t == "infinity" | table e"#),
        &events,
    );
    assert_eq!(column(&unequal, "e"), vec!["false"]);
}

/// A text with no timestamp reading DROPS the row live, because the batch
/// lane refuses the query outright.
///
/// eval used to strip the malformed offset and compare the rest, so the
/// row MATCHED where the equivalent query returned no rows at all.
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

/// The D1 ordering guard: an SSE frame's BYTES are what they were before
/// rows were typed.
///
/// The pre-change lane serialized the event map itself, so the expected
/// bytes are exactly `serde_json::to_string` of that map — computed here
/// by the old path's own rule rather than transcribed. The literal below
/// pins the same thing a second time, so a change to BOTH sides at once
/// is still visible.
#[test]
fn an_sse_frame_keeps_its_pre_change_bytes() {
    let event = json!({
        "service": "nginx",
        "_time": "2026-01-15T09:00:00Z",
        "status": 200,
        "rate": 1.5,
        "flag": true,
        "missing": null,
        "message": "hello",
        // Above `i64::MAX`: the frame must carry the digits, not the
        // double they round to.
        "request_id": u64::MAX,
    });
    let map = event.as_object().unwrap();

    // What the pre-change lane emitted: the wire JSON, serialized.
    let before = serde_json::to_string(map).unwrap();
    // What this lane emits: the same event, through both doors.
    let after = serde_json::to_string(&row::to_json(row::from_json(map))).unwrap();
    assert_eq!(after, before, "the frame bytes changed");
    assert_eq!(
        after,
        r#"{"_time":"2026-01-15T09:00:00Z","flag":true,"message":"hello","missing":null,"rate":1.5,"request_id":18446744073709551615,"service":"nginx","status":200}"#,
        "the frame's key order or cell rendering changed"
    );
}
