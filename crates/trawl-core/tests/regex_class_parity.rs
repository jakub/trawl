// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Regex classes answer alike in the live lane and in batch SQL.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write as _;

use duckdb::Connection;
use serde_json::{Map, Value, json};
use trawl_core::context::EvalContext;
use trawl_core::emitter::{self, SqlValue};
use trawl_core::filter::CompiledFilter;
use trawl_core::pin_scope::PinScope;
use trawl_core::row;
use trawl_core::schema::FieldTypes;
use trawl_core::stream::{StageResult, StreamPlan, compile_stream_plan};

const SUBJECTS: [&str; 29] = [
    "\u{663}",
    "\u{e9}",
    "\u{200a}",
    "\u{b}",
    "\u{c}",
    "\u{212a}",
    "\u{17f}",
    "3",
    "e",
    "_",
    "K",
    "s",
    " ",
    "\t",
    "\n",
    "\r",
    "",
    "x\u{e9}y",
    "x\u{e9}",
    "\u{e9}x",
    "x\u{663}y",
    "x\u{212a}y",
    "x\u{17f}y",
    "\u{e9}\u{e9}",
    "a\n",
    "a\r\n",
    "x y",
    "Ka",
    "aK",
];

const EMPTY_NON_BOUNDARY_FORMS: [&str; 2] = [r"\B", r"x*\B"];

const SPLIT_WORD_SUBJECTS: [&str; 4] = ["x\u{e9}y", "x\u{663}y", "x\u{212a}y", "x\u{17f}y"];

fn case_forms(body: &str) -> [String; 3] {
    [
        body.to_owned(),
        format!("(?i){body}"),
        format!("(?i:{body}){body}"),
    ]
}

fn bodies() -> Vec<(String, String)> {
    let mut bodies = Vec::new();
    for class in [r"\d", r"\D", r"\w", r"\W", r"\s", r"\S"] {
        for form in [
            class.to_owned(),
            format!("^{class}$"),
            format!("{class}+"),
            format!("[{class}]"),
            format!("[x{class}]"),
            format!("[^{class}]"),
        ] {
            for body in case_forms(&form) {
                bodies.push((class.to_owned(), body));
            }
        }
    }
    for (family, forms) in [
        (r"\b", &[r"\b", r"x\b", r"\bx", r"\b\w+\b"][..]),
        (r"\B", &[r"\B", r"x\B", r"\Bx", r"x*\B"][..]),
    ] {
        for form in forms {
            for body in case_forms(form) {
                bodies.push((family.to_owned(), body));
            }
        }
    }
    for body in [r"(?i)k", r"(?i)s", r"(?i)\w", r"(?i)\W"] {
        bodies.push(("case fold".to_owned(), body.to_owned()));
    }
    for body in ["a$", "(?m)a$", "^.$", "(?s)^.$"] {
        bodies.push(("anchor and dot".to_owned(), body.to_owned()));
    }
    for body in [r"\pN", r"\p{Nd}", r"\pL"] {
        bodies.push(("Unicode class".to_owned(), body.to_owned()));
    }
    bodies
}

fn accepted_differences() -> BTreeSet<(String, &'static str)> {
    EMPTY_NON_BOUNDARY_FORMS
        .iter()
        .flat_map(|form| case_forms(form))
        .flat_map(|body| SPLIT_WORD_SUBJECTS.map(|subject| (body.clone(), subject)))
        .collect()
}

