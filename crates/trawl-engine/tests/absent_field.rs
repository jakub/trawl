// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The absent-field oracle (ADR-0062).
//!
//! A field the current stage's input does not carry reads as NULL, in every
//! lane. Every rule that holds for a NULL value then applies unchanged: the
//! search stage's `!=`, negated patterns and negated lists match, positive
//! comparisons are unknown, `NOT` keeps three-valued logic, `where` stays
//! strict, and an absent field never removes a sibling `OR` branch's rows.
//!
//! One corpus of six events ([`EVENTS`]) runs through every case. In the
//! base corpus no event carries `f` or `g`. A [`Split`] decides where each
//! event lives: all in cold parquet, all in the hot snapshot (the ADR-0008
//! cold-start route), or mixed. A [`Variant`] decides what the events carry
//! and how the query reads them: an explicit null, `f` present in one cold
//! file or only in hot, a catalog pin with no physical column, and a query
//! window that includes or excludes the cold file carrying `f`.
//!
//! Each [`Case`] states its answer per variant: exact ids, or typed cells
//! for the stages that write a column. Batch runs once per split through
//! the public executor entry points. The live lane runs the same events
//! once per variant through `CompiledFilter` and the `StreamPlan`, as the
//! SSE handler does, and must give the same stated answer. Two lanes can
//! agree on zero rows, so neither lane is the reference: the stated answer
//! is, taken from ADR-0062's rule table.
//!
//! A batch result with no columns at all is never a correct answer here.
//! Every split holds data, so a column-less result is an empty success
//! standing in for an answer: the fallback ADR-0062 deletes.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value as Json};
use trawl_core::context::EvalContext;
use trawl_core::eval::EvalValue;
use trawl_core::filter::CompiledFilter;
use trawl_core::pin_scope::PinScope;
use trawl_core::row::Row;
use trawl_core::schema::{CanonicalType, FieldTypes};
use trawl_core::stream::{
    LiveOutcome, SnapshotContext, StreamPlan, accept_event, accept_event_into_aggregate,
    compile_stream_plan, emit_snapshot,
};
use trawl_engine::error::EngineError;
use trawl_engine::executor::Executor;
use trawl_engine::value::{ColumnType, QueryResult, Value};

// ── the corpus ──────────────────────────────────────────────────────

/// Where an event lives in the [`Split::Mixed`] corpus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Home {
    /// The cold file of hour 10, the oldest. The window variants carry `f`
    /// here and include or exclude this hour.
    Hour10,
    /// The cold file of hour 11.
    Hour11,
    /// The hot snapshot. [`Split::ColdOnly`] puts these events in the cold
    /// file of hour 12 instead.
    Hot,
}

/// One event of the corpus. `message` (and `_raw`, which repeats it) is
/// what a text search reads and what `extract kv` splits.
#[derive(Debug)]
struct Event {
    id: i64,
    host: &'static str,
    home: Home,
    message: &'static str,
}

/// The corpus. Every case's answer is stated over these ids, so read this
/// table next to the cases: web1 is ids 1, 3 and 5, `login` appears in the
/// messages of ids 1, 2 and 5, and the kv pairs are `user` on 1, 2 and 5
/// and `svc` on 3.
const EVENTS: [Event; 6] = [
    Event {
        id: 1,
        host: "web1",
        home: Home::Hour10,
        message: "login ok user=alice",
    },
    Event {
        id: 2,
        host: "web2",
        home: Home::Hour10,
        message: "login failed user=bob",
    },
    Event {
        id: 3,
        host: "web1",
        home: Home::Hour11,
        message: "timeout svc=api",
    },
    Event {
        id: 4,
        host: "web3",
        home: Home::Hour11,
        message: "cache miss",
    },
    Event {
        id: 5,
        host: "web1",
        home: Home::Hot,
        message: "login ok user=carol",
    },
    Event {
        id: 6,
        host: "web2",
        home: Home::Hot,
        message: "disk full",
    },
];

/// The cold hours a corpus lays out, one directory each.
const COLD_HOURS: [u32; 3] = [10, 11, 12];

impl Event {
    /// The hour this event's `_time` falls in. The hot events are the
    /// newest, at hour 12, whichever split stores them.
    fn hour(&self) -> u32 {
        match self.home {
            Home::Hour10 => 10,
            Home::Hour11 => 11,
            Home::Hot => 12,
        }
    }

    /// `_time` and `_ingested` as the wire's RFC 3339 text: hour by home,
    /// the id as the second, so every instant is distinct.
    fn instant(&self) -> String {
        format!("2024-01-15T{:02}:00:{:02}Z", self.hour(), self.id)
    }

    /// The same instant as a `DuckDB` TIMESTAMP literal, for cold parquet.
    fn timestamp_sql(&self) -> String {
        format!(
            "TIMESTAMP '2024-01-15 {:02}:00:{:02}'",
            self.hour(),
            self.id
        )
    }

