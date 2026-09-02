// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Whole-pipeline parity for the values that have no JSON spelling.
//!
//! `filter_parity` proves the search stage agrees and `pin_stage_parity`
//! proves one `| where`/`| let` cell agrees. Neither watches the carrier
//! that moves a row between stages: a row crossing a stage boundary as
//! JSON turns a computed `0/0` into NULL one stage before anything asks
//! about it, so `* | let x = 0/0 | where x == x` keeps the row in batch
//! while the live tail drops it.
//!
//! So these cases are whole pipelines, run through both lanes over the
//! same events, compared cell by cell as text: the batch side rendered
//! by `DuckDB`'s own `::VARCHAR`, the live side by
//! [`trawl_core::row::cell_text`], which is what a group key and a
//! displayed cell already are. Each case also states the answer, because
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

/// A lane's whole answer: rows sorted, so the comparison ignores order,
/// and a `Vec` rather than a set, so it does not ignore multiplicity.
/// A set would compare a lane emitting one row equal to a lane emitting
/// the same row twice, which is exactly the failure a broken `dedup`
/// produces.
type TextRows = Vec<TextRow>;

/// Whether a column takes part in the cell-by-cell comparison.
///
/// `_time` does not, for the reason `pin_stage_parity` excludes it too:
/// the live lane carries the sender's wire text while the batch lane
/// renders the parsed instant (`2026-01-15T09:00:00Z` vs
/// `2026-01-15 09:00:00`). That representation gap is out of scope here
/// and would mask every case in the file.
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

/// The same, surfacing a `DuckDB` error instead of panicking on it: the
/// shape a case needs when the batch lane refuses the query and the
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
    let emitted = emitter::emit_with_pins(
        &query,
        &source,
        &FieldTypes::new(),
        trawl_core::context::EvalContext::capture(),
    )
    .expect("emit succeeds");
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

