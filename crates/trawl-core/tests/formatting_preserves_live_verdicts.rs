// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Formatting never changes what a query means, in the live lane
//! (ADR-0061). Each fixture query compiles to a live filter and stream
//! plan as written and again as `checked_format_query` spells it. Both
//! run over the same in-memory events at one fixed instant, and the
//! per-event filter verdicts and the emitted rows must be equal.
//!
//! The comparison stays inside this lane. Whether the live lane agrees
//! with the SQL lane is a different property (`filter_parity`), and
//! mixing the two here would hide a formatter defect behind a lane gap.
//!
//! Every row also names the row count it must emit, so a query that
//! matches nothing before and after formatting cannot pass by accident.

use serde_json::{Map, Value, json};
use trawl_core::context::EvalContext;
use trawl_core::filter::CompiledFilter;
use trawl_core::format::checked_format_query;
use trawl_core::parser;
use trawl_core::pin_scope::PinScope;
use trawl_core::row::{self, Row};
use trawl_core::schema::FieldTypes;
use trawl_core::stream::{
    self, LiveOutcome, StageResult, StreamPlan, apply_stage, compile_stream_plan,
};

/// The one instant every case evaluates at, so `last=` reads a fixed
/// window and nothing here samples a clock.
fn anchor() -> EvalContext {
    EvalContext::at(
        chrono::DateTime::parse_from_rfc3339("2024-01-15T10:06:00Z")
            .expect("literal is RFC 3339")
            .with_timezone(&chrono::Utc),
    )
}

/// Six events. Keys that start with a field-start keyword (`or.a`,
/// `true.a`, `not.a`) are the spellings the formatter used to print
/// bare; the queries name the first as `` `OR.a` ``. Ingest ASCII-folds
/// every key it writes, so the events carry the folded spelling. `opt` is `null` on one event and absent from the last two. One
/// `host` holds a literal `*` and one `message` holds a `*`, so bare `*`
/// and quoted `"*"` select different events.
fn events() -> Vec<Map<String, Value>> {
    [
        json!({"_time": "2024-01-15T10:00:00Z", "service": "web", "host": "web01",
               "message": "GET /a ok", "or.a": "x", "true.a": 1, "not.a": "p", "opt": "keep"}),
        json!({"_time": "2024-01-15T10:00:01Z", "service": "web", "host": "web02",
               "message": "GET /b connection refused", "or.a": "y", "true.a": 2, "not.a": "q",
               "opt": null}),
        json!({"_time": "2024-01-15T10:00:02Z", "service": "db", "host": "*",
               "message": "star * in the message", "or.a": "x", "true.a": 1, "not.a": "p",
               "opt": "keep"}),
        json!({"_time": "2024-01-15T10:00:03Z", "service": "db", "host": "db01",
               "message": "bad gateway upstream", "or.a": "", "true.a": 3, "not.a": "r",
               "opt": "drop"}),
        json!({"_time": "2024-01-15T10:05:00Z", "service": "web", "host": "web01",
               "message": "POST /c timeout", "or.a": "x", "true.a": 2, "not.a": "q"}),
        json!({"_time": "2024-01-15T10:05:01Z", "service": "cache", "host": "c01",
               "message": "connection refused again", "or.a": "z", "true.a": 1, "not.a": "p"}),
    ]
    .into_iter()
    .map(|event| event.as_object().expect("an object").clone())
    .collect()
}

/// `(query, rows it emits)`. Six events in all; no row expects 0 or 6
/// unless the query is about matching everything.
const FIXTURES: &[(&str, usize)] = &[
    // Keyword-prefixed fields, in the search stage.
    ("`OR.a`=x", 3),
    ("`OR.a`=\"\"", 1),
    ("`true.a`=1", 3),
    ("`not.a`=p service=db", 1),
    ("`OR.a`=x OR `true.a`=2", 4),
    ("service=web `OR.a`=x OR service=cache", 3),
    ("NOT `OR.a`=x", 3),
    // Keyword-prefixed fields, in pipeline expressions.
    ("* | where `OR.a` == \"x\" and `true.a` == 1", 2),
    ("* | where `true.a` in (2, 3)", 3),
    (
        "* | let t = `true.a` + 1 | where t > 2 | stats count() as n by service",
        3,
    ),
    // NOT text terms.
    ("NOT refused", 4),
    ("NOT \"bad gateway\"", 5),
    ("NOT refused NOT timeout", 3),
    ("connection NOT again", 1),
    ("refused OR NOT service=web", 4),
    // Bare against quoted `*`.
    ("*", 6),
    ("\"*\"", 1),
    ("host=*", 6),
    ("host=\"*\"", 1),
    ("service=db \"*\"", 1),
    // Missing and null fields.
    ("opt=*", 3),
    ("opt=keep", 2),
    ("* | where isnull(opt)", 3),
    ("* | where opt != \"keep\" | stats count() as n", 1),
    // Time bounds: absolute, relative to the anchor, and last-wins.
    (
        "earliest=\"2024-01-15T10:00:01Z\" latest=\"2024-01-15T10:05:00Z\"",
        3,
    ),
    (
        "service=web OR service=cache earliest=\"2024-01-15T10:00:01Z\"",
        3,
    ),
    ("latest=\"2024-01-15T10:00:02Z\" NOT refused", 1),
    ("last=2m", 2),
    ("last=1h last=2m service=web", 1),
    ("NOT refused last=10m OR service=cache", 5),
    (
        "earliest=\"2024-01-15T10:00:00Z\" `OR.a`=x | stats count() as n by service | where n > 0",
        3,
    ),
];