    /// The event as the live lane and the hot snapshot see it: JSON, with
    /// `f` carried only where the variant puts it.
    fn json(&self, variant: Variant) -> Map<String, Json> {
        let mut event = Map::new();
        event.insert("_time".into(), Json::from(self.instant()));
        event.insert("_ingested".into(), Json::from(self.instant()));
        event.insert("id".into(), Json::from(self.id));
        event.insert("host".into(), Json::from(self.host));
        event.insert("message".into(), Json::from(self.message));
        event.insert("_raw".into(), Json::from(self.message));
        match variant.f(self.id) {
            F::Absent => {}
            F::Null => {
                event.insert("f".into(), Json::Null);
            }
            F::Text(text) => {
                event.insert("f".into(), Json::from(text));
            }
        }
        event
    }
}

/// What one event carries under `f`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum F {
    /// No key at all.
    Absent,
    /// An explicit JSON null (a NULL cell in cold parquet).
    Null,
    Text(&'static str),
}

/// Where the events live. The answer never depends on the split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Split {
    /// Every event in cold parquet, read with no hot snapshot
    /// (`Executor::run_query`).
    ColdOnly,
    /// Every event in the hot snapshot, with a cold source that reaches no
    /// file: the ADR-0008 cold-start route.
    HotOnly,
    /// Hours 10 and 11 in cold parquet, the rest in the hot snapshot.
    Mixed,
}

/// Where a split stores one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Cold(u32),
    Hot,
}

impl Split {
    const ALL: [Self; 3] = [Self::ColdOnly, Self::HotOnly, Self::Mixed];

    fn name(self) -> &'static str {
        match self {
            Self::ColdOnly => "cold-only",
            Self::HotOnly => "hot-only",
            Self::Mixed => "mixed",
        }
    }

    fn place(self, event: &Event) -> Place {
        match (self, event.home) {
            (Self::HotOnly, _) | (Self::Mixed, Home::Hot) => Place::Hot,
            (Self::ColdOnly | Self::Mixed, _) => Place::Cold(event.hour()),
        }
    }
}

/// What the events carry, and how the query reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    /// No event carries `f`.
    Absent,
    /// Every event carries an explicit null `f`. Cold parquet holds it as
    /// an all-NULL VARCHAR column; the hot snapshot as JSON `null`, a key
    /// `read_json` can only infer as JSON.
    ExplicitNull,
    /// No event carries `f`, and the catalog pins it VARCHAR.
    PinnedVarchar,
    /// No event carries `f`, and the catalog pins it DOUBLE. It runs only
    /// the cases whose literals a DOUBLE column accepts: an all-NULL DOUBLE
    /// column that is present already refuses `f=x` on main ("Could not
    /// convert string 'x' to DOUBLE"). That is pin typing, not absence.
    PinnedDouble,
    /// `f` is present in the hour-10 cold file only: id 1 `b2`, id 2 `x`.
    /// The hot-only split stores those events hot, so there it is a
    /// partial presence in hot.
    ColdFileOnly,
    /// `f` is present in hot only: id 5 `a1`, id 6 `x`. The cold-only
    /// split stores those events in the hour-12 file.
    HotOnlyPresence,
    /// [`Self::ColdFileOnly`]'s events under a window that includes the
    /// hour-10 file: `earliest=` at 10:00, and the cold source lists hours
    /// 10 to 12.
    WindowIncludes,
    /// The same events under a window that excludes the hour-10 file:
    /// `earliest=` at 11:00, and the cold source lists hours 11 and 12
    /// only, as the server narrows it. No selected source carries `f`.
    WindowExcludes,
}

impl Variant {
    fn name(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::ExplicitNull => "explicit-null",
            Self::PinnedVarchar => "pinned-varchar",
            Self::PinnedDouble => "pinned-double",
            Self::ColdFileOnly => "cold-file-only",
            Self::HotOnlyPresence => "hot-only-presence",
            Self::WindowIncludes => "window-includes",
            Self::WindowExcludes => "window-excludes",
        }
    }

    /// What event `id` carries under `f`.
    fn f(self, id: i64) -> F {
        match (self, id) {
            (Self::ExplicitNull, _) => F::Null,
            (Self::ColdFileOnly | Self::WindowIncludes | Self::WindowExcludes, 1) => F::Text("b2"),
            (Self::ColdFileOnly | Self::WindowIncludes | Self::WindowExcludes, 2)
            | (Self::HotOnlyPresence, 6) => F::Text("x"),
            (Self::HotOnlyPresence, 5) => F::Text("a1"),
            _ => F::Absent,
        }
    }

    /// The catalog pin snapshot: the comparison pins of the query.
    fn pins(self) -> FieldTypes {
        let mut pins = FieldTypes::new();
        match self {
            Self::PinnedVarchar => pins.insert("f", CanonicalType::Varchar),
            Self::PinnedDouble => pins.insert("f", CanonicalType::Double),
            _ => {}
        }
        pins
    }

    /// The case's query text under this variant's window.
    fn query(self, dsl: &str) -> String {
        let earliest = match self {
            Self::WindowIncludes => "2024-01-15T10:00:00Z",
            Self::WindowExcludes => "2024-01-15T11:00:00Z",
            _ => return dsl.to_owned(),
        };
        format!("earliest=\"{earliest}\" {dsl}")
    }

    /// The cold hours the source lists.
    fn cold_hours(self) -> &'static [u32] {
        match self {
            Self::WindowExcludes => &COLD_HOURS[1..],
            _ => &COLD_HOURS,
        }
    }

    /// The splits this variant runs over. A window over cold files means
    /// nothing when no event is cold.
    fn splits(self) -> &'static [Split] {
        match self {
            Self::WindowIncludes | Self::WindowExcludes => &[Split::ColdOnly, Split::Mixed],
            _ => &Split::ALL,
        }
    }
}