/// Run the same pipeline through the live lane over the same events.
fn live_rows(dsl: &str, events: &[Value]) -> TextRows {
    // One anchor for this lane's whole run: these cases are about the
    // carrier, not about the clock, so no stage samples a second instant
    // (ADR-0017 §3).
    let anchor = trawl_core::context::EvalContext::capture();
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

/// Row order is an implementation detail of each lane; the content and
/// how many times it appears are not.
fn sorted(mut rows: TextRows) -> TextRows {
    rows.sort();
    rows
}

/// Both lanes, same events, same answer, returned so the caller can say
/// what the answer must be.
///
/// The comparison is byte-exact, in every column. A normalization applied
/// here would be applied to every cell of every case, and a non-injective
/// one (folding `-nan` onto `nan`, say) would then launder a real VARCHAR
/// divergence, an ordinary string cell carrying `-nan` through a stage,
/// into agreement. Cases whose cell text is genuinely not comparable opt
/// in, by column, through [`agreed_folding_nan_in`].
fn agreed(conn: &Connection, dsl: &str, events: &[Value]) -> TextRows {
    assert_lanes_agree(conn, dsl, events, None)
}

/// [`agreed`], with one named column's NaN sign folded in both lanes.
///
/// The opt-in exists for exactly one shape: a cell holding a computed NaN
/// (`0.0 / 0`), whose sign is the hardware's rather than either engine's,
/// read by two different renderers. The live side goes through the
/// identity path, which ties the two signs deliberately so a group is one
/// group ([`row::cell_text`]); the batch side through `::VARCHAR`, which
/// prints the sign the value carries. No production path compares those
/// bytes across lanes (the wire renders a NaN `null` on both sides).
///
/// Everything else in the row stays byte-exact, every other column, the
/// row count and the multiplicity alike, and a case that names a column
/// here says so in its own text.
fn agreed_folding_nan_in(conn: &Connection, dsl: &str, events: &[Value], column: &str) -> TextRows {
    assert_lanes_agree(conn, dsl, events, Some(column))
}

/// Both lanes over a case where one column holds a nondeterministic
/// representative: which of several equal rows each lane kept, a choice
/// `dedup` and a group-by both make for themselves.
///
/// Deliberately not [`agreed_folding_nan_in`]: that one exists for a
/// value whose sign is the hardware's, and stretching it to cover a
/// representative choice would make its contract mean two things. Here
/// every other column is still compared byte-exactly, multiplicity
/// included, and the named column comes back per lane so the caller can
/// say what each lane must show.
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

/// The value of one column across the agreed rows, sorted, one entry
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

/// A computed NaN survives the stage boundary, so `x == x` is true in
/// both lanes (`DuckDB` orders NaN equal to itself, ADR-0011's total
/// DOUBLE order).
#[test]
fn a_computed_nan_survives_the_stage_boundary() {
    let conn = conn();
    // `x` holds a computed NaN, whose sign is the hardware's: the one
    // cell in this row the two lanes cannot be compared on byte for
    // byte. Every other column, and the row count, still are.
    let kept = agreed_folding_nan_in(
        &conn,
        "* | let x = 0.0 / 0 | where x == x",
        &one_event(),
        "x",
    );
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
    // one is true. (Spelled as a plain decimal: the DSL float grammar has
    // no exponent form.)
    let rows = agreed(
        &conn,
        "* | let x = 0.0 / 0 | let y = x > 1000000.0 | table y",
        &one_event(),
    );
    assert_eq!(column(&rows, "y"), vec!["true"]);
}

/// A NaN group key is one group, rendered `nan`, not a null group and not
/// one group per row.
///
/// The key's text carries the same parked divergence `-0.0` has: the
/// identity path normalizes the sign the total order ties, while `DuckDB`
/// displays whichever representative row it kept (`0.0 / 0` produces a NaN
/// whose sign is the hardware's). So the grouping is compared across lanes
/// and the rendering is asserted per lane.
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

/// Negative zero groups with positive zero: they compare equal, so a key
/// that split them would contradict the comparison.
///
/// The two lanes agree on the grouping and diverge, deliberately, on the
/// key's text: `DuckDB` shows whichever representative it kept (`-0.0`),
/// while [`row::cell_text`] normalizes the sign so that identity and
/// display say the same thing. That rendering delta is pinned here rather
/// than left to be noticed.
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

/// The accumulators see an infinity as the value it is: `max` does not
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

/// Two NaN rows dedup to one: the identity a dedup key expresses is the
/// same identity the comparison does.
///
/// The two events are identical because dedup's SQL ranks by `_time` and
/// the lanes pick their survivor differently (first-seen live, newest in
/// batch), so identical rows keep the case on its own subject.
#[test]
fn two_nan_rows_dedup_to_one() {
    let conn = conn();
    let event = json!({"service": "nginx", "_time": "2026-01-15T09:00:00Z"});
    let events = vec![event.clone(), event];
    // `x` is a computed NaN, so its sign is the hardware's and the two
    // lanes render it through different doors, the one shape the fold
    // exists for. The two source rows are identical, so which one
    // survives cannot matter here; only the row count is under test, and
    // every other column is still compared byte-exactly.
    let rows = agreed_folding_nan_in(&conn, "* | let x = 0.0 / 0 | dedup x", &events, "x");
    assert_eq!(rows.len(), 1, "one row survives dedup: {rows:?}");
}

/// A finite float takes the same text in both lanes: the ordinary case
/// the specials above must not break.
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

/// A string cell crosses a stage byte for byte, `-nan` included.
///
/// This is the case a global sign-fold in the harness would launder:
/// `tostring(tonumber("-nan"))` is an ordinary VARCHAR whose sign is
/// explicit in the text (no hardware involved, since the cast domain
/// carries the sign from the input string in both lanes), so a carrier
/// regression that flipped it to `nan` is a real divergence and has to
/// be visible. Nothing here is folded; the comparison is exact.
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

    // …and the same text through a second stage, so it crosses a
    // boundary rather than being read where it was made: carried across
    // two stages and compared as text at the end.
    let rows = agreed(
        &conn,
        r#"* | let s = tostring(tonumber("-nan")) | let t = s | where t != "" | table s, t"#,
        &events,
    );
    assert_eq!(column(&rows, "s"), vec!["-nan"]);
    assert_eq!(column(&rows, "t"), vec!["-nan"]);
}