fn code_points(text: &str) -> String {
    if text.is_empty() {
        return "(empty)".to_owned();
    }
    text.chars()
        .map(|c| format!("U+{:04X}", u32::from(c)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn events(subjects: &[&str]) -> Vec<Map<String, Value>> {
    subjects
        .iter()
        .enumerate()
        .map(|(id, subject)| {
            let Value::Object(event) = json!({"id": id, "message": subject}) else {
                unreachable!("an object literal")
            };
            event
        })
        .collect()
}

fn source(events: &[Map<String, Value>]) -> tempfile::NamedTempFile {
    let mut tmp = tempfile::Builder::new()
        .suffix(".ndjson")
        .tempfile()
        .unwrap();
    for event in events {
        writeln!(tmp, "{}", Value::Object(event.clone())).unwrap();
    }
    tmp.flush().unwrap();
    tmp
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

fn batch_rows(
    conn: &Connection,
    dsl: &str,
    path: &str,
    columns: &[&str],
) -> BTreeMap<i64, Vec<Option<String>>> {
    let query = trawl_core::parser::parse(dsl).expect("dsl parses");
    let emitted = emitter::emit_with_pins(
        &query,
        path,
        &FieldTypes::new(),
        &emitter::InputColumns::unprobed(),
        EvalContext::capture(),
    )
    .unwrap_or_else(|e| panic!("{dsl:?} emits: {e}"));
    let projection = columns.iter().fold(String::new(), |mut projection, name| {
        write!(projection, r#", CAST("{name}" AS VARCHAR)"#).expect("writes to a String");
        projection
    });
    let sql = format!(
        r#"SELECT CAST("id" AS BIGINT){projection} FROM ({}) AS _sub"#,
        emitted.sql
    );
    let params = bind_params(&emitted.params);
    let refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let mut stmt = conn
        .prepare(&sql)
        .unwrap_or_else(|e| panic!("{dsl:?} prepares: {e}\n{sql}"));
    stmt.query_map(refs.as_slice(), |row| {
        let id: i64 = row.get(0)?;
        let cells = (1..=columns.len())
            .map(|i| row.get::<_, Option<String>>(i))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((id, cells))
    })
    .unwrap_or_else(|e| panic!("{dsl:?} runs: {e}"))
    .collect::<Result<_, _>>()
    .unwrap_or_else(|e| panic!("{dsl:?} reads: {e}"))
}

fn live_rows(
    dsl: &str,
    events: &[Map<String, Value>],
    columns: &[&str],
) -> BTreeMap<i64, Vec<Option<String>>> {
    let anchor = EvalContext::capture();
    let query = trawl_core::parser::parse(dsl).expect("dsl parses");
    let filter = CompiledFilter::compile(&query.search, &FieldTypes::new())
        .unwrap_or_else(|e| panic!("{dsl:?} filter compiles: {e}"));
    let StreamPlan::PassThrough(mut stages) =
        compile_stream_plan(&query.pipeline, &PinScope::unpinned(), None)
            .unwrap_or_else(|e| panic!("{dsl:?} plan compiles: {e}"))
    else {
        panic!("{dsl:?} is a pass-through plan");
    };
    let mut kept = BTreeMap::new();
    for event in events {
        if !filter
            .matches_at(event, &anchor)
            .expect("a never stop never cancels")
        {
            continue;
        }
        let mut row = row::from_json(event);
        let passed = stages.iter_mut().all(|stage| {
            trawl_core::stream::apply_stage(stage, &mut row, &anchor)
                .expect("a compiled plan prepares every pattern")
                == StageResult::Pass
        });
        if !passed {
            continue;
        }
        let id = event["id"].as_i64().expect("an integer id");
        let cells = columns
            .iter()
            .map(|name| {
                row.iter()
                    .find(|(field, _)| field.as_str() == *name)
                    .map(|(_, cell)| cell)
                    .filter(|cell| !matches!(cell, trawl_core::eval::EvalValue::Null))
                    .map(row::cell_text)
            })
            .collect();
        kept.insert(id, cells);
    }
    kept
}

#[test]
fn regex_class_lane_parity() {
    let conn = Connection::open_in_memory().unwrap();
    let events = events(&SUBJECTS);
    let file = source(&events);
    let path = file.path().to_str().unwrap();
    let accepted = accepted_differences();

    let mut mismatches = Vec::new();
    let mut seen: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut excluded = 0usize;
    for (family, body) in bodies() {
        for dsl in [
            format!("message=/{body}/"),
            format!("* | where message matches /{body}/"),
        ] {
            let batch = batch_rows(&conn, &dsl, path, &[]);
            let live = live_rows(&dsl, &events, &[]);
            for (id, subject) in SUBJECTS.iter().enumerate() {
                let id = i64::try_from(id).expect("a small index");
                let (in_batch, in_live) = (batch.contains_key(&id), live.contains_key(&id));
                if accepted.contains(&(body.clone(), *subject)) {
                    excluded += 1;
                    continue;
                }
                let counts = seen.entry(family.clone()).or_default();
                if in_batch {
                    counts.0 += 1;
                } else {
                    counts.1 += 1;
                }
                if in_batch != in_live {
                    mismatches.push(format!(
                        "{dsl} | subject {} | batch {in_batch} | live {in_live}",
                        code_points(subject)
                    ));
                }
            }
        }
    }
    assert_eq!(
        excluded,
        2 * accepted.len(),
        "each accepted row is skipped once a form"
    );
    for (family, (hits, misses)) in &seen {
        assert!(
            *hits > 0 && *misses > 0,
            "{family}: {hits} hits, {misses} misses"
        );
    }
    assert!(
        mismatches.is_empty(),
        "{} rows disagree:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

fn capture_subjects() -> Vec<&'static str> {
    let mut subjects = SUBJECTS.to_vec();
    subjects.extend(["ab 12", "e\u{e9} \u{663}3", "x_1 \u{200a}y"]);
    subjects
}

#[test]
fn regex_class_capture_parity() {
    let conn = Connection::open_in_memory().unwrap();
    let subjects = capture_subjects();
    let events = events(&subjects);
    let file = source(&events);
    let path = file.path().to_str().unwrap();

    let mut mismatches = Vec::new();
    let (mut present, mut null) = (0usize, 0usize);
    for (body, columns) in [
        (r"(?P<n>\d+)", &["n"][..]),
        (r"(?P<n>\D+)", &["n"]),
        (r"(\w)(?P<n>\s)", &["n"]),
        (r"(?P<n>\S+)", &["n"]),
        (r"(?P<a>\w+)\b(?P<b>\W*)", &["a", "b"]),
        (r"x(?P<n>\B)", &["n"]),
        (r"(?P<n>\b)", &["n"]),
        (r"(?P<a>\b\w)(?P<b>.)", &["a", "b"]),
        (r"(?i:(?P<n>\w))", &["n"]),
        (r"(?i)(?P<n>[\w\s]+)", &["n"]),
        (r"(?P<n>[^\w])", &["n"]),
        (r"(?P<a>\d+)?(?P<b>[a-z]+)", &["a", "b"]),
        (r"(\s)?(?P<n>\w+)", &["n"]),
        (r"(\w+)\s+(?P<num>\d+)", &["num"]),
        (r"(?P<word>\w+)(\s)(?P<rest>\S*)", &["word", "rest"]),
    ] {
        let dsl = format!(r#"* | extract "{body}" from message"#);
        let batch = batch_rows(&conn, &dsl, path, columns);
        let live = live_rows(&dsl, &events, columns);
        assert_eq!(
            batch.keys().collect::<Vec<_>>(),
            live.keys().collect::<Vec<_>>(),
            "{dsl}: extract keeps every row in both lanes"
        );
        for (id, cells) in &batch {
            let subject = subjects[usize::try_from(*id).expect("a small id")];
            for cell in cells {
                if cell.is_some() {
                    present += 1;
                } else {
                    null += 1;
                }
            }
            if live.get(id) != Some(cells) {
                mismatches.push(format!(
                    "{dsl} | columns {columns:?} | subject {} | batch {cells:?} | live {:?}",
                    code_points(subject),
                    live.get(id)
                ));
            }
        }
    }
    assert!(present > 0 && null > 0, "{present} present, {null} null");
    assert!(
        mismatches.is_empty(),
        "{} rows disagree:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}