/// The variants in which no event carries a non-null `f`.
const NO_F: &[Variant] = &[
    Variant::Absent,
    Variant::ExplicitNull,
    Variant::PinnedVarchar,
    Variant::PinnedDouble,
];
/// [`NO_F`] without [`Variant::PinnedDouble`], for a case that a present
/// all-NULL DOUBLE column already refuses on main: a string literal it
/// cannot convert, or a string-only function.
const NO_F_TEXT: &[Variant] = &[
    Variant::Absent,
    Variant::ExplicitNull,
    Variant::PinnedVarchar,
];
/// The variants with `f` on ids 1 (`b2`) and 2 (`x`), both in the window.
const COLD_F: &[Variant] = &[Variant::ColdFileOnly, Variant::WindowIncludes];
/// The variant with `f` on ids 5 (`a1`) and 6 (`x`).
const HOT_F: &[Variant] = &[Variant::HotOnlyPresence];
/// The window that leaves ids 1 and 2, and with them every `f`, out.
const OUT: &[Variant] = &[Variant::WindowExcludes];

/// One corpus on disk: a split of the events under one variant.
struct Corpus {
    /// Owns the files; dropped with the corpus.
    _dir: tempfile::TempDir,
    /// The cold source in the server's list shape, one hour directory per
    /// element. An hour with no file is narrowed away at resolution, and a
    /// list that reaches no file at all is the cold start.
    cold: String,
    /// The hot snapshot, when the split stores any event hot.
    hot: Option<PathBuf>,
    /// The keys the hot snapshot carries, to cut the hot pins from.
    hot_keys: Vec<String>,
}

impl Corpus {
    fn build(variant: Variant, split: Split) -> Self {
        let dir = tempfile::tempdir().expect("corpus tempdir");
        let conn = duckdb::Connection::open_in_memory().expect("fixture database");
        for hour in COLD_HOURS {
            let hour_dir = dir.path().join("cold").join(hour.to_string());
            std::fs::create_dir_all(&hour_dir).expect("hour directory");
            let events: Vec<&Event> = EVENTS
                .iter()
                .filter(|event| split.place(event) == Place::Cold(hour))
                .collect();
            if !events.is_empty() {
                write_cold_file(&conn, &hour_dir.join("part.parquet"), &events, variant);
            }
        }

        let hot_events: Vec<Map<String, Json>> = EVENTS
            .iter()
            .filter(|event| split.place(event) == Place::Hot)
            .map(|event| event.json(variant))
            .collect();
        let mut hot_keys: Vec<String> = hot_events
            .iter()
            .flat_map(|event| event.keys().cloned())
            .collect();
        hot_keys.sort();
        hot_keys.dedup();
        let hot = (!hot_events.is_empty()).then(|| {
            let path = dir.path().join("hot.ndjson");
            let mut text = String::new();
            for event in &hot_events {
                text.push_str(&Json::Object(event.clone()).to_string());
                text.push('\n');
            }
            std::fs::write(&path, text).expect("hot snapshot");
            path
        });

        let cold = format!(
            "[{}]",
            variant
                .cold_hours()
                .iter()
                .map(|hour| format!("'{}/cold/{hour}/*.parquet'", dir.path().display()))
                .collect::<Vec<_>>()
                .join(", ")
        );
        Self {
            _dir: dir,
            cold,
            hot,
            hot_keys,
        }
    }

    /// Run `dsl` through the batch lane this corpus calls for: the hot
    /// entry point when the split stores events hot, the cold one
    /// otherwise. `pins` is the catalog snapshot; the hot pins are the
    /// pins the snapshot carries a key for, as the server cuts them.
    fn run(
        &self,
        exec: &Executor,
        dsl: &str,
        pins: &FieldTypes,
    ) -> Result<QueryResult, EngineError> {
        match &self.hot {
            None => exec.run_query(dsl, &self.cold, pins, usize::MAX, 0),
            Some(hot) => {
                let mut hot_pins = FieldTypes::new();
                for (field, ty) in pins.iter() {
                    if self.hot_keys.iter().any(|key| key == field) {
                        hot_pins.insert(field, ty);
                    }
                }
                exec.run_query_with_hot(
                    dsl,
                    &self.cold,
                    hot.to_str().expect("utf-8 hot path"),
                    &hot_pins,
                    pins,
                    usize::MAX,
                    0,
                )
            }
        }
    }
}