/// What the live lane decides for one query: the filter's verdict on
/// each event, and the rows the whole plan emits, as text in a fixed
/// order.
#[derive(Debug, PartialEq)]
struct Outcome {
    verdicts: Vec<bool>,
    rows: Vec<Vec<(String, String)>>,
}

fn live(dsl: &str, events: &[Map<String, Value>]) -> Outcome {
    let query = parser::parse(dsl).unwrap_or_else(|error| panic!("{dsl:?}: {error:?}"));
    let filter = CompiledFilter::compile(&query.search, &FieldTypes::new())
        .unwrap_or_else(|error| panic!("{dsl:?} filter compiles: {error:?}"));
    let plan = compile_stream_plan(&query.pipeline, &PinScope::unpinned(), None)
        .unwrap_or_else(|error| panic!("{dsl:?} plan compiles: {error:?}"));
    let anchor = anchor();

    let verdicts = events
        .iter()
        .map(|event| filter.matches_at(event, &anchor))
        .collect();

    let emitted: Vec<Row> = match plan {
        StreamPlan::PassThrough(mut stages) => events
            .iter()
            .filter_map(
                |event| match stream::accept_event(&filter, &mut stages, event, &anchor) {
                    LiveOutcome::Emit(row) => Some(row),
                    LiveOutcome::Filtered | LiveOutcome::Done => None,
                },
            )
            .collect(),
        StreamPlan::Aggregate {
            mut pre_stages,
            mut aggregation,
            mut post_stages,
        } => {
            for event in events
                .iter()
                .filter(|event| filter.matches_at(event, &anchor))
            {
                let mut row = row::from_json(event);
                if pre_stages
                    .iter_mut()
                    .all(|stage| apply_stage(stage, &mut row, &anchor) == StageResult::Pass)
                {
                    aggregation.feed_event(&row, &anchor);
                }
            }
            aggregation
                .snapshot()
                .1
                .into_iter()
                .filter_map(|mut row| {
                    post_stages
                        .iter_mut()
                        .all(|stage| apply_stage(stage, &mut row, &anchor) == StageResult::Pass)
                        .then_some(row)
                })
                .collect()
        }
    };

    let mut rows: Vec<Vec<(String, String)>> = emitted
        .iter()
        .map(|row| {
            row.iter()
                .map(|(name, cell)| (name.clone(), row::cell_text(cell)))
                .collect()
        })
        .collect();
    rows.sort();
    Outcome { verdicts, rows }
}

/// The rows a query emitted: its row count, or the sum of its `n` cells
/// when it ends in `stats count() as n`.
fn emitted(dsl: &str, outcome: &Outcome) -> usize {
    outcome
        .rows
        .iter()
        .map(
            |row| match row.iter().find(|(name, _)| name == "n").map(|(_, n)| n) {
                Some(n) => n
                    .parse::<usize>()
                    .unwrap_or_else(|_| panic!("{dsl:?}: count {n:?}")),
                None => 1,
            },
        )
        .sum()
}

#[test]
fn formatting_preserves_live_verdicts() {
    let events = events();
    let mut failures = Vec::new();
    for &(query, expected_rows) in FIXTURES {
        let parsed = parser::parse(query).unwrap_or_else(|error| panic!("{query:?}: {error:?}"));
        let formatted = checked_format_query(&parsed)
            .unwrap_or_else(|error| panic!("{query:?} formats: {error:?}"));

        let before = live(query, &events);
        let after = live(&formatted, &events);

        let counted = emitted(query, &before);
        if counted != expected_rows {
            failures.push(format!(
                "{query:?} emits {counted} rows, expected {expected_rows}"
            ));
        }
        if before.verdicts != after.verdicts {
            failures.push(format!(
                "{query:?} formatted as {formatted:?} changes the filter verdicts: \
                 {:?} then {:?}",
                before.verdicts, after.verdicts
            ));
        }
        if before.rows != after.rows {
            failures.push(format!(
                "{query:?} formatted as {formatted:?} changes the emitted rows"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