/// The two NaN signs are one identity: one group, one distinct value,
/// one row after `dedup`.
///
/// `cell_text` renders a float through `DuckDB`'s own text, which carries
/// the sign. That is right for `tostring()` and wrong for a key, because
/// the comparator ties the two signs (`NaN = NaN` is true) and a signed
/// key would split a group the engine does not split.
///
/// The grouping is asserted in both lanes; the key's text carries the
/// same parked divergence `-0.0` has, since `DuckDB` displays whichever
/// representative row it kept.
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

    // The group key itself: unsigned live, whichever sign DuckDB kept in
    // batch.
    let grouped = format!("* | {signed} | stats count() by x");
    assert_eq!(column(&live_rows(&grouped, &events), "x"), vec!["nan"]);
    let batch_key = column(&batch_rows(&conn, &grouped, &events), "x");
    assert!(
        batch_key == vec!["nan"] || batch_key == vec!["-nan"],
        "the batch key is whichever NaN DuckDB kept: {batch_key:?}"
    );

    // `values()` collects one member: the two signs are one value.
    let listed = format!("* | {signed} | stats values(x) as vs");
    assert_eq!(
        column(&live_rows(&listed, &events), "vs"),
        vec![r#"["nan"]"#]
    );

    // …and both `dedup` forms keep one row: the by-field form, and the
    // whole-row one that reads the same cell through `cell_key`.
    //
    // These NaNs are sign-explicit, read from the text `tonumber("-nan")`
    // takes rather than computed, so nothing here is hardware-dependent.
    // What each lane chooses for itself is which of the two equal rows
    // survives, and the survivor carries its own spelling: `DuckDB` keeps
    // whichever row it kept, while the live lane's identity rule makes
    // every NaN cell read `nan`.
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

/// A computed TIMESTAMP groups by the text `DuckDB` prints, not by the
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

/// A number above `i64::MAX` is its own identity, not the double it
/// computes as.
///
/// The row's door reads every cell, pass-through fields included, so
/// `18446744073709551615` must not round on the way in: a rounded value
/// collapses with its neighbour in a `dedup` and merges two groups into
/// one.
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
/// A computed instant rather than `_time`, which [`compared`] excludes:
/// that exclusion is about the sender's wire text and would hide every
/// case here.
#[test]
fn a_computed_timestamp_agrees_across_the_lanes() {
    let conn = conn();
    let events = vec![json!({"service": "nginx", "n": 1})];
    let strptime = r#"strptime("2026-01-15 09:00:00", "%Y-%m-%d %H:%M:%S")"#;

    // Compared against a text, in both operand orders.
    for expression in [
        format!(r#"* | let t = {strptime} | where t > "2020-01-01" | table service"#),
        format!(r#"* | let t = {strptime} | where "2020-01-01" < t | table service"#),
        // …and against an infinity, which orders above every date.
        format!(r#"* | let t = {strptime} | where t < "infinity" | table service"#),
        format!(r#"* | let t = {strptime} | where t > "-infinity" | table service"#),
    ] {
        let kept = agreed(&conn, &expression, &events);
        assert_eq!(kept.len(), 1, "the row must be kept: {expression}");
    }

    // A comparison that does not hold drops the row in both lanes.
    let dropped = agreed(
        &conn,
        &format!(r#"* | let t = {strptime} | where t > "2030-01-01" | table service"#),
        &events,
    );
    assert!(dropped.is_empty(), "the row must be dropped: {dropped:?}");

    // The instant's text is DuckDB's own cast text on both sides.
    let rendered = agreed(
        &conn,
        &format!("* | let t = {strptime} | let s = tostring(t) | table s"),
        &events,
    );
    assert_eq!(column(&rendered, "s"), vec!["2026-01-15 09:00:00"]);

    // An infinity is a value the comparison reaches, not a text it
    // fails on: equality against one is false for a finite instant in
    // both lanes, where an unreadable text would be UNKNOWN.
    let unequal = agreed(
        &conn,
        &format!(r#"* | let t = {strptime} | let e = t == "infinity" | table e"#),
        &events,
    );
    assert_eq!(column(&unequal, "e"), vec!["false"]);
}

/// A text with no timestamp reading drops the row live, because the batch
/// lane refuses the query outright.
///
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

/// A stage-computed TIMESTAMP is text to an `extract`.
///
/// The two lanes do not agree on this shape: `DuckDB` has no
/// `regexp_extract(TIMESTAMP, …)` overload and does not implicitly cast
/// one to VARCHAR, so the emitted `regexp_extract("t", ?, 1)`
/// (`emitter::pipeline::process_extract` quotes the source field and
/// casts nothing) is a Binder Error and the batch lane returns no rows at
/// all. The refusal is pinned here so adding a cast later is a deliberate
/// change rather than drift.
///
/// What this case protects is the live answer: a timestamp cell must
/// still read as its cast text, or the extraction silently finds nothing.
/// Behind `extract kv` that same code is the batch tail, where there is
/// no SQL lane to refuse anything, so the loss would be the whole answer
/// rather than half of it.
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

    // The live lane reads the cell's cast text.
    let live = live_rows(dsl, &events);
    assert_eq!(column(&live, "y"), vec!["2026"]);
}

/// An SSE frame's bytes are the wire JSON of the event map itself.
///
/// The round trip through the row door (`from_json` then `to_json`) must
/// change neither key order nor cell rendering, so the expectation is
/// computed as `serde_json::to_string` of the map rather than
/// transcribed. The literal below pins the same thing a second time, so a
/// change to both sides at once is still visible.
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

    // The wire JSON, serialized.
    let before = serde_json::to_string(map).unwrap();
    // The same event, through both row doors.
    let after = serde_json::to_string(&row::to_json(row::from_json(map))).unwrap();
    assert_eq!(after, before, "the frame bytes changed");
    assert_eq!(
        after,
        r#"{"_time":"2026-01-15T09:00:00Z","flag":true,"message":"hello","missing":null,"rate":1.5,"request_id":18446744073709551615,"service":"nginx","status":200}"#,
        "the frame's key order or cell rendering changed"
    );
}