/// Write one cold parquet file. `f` is a VARCHAR column only when some
/// event of the file carries it, even as null. A file in which no event
/// carries `f` has no `f` column at all.
fn write_cold_file(conn: &duckdb::Connection, path: &Path, events: &[&Event], variant: Variant) {
    let with_f = events.iter().any(|event| variant.f(event.id) != F::Absent);
    let rows: Vec<String> = events
        .iter()
        .map(|event| {
            let f = match variant.f(event.id) {
                F::Absent | F::Null => "CAST(NULL AS VARCHAR)".to_owned(),
                F::Text(text) => format!("CAST('{text}' AS VARCHAR)"),
            };
            let mut row = format!(
                "({ts}, {ts}, CAST({id} AS BIGINT), '{host}', '{message}', '{message}'",
                ts = event.timestamp_sql(),
                id = event.id,
                host = event.host,
                message = event.message,
            );
            if with_f {
                row.push_str(", ");
                row.push_str(&f);
            }
            row.push(')');
            row
        })
        .collect();
    let columns = if with_f {
        "_time, _ingested, id, host, message, _raw, f"
    } else {
        "_time, _ingested, id, host, message, _raw"
    };
    conn.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES {}) AS t({columns})) TO '{}' (FORMAT PARQUET)",
        rows.join(", "),
        path.display()
    ))
    .expect("cold parquet file");
}

// ── answers ─────────────────────────────────────────────────────────

/// One typed cell, as both lanes are read back.
#[derive(Debug, Clone, PartialEq)]
enum Cell {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// Anything else, as its debug text. No case expects one.
    Other(String),
}

fn s(text: &str) -> Cell {
    Cell::Str(text.to_owned())
}

impl From<&Value> for Cell {
    fn from(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Boolean(b) => Self::Bool(*b),
            Value::Integer(n) => Self::Int(*n),
            Value::Float(f) => Self::Float(*f),
            Value::String(text) => Self::Str(text.clone()),
            other => Self::Other(format!("{other:?}")),
        }
    }
}

impl From<&EvalValue> for Cell {
    fn from(value: &EvalValue) -> Self {
        match value {
            EvalValue::Null => Self::Null,
            EvalValue::Bool(b) => Self::Bool(*b),
            EvalValue::Int(n) => Self::Int(*n),
            EvalValue::Float(f) => Self::Float(*f),
            EvalValue::Str(text) => Self::Str(text.clone()),
            other => Self::Other(format!("{other:?}")),
        }
    }
}

/// What a case expects from one variant.
#[derive(Debug, Clone)]
enum Answer {
    /// The ids of the rows returned, in any order.
    Ids(Vec<i64>),
    /// Typed cells.
    Rows(Rows),
}

/// The typed-cell answer of a stage that writes a column.
#[derive(Debug, Clone)]
struct Rows {
    /// The columns read from every output row. A live row that lacks one
    /// reads it as NULL; a batch result that lacks one is wrong.
    columns: Vec<&'static str>,
    /// The cells of every output row, over `columns`, in any row order.
    cells: Vec<Vec<Cell>>,
    /// Batch result types, per column.
    types: Vec<(&'static str, ColumnType)>,
    /// The batch result's whole column list, in order, when the stage
    /// fixes it.
    exact_columns: Option<Vec<&'static str>>,
    /// Names no output row may carry, in either lane.
    gone: Vec<&'static str>,
}

impl Rows {
    fn new(columns: &[&'static str], rows: Vec<Vec<Cell>>) -> Self {
        Self {
            columns: columns.to_vec(),
            cells: rows,
            types: Vec::new(),
            exact_columns: None,
            gone: Vec::new(),
        }
    }

    fn typed(mut self, column: &'static str, ty: ColumnType) -> Self {
        self.types.push((column, ty));
        self
    }

    fn exactly(mut self, columns: &[&'static str]) -> Self {
        self.exact_columns = Some(columns.to_vec());
        self
    }

    fn without(mut self, column: &'static str) -> Self {
        self.gone.push(column);
        self
    }
}

/// `[id, cell]` rows from `(id, cell)` pairs.
fn by_id(cells: &[(i64, Cell)]) -> Vec<Vec<Cell>> {
    cells
        .iter()
        .map(|(id, cell)| vec![Cell::Int(*id), cell.clone()])
        .collect()
}

/// Every id in `ids` with a NULL cell.
fn nulls(ids: &[i64]) -> Vec<(i64, Cell)> {
    ids.iter().map(|id| (*id, Cell::Null)).collect()
}

/// Every id in `ids` with the same cell.
fn each(ids: &[i64], cell: &Cell) -> Vec<(i64, Cell)> {
    ids.iter().map(|id| (*id, cell.clone())).collect()
}

/// `f` as the [`COLD_F`] variants carry it, over every id.
fn cold_f(fill: &Cell) -> Vec<(i64, Cell)> {
    let mut cells = vec![(1, s("b2")), (2, s("x"))];
    cells.extend(each(&[3, 4, 5, 6], fill));
    cells
}

/// `f` as the [`HOT_F`] variant carries it, over every id.
fn hot_f(fill: &Cell) -> Vec<(i64, Cell)> {
    let mut cells = each(&[1, 2, 3, 4], fill);
    cells.extend([(5, s("a1")), (6, s("x"))]);
    cells
}

/// `[host, cell]` rows from `(id, cell)` pairs, the host read off
/// [`EVENTS`].
fn host_rows(cells: &[(i64, Cell)]) -> Vec<Vec<Cell>> {
    cells
        .iter()
        .map(|(id, cell)| {
            let event = EVENTS
                .iter()
                .find(|event| event.id == *id)
                .expect("a corpus id");
            vec![s(event.host), cell.clone()]
        })
        .collect()
}

const ALL: &[i64] = &[1, 2, 3, 4, 5, 6];
/// The ids the window that excludes hour 10 keeps.
const LATE: &[i64] = &[3, 4, 5, 6];

/// One query and its answer per variant.
#[derive(Debug)]
struct Case {
    dsl: &'static str,
    answers: Vec<(&'static [Variant], Answer)>,
}

impl Case {
    fn new(dsl: &'static str) -> Self {
        Self {
            dsl,
            answers: Vec::new(),
        }
    }

    fn ids(mut self, variants: &'static [Variant], ids: &[i64]) -> Self {
        self.answers.push((variants, Answer::Ids(ids.to_vec())));
        self
    }

    fn rows(mut self, variants: &'static [Variant], rows: Rows) -> Self {
        self.answers.push((variants, Answer::Rows(rows)));
        self
    }
}

// ── the lanes ───────────────────────────────────────────────────────

/// Run the case over every variant it names: batch over each of the
/// variant's splits, live once. Every mismatch is collected, so one
/// failing assertion reports the whole matrix.
fn check(case: &Case) {
    let exec = Executor::new().expect("executor");
    let mut failed = Vec::new();
    let mut passed = Vec::new();
    for (variants, answer) in &case.answers {
        for &variant in *variants {
            let dsl = variant.query(case.dsl);
            let pins = variant.pins();

            let label = format!("{}/live", variant.name());
            match live_rows(&dsl, variant, &pins).and_then(|rows| judge_live(answer, &rows)) {
                Ok(()) => passed.push(label),
                Err(why) => failed.push(format!("{label}: {why}")),
            }

            for &split in variant.splits() {
                let label = format!("{}/{}", variant.name(), split.name());
                let corpus = Corpus::build(variant, split);
                match judge_batch(answer, corpus.run(&exec, &dsl, &pins)) {
                    Ok(()) => passed.push(label),
                    Err(why) => failed.push(format!("{label}: {why}")),
                }
            }
        }
    }
    assert!(
        failed.is_empty(),
        "case `{}`: {} of {} checks failed\n  {}\npassed: {}",
        case.dsl,
        failed.len(),
        failed.len() + passed.len(),
        failed.join("\n  "),
        if passed.is_empty() {
            "none".to_owned()
        } else {
            passed.join(", ")
        }
    );
}

/// The live lane over the variant's events: the search-stage filter and
/// the stream plan under one catalog snapshot, as the SSE handler compiles
/// them, one evaluation context for the whole run.
fn live_rows(dsl: &str, variant: Variant, pins: &FieldTypes) -> Result<Vec<Row>, String> {
    let query = trawl_core::parser::parse(dsl).map_err(|errors| format!("parse: {errors:?}"))?;
    let filter = CompiledFilter::compile(&query.search, pins)
        .map_err(|error| format!("filter refused: {error}"))?;
    let plan = compile_stream_plan(&query.pipeline, &PinScope::root(pins), None)
        .map_err(|error| format!("plan refused: {error}"))?;
    let ctx = EvalContext::capture();
    let events: Vec<Map<String, Json>> = EVENTS.iter().map(|event| event.json(variant)).collect();
    Ok(match plan {
        StreamPlan::PassThrough(mut stages) => {
            let mut rows = Vec::new();
            for event in &events {
                match accept_event(&filter, &mut stages, event, &ctx) {
                    LiveOutcome::Emit(row) => rows.push(row),
                    LiveOutcome::Filtered => {}
                    LiveOutcome::Done => break,
                }
            }
            rows
        }
        StreamPlan::Aggregate {
            mut pre_stages,
            mut aggregation,
            mut post_stages,
        } => {
            for event in &events {
                accept_event_into_aggregate(
                    &filter,
                    &mut pre_stages,
                    &mut aggregation,
                    event,
                    &ctx,
                );
            }
            emit_snapshot(&aggregation, &mut post_stages, &SnapshotContext::new(ctx)).1
        }
    })
}

fn judge_live(answer: &Answer, rows: &[Row]) -> Result<(), String> {
    match answer {
        Answer::Ids(want) => {
            let got = rows
                .iter()
                .map(|row| match row.get("id") {
                    Some(EvalValue::Int(id)) => Ok(*id),
                    other => Err(format!("a row with id {other:?}")),
                })
                .collect::<Result<Vec<i64>, String>>()?;
            compare_ids(want, got)
        }
        Answer::Rows(spec) => {
            for name in &spec.gone {
                if rows.iter().any(|row| row.contains_key(*name)) {
                    return Err(format!("expected no `{name}` key, got rows {rows:?}"));
                }
            }
            let got = rows
                .iter()
                .map(|row| {
                    spec.columns
                        .iter()
                        .map(|name| row.get(*name).map_or(Cell::Null, Cell::from))
                        .collect()
                })
                .collect();
            compare_rows(&spec.cells, got)
        }
    }
}

fn judge_batch(answer: &Answer, outcome: Result<QueryResult, EngineError>) -> Result<(), String> {
    let result =
        outcome.map_err(|error| format!("expected {}, got error: {error}", describe(answer)))?;
    if result.columns.is_empty() {
        return Err(format!(
            "expected {}, got the column-less empty result",
            describe(answer)
        ));
    }
    let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    let index = |name: &str| names.iter().position(|n| *n == name);
    match answer {
        Answer::Ids(want) => {
            let column = index("id").ok_or_else(|| format!("no `id` column in {names:?}"))?;
            let got = result
                .rows
                .iter()
                .map(|row| match &row[column] {
                    Value::Integer(id) => Ok(*id),
                    other => Err(format!("a row with id {other:?}")),
                })
                .collect::<Result<Vec<i64>, String>>()?;
            compare_ids(want, got)
        }
        Answer::Rows(spec) => {
            if let Some(exact) = &spec.exact_columns
                && names != *exact
            {
                return Err(format!("expected columns {exact:?}, got {names:?}"));
            }
            for name in &spec.gone {
                if index(name).is_some() {
                    return Err(format!("expected no `{name}` column, got {names:?}"));
                }
            }
            for (name, want) in &spec.types {
                let got = result
                    .columns
                    .iter()
                    .find(|c| c.name == *name)
                    .map(|c| c.ty.name());
                if got != Some(want.name()) {
                    return Err(format!(
                        "expected `{name}` typed {}, got {got:?} in {:?}",
                        want.name(),
                        result
                            .columns
                            .iter()
                            .map(|c| format!("{}:{}", c.name, c.ty.name()))
                            .collect::<Vec<_>>()
                    ));
                }
            }
            let columns = spec
                .columns
                .iter()
                .map(|name| index(name).ok_or_else(|| format!("no `{name}` column in {names:?}")))
                .collect::<Result<Vec<usize>, String>>()?;
            let got = result
                .rows
                .iter()
                .map(|row| columns.iter().map(|&i| Cell::from(&row[i])).collect())
                .collect();
            compare_rows(&spec.cells, got)
        }
    }
}

fn describe(answer: &Answer) -> String {
    match answer {
        Answer::Ids(ids) => format!("ids {ids:?}"),
        Answer::Rows(spec) => format!("rows {:?} = {:?}", spec.columns, sorted(spec.cells.clone())),
    }
}

fn compare_ids(want: &[i64], mut got: Vec<i64>) -> Result<(), String> {
    got.sort_unstable();
    let mut want = want.to_vec();
    want.sort_unstable();
    if got == want {
        Ok(())
    } else {
        Err(format!("expected ids {want:?}, got ids {got:?}"))
    }
}

fn sorted(mut rows: Vec<Vec<Cell>>) -> Vec<Vec<Cell>> {
    rows.sort_by_key(|row| format!("{row:?}"));
    rows
}

fn compare_rows(want: &[Vec<Cell>], got: Vec<Vec<Cell>>) -> Result<(), String> {
    let want = sorted(want.to_vec());
    let got = sorted(got);
    if got == want {
        Ok(())
    } else {
        Err(format!("expected rows {want:?}, got rows {got:?}"))
    }
}

// ── AC 1: every read of an absent field is a NULL read ─────────────

/// The search-stage operators, the boolean compositions, the strict
/// `where`, and the stages that write a column. Each case runs over every
/// variant whose column types it is valid for, and over each of that
/// variant's splits.
mod oracle {
    use super::*;

    #[test]
    fn eq() {
        check(
            &Case::new("f=x")
                .ids(NO_F_TEXT, &[])
                .ids(COLD_F, &[2])
                .ids(HOT_F, &[6])
                .ids(OUT, &[]),
        );
    }

    #[test]
    fn ne() {
        check(
            &Case::new("f!=x")
                .ids(NO_F, ALL)
                .ids(COLD_F, &[1, 3, 4, 5, 6])
                .ids(HOT_F, &[1, 2, 3, 4, 5])
                .ids(OUT, LATE),
        );
    }

    #[test]
    fn ne_any() {
        check(
            &Case::new("f!=*")
                .ids(NO_F, ALL)
                .ids(COLD_F, &[3, 4, 5, 6])
                .ids(HOT_F, &[1, 2, 3, 4])
                .ids(OUT, LATE),
        );
    }

    #[test]
    fn eq_any() {
        check(
            &Case::new("f=*")
                .ids(NO_F, &[])
                .ids(COLD_F, &[1, 2])
                .ids(HOT_F, &[5, 6])
                .ids(OUT, &[]),
        );
    }

    #[test]
    fn ne_glob() {
        check(
            &Case::new("f!=a*")
                .ids(NO_F, ALL)
                .ids(COLD_F, ALL)
                .ids(HOT_F, &[1, 2, 3, 4, 6])
                .ids(OUT, LATE),
        );
    }

    #[test]
    fn ne_regex() {
        check(
            &Case::new("f!=/^b/")
                .ids(NO_F, ALL)
                .ids(COLD_F, &[2, 3, 4, 5, 6])
                .ids(HOT_F, ALL)
                .ids(OUT, LATE),
        );
    }

    #[test]
    fn eq_list() {
        check(
            &Case::new("f=a,b*")
                .ids(NO_F_TEXT, &[])
                .ids(COLD_F, &[1])
                .ids(HOT_F, &[])
                .ids(OUT, &[]),
        );
    }

    #[test]
    fn ne_list() {
        check(
            &Case::new("f!=a,b*")
                .ids(NO_F, ALL)
                .ids(COLD_F, &[2, 3, 4, 5, 6])
                .ids(HOT_F, ALL)
                .ids(OUT, LATE),
        );
    }

    /// Only where no selected source holds `f` as text: a present VARCHAR
    /// `f`, all-NULL or not, already refuses an ordered numeric literal on
    /// main ("Cannot compare values of type VARCHAR and type BIGINT"),
    /// which is column typing, not absence.
    #[test]
    fn gt() {
        check(
            &Case::new("f>1")
                .ids(
                    &[
                        Variant::Absent,
                        Variant::PinnedVarchar,
                        Variant::PinnedDouble,
                    ],
                    &[],
                )
                .ids(OUT, &[]),
        );
    }

    #[test]
    fn not_eq() {
        check(
            &Case::new("NOT f=x")
                .ids(NO_F_TEXT, &[])
                .ids(COLD_F, &[1])
                .ids(HOT_F, &[5])
                .ids(OUT, &[]),
        );
    }

    #[test]
    fn not_ne() {
        check(
            &Case::new("NOT f!=x")
                .ids(NO_F, &[])
                .ids(COLD_F, &[2])
                .ids(HOT_F, &[6])
                .ids(OUT, &[]),
        );
    }

    #[test]
    fn or_keeps_the_present_branch() {
        check(
            &Case::new("host=web1 OR f=x")
                .ids(NO_F_TEXT, &[1, 3, 5])
                .ids(COLD_F, &[1, 2, 3, 5])
                .ids(HOT_F, &[1, 3, 5, 6])
                .ids(OUT, &[3, 5]),
        );
    }

    #[test]
    fn or_keeps_the_present_branch_reversed() {
        check(
            &Case::new("f=x OR host=web1")
                .ids(NO_F_TEXT, &[1, 3, 5])
                .ids(COLD_F, &[1, 2, 3, 5])
                .ids(HOT_F, &[1, 3, 5, 6])
                .ids(OUT, &[3, 5]),
        );
    }

    #[test]
    fn and_with_a_present_field() {
        check(
            &Case::new("host=web1 f!=x")
                .ids(NO_F, &[1, 3, 5])
                .ids(COLD_F, &[1, 3, 5])
                .ids(HOT_F, &[1, 3, 5])
                .ids(OUT, &[3, 5]),
        );
    }

    #[test]
    fn text_search_with_ne() {
        check(
            &Case::new("login f!=x")
                .ids(NO_F, &[1, 2, 5])
                .ids(COLD_F, &[1, 5])
                .ids(HOT_F, &[1, 2, 5])
                .ids(OUT, &[5]),
        );
    }

    #[test]
    fn where_eq() {
        check(
            &Case::new(r#"* | where f=="x""#)
                .ids(NO_F_TEXT, &[])
                .ids(COLD_F, &[2])
                .ids(HOT_F, &[6])
                .ids(OUT, &[]),
        );
    }

    /// `where` is strict: a NULL `f` is not unequal to anything.
    #[test]
    fn where_ne() {
        check(
            &Case::new(r#"* | where f!="x""#)
                .ids(NO_F_TEXT, &[])
                .ids(COLD_F, &[1])
                .ids(HOT_F, &[5])
                .ids(OUT, &[]),
        );
    }

    #[test]
    fn where_isnull() {
        check(
            &Case::new("* | where isnull(f)")
                .ids(NO_F, ALL)
                .ids(COLD_F, &[3, 4, 5, 6])
                .ids(HOT_F, &[1, 2, 3, 4])
                .ids(OUT, LATE),
        );
    }

    #[test]
    fn where_coalesce() {
        check(
            &Case::new(r#"* | where coalesce(f,"d")=="d""#)
                .ids(NO_F_TEXT, ALL)
                .ids(COLD_F, &[3, 4, 5, 6])
                .ids(HOT_F, &[1, 2, 3, 4])
                .ids(OUT, LATE),
        );
    }

    /// A direct NULL of an absent field: VARCHAR when unpinned, the pin's
    /// type when pinned (ADR-0062).
    #[test]
    fn let_bare() {
        let rows = |cells: &[(i64, Cell)], ty: ColumnType| {
            Rows::new(&["id", "y"], by_id(cells)).typed("y", ty)
        };
        check(
            &Case::new("* | let y=f")
                .rows(NO_F_TEXT, rows(&nulls(ALL), ColumnType::String))
                .rows(
                    &[Variant::PinnedDouble],
                    rows(&nulls(ALL), ColumnType::Float),
                )
                .rows(COLD_F, rows(&cold_f(&Cell::Null), ColumnType::String))
                .rows(HOT_F, rows(&hot_f(&Cell::Null), ColumnType::String))
                .rows(OUT, rows(&nulls(LATE), ColumnType::String)),
        );
    }

    #[test]
    fn let_coalesce() {
        let fb = s("fb");
        let rows = |cells: &[(i64, Cell)]| {
            Rows::new(&["id", "y"], by_id(cells)).typed("y", ColumnType::String)
        };
        check(
            &Case::new(r#"* | let y=coalesce(f,"fb")"#)
                .rows(NO_F_TEXT, rows(&each(ALL, &fb)))
                .rows(COLD_F, rows(&cold_f(&fb)))
                .rows(HOT_F, rows(&hot_f(&fb)))
                .rows(OUT, rows(&each(LATE, &fb))),
        );
    }

    /// `table` materializes a requested absent name as a null column, in
    /// the requested order.
    #[test]
    fn table() {
        let rows = |cells: &[(i64, Cell)], ty: ColumnType| {
            Rows::new(&["host", "f"], host_rows(cells))
                .typed("f", ty)
                .exactly(&["host", "f"])
        };
        check(
            &Case::new("* | table host,f")
                .rows(NO_F_TEXT, rows(&nulls(ALL), ColumnType::String))
                .rows(
                    &[Variant::PinnedDouble],
                    rows(&nulls(ALL), ColumnType::Float),
                )
                .rows(COLD_F, rows(&cold_f(&Cell::Null), ColumnType::String))
                .rows(HOT_F, rows(&hot_f(&Cell::Null), ColumnType::String))
                .rows(OUT, rows(&nulls(LATE), ColumnType::String)),
        );
    }

    /// `rename` writes its target, NULL when the source is absent, and the
    /// source is gone.
    #[test]
    fn rename() {
        let rows = |cells: &[(i64, Cell)], ty: ColumnType| {
            Rows::new(&["id", "q"], by_id(cells))
                .typed("q", ty)
                .without("f")
        };
        check(
            &Case::new("* | rename f as q")
                .rows(NO_F_TEXT, rows(&nulls(ALL), ColumnType::String))
                .rows(
                    &[Variant::PinnedDouble],
                    rows(&nulls(ALL), ColumnType::Float),
                )
                .rows(COLD_F, rows(&cold_f(&Cell::Null), ColumnType::String))
                .rows(HOT_F, rows(&hot_f(&Cell::Null), ColumnType::String))
                .rows(OUT, rows(&nulls(LATE), ColumnType::String)),
        );
    }

    /// A regex `extract` writes its capture, NULL when the source is
    /// absent. Not under the DOUBLE pin: a present DOUBLE column is no
    /// `regexp_extract` argument on main either.
    #[test]
    fn extract_from_absent() {
        let rows = |cells: &[(i64, Cell)]| {
            Rows::new(&["id", "k"], by_id(cells)).typed("k", ColumnType::String)
        };
        check(
            &Case::new(r#"* | extract "(?P<k>\w+)" from f"#)
                .rows(NO_F_TEXT, rows(&nulls(ALL)))
                .rows(COLD_F, rows(&cold_f(&Cell::Null)))
                .rows(HOT_F, rows(&hot_f(&Cell::Null)))
                .rows(OUT, rows(&nulls(LATE))),
        );
    }

    /// The SQL prefix before the kv batch tail reads `f` as NULL, and the
    /// tail splits `message` as usual.
    #[test]
    fn ne_then_extract_kv() {
        let kv = |ids: &[i64]| {
            let rows = ids
                .iter()
                .map(|id| {
                    let (user, svc) = match id {
                        1 => (s("alice"), Cell::Null),
                        2 => (s("bob"), Cell::Null),
                        3 => (Cell::Null, s("api")),
                        5 => (s("carol"), Cell::Null),
                        _ => (Cell::Null, Cell::Null),
                    };
                    vec![Cell::Int(*id), user, svc]
                })
                .collect();
            Rows::new(&["id", "user", "svc"], rows)
        };
        check(
            &Case::new("f!=x | extract kv")
                .rows(NO_F, kv(ALL))
                .rows(COLD_F, kv(&[1, 3, 4, 5, 6]))
                .rows(HOT_F, kv(&[1, 2, 3, 4, 5]))
                .rows(OUT, kv(LATE)),
        );
    }
}
