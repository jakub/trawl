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
//! and how the query reads them: an explicit null, as VARCHAR or as DOUBLE
//! for a numeric reader, `f` present in one cold file or only in hot, a
//! catalog pin with no physical column, and a query window that includes
//! or excludes the cold file carrying `f`.
//! The null-identity variant mixes the empty string, the text `null`, an
//! explicit null and absence under one name.
//!
//! Each [`Case`] states its answer per variant: exact ids, ids in order,
//! or typed cells for the stages that write a column. Batch runs once per
//! split through the public executor entry points. The live lane runs the
//! same events once per variant through `CompiledFilter` and the
//! `StreamPlan`, as the SSE handler does, and must give the same stated
//! answer; a stage the stream plan refuses runs batch only. Two lanes can
//! agree on zero rows, so neither lane is the reference: the stated answer
//! is, taken from ADR-0062 and the DSL reference's missing-field rules.
//!
//! Each module below is one acceptance criterion, named as its evidence
//! filter names it (`cargo test -p trawl-engine --test absent_field
//! <module>`).
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
use trawl_engine::executor::{Executor, hot_columns_of};
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
    service: &'static str,
    home: Home,
    message: &'static str,
}

/// The corpus. Every case's answer is stated over these ids, so read this
/// table next to the cases: web1 is ids 1, 3 and 5, `login` appears in the
/// messages of ids 1, 2 and 5, and the kv pairs are `user` on 1, 2 and 5
/// and `svc` on 3. Every event carries a `service`, so a projection that
/// names one present and one absent field has a column of each.
const EVENTS: [Event; 6] = [
    Event {
        id: 1,
        host: "web1",
        service: "auth",
        home: Home::Hour10,
        message: "login ok user=alice",
    },
    Event {
        id: 2,
        host: "web2",
        service: "auth",
        home: Home::Hour10,
        message: "login failed user=bob",
    },
    Event {
        id: 3,
        host: "web1",
        service: "api",
        home: Home::Hour11,
        message: "timeout svc=api",
    },
    Event {
        id: 4,
        host: "web3",
        service: "cache",
        home: Home::Hour11,
        message: "cache miss",
    },
    Event {
        id: 5,
        host: "web1",
        service: "auth",
        home: Home::Hot,
        message: "login ok user=carol",
    },
    Event {
        id: 6,
        host: "web2",
        service: "storage",
        home: Home::Hot,
        message: "disk full",
    },
];

/// The columns every event carries, whatever the variant: the input
/// columns a `*` over input that lacks `f` returns.
const INPUT_COLUMNS: &[&str] = &[
    "_time",
    "_ingested",
    "id",
    "host",
    "service",
    "message",
    "_raw",
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
        event.insert("service".into(), Json::from(self.service));
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
    /// [`Self::ExplicitNull`] for a numeric reader: cold parquet holds `f`
    /// as an all-NULL DOUBLE column, so `sum(f)` and its kind bind on it
    /// as they do on any present numeric column. The hot snapshot holds
    /// JSON `null`, as for [`Self::ExplicitNull`].
    NullDouble,
    /// The null identity's corpus: id 1 carries the empty string, id 2
    /// the text `null`, id 5 an explicit null, and ids 3, 4 and 6 no `f`
    /// at all. The hour-11 cold file (ids 3 and 4) has no `f` column, so
    /// in every split absent and explicit null meet in one relation.
    NullIdentity,
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
            Self::NullDouble => "null-double",
            Self::NullIdentity => "null-identity",
        }
    }

    /// What event `id` carries under `f`.
    fn f(self, id: i64) -> F {
        match (self, id) {
            (Self::ExplicitNull | Self::NullDouble, _) | (Self::NullIdentity, 5) => F::Null,
            (Self::ColdFileOnly | Self::WindowIncludes | Self::WindowExcludes, 1) => F::Text("b2"),
            (Self::ColdFileOnly | Self::WindowIncludes | Self::WindowExcludes, 2)
            | (Self::HotOnlyPresence, 6) => F::Text("x"),
            (Self::HotOnlyPresence, 5) => F::Text("a1"),
            (Self::NullIdentity, 1) => F::Text(""),
            (Self::NullIdentity, 2) => F::Text("null"),
            _ => F::Absent,
        }
    }

    /// The type cold parquet stores `f` as, in a file where some event
    /// carries it.
    fn cold_type(self) -> &'static str {
        match self {
            Self::NullDouble => "DOUBLE",
            _ => "VARCHAR",
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
/// The metamorphic law's pairs (AC 2): input that lacks `f`, and input
/// that carries an all-null `f` of the type the stage reads. A stage that
/// reads any type takes [`LAW`]; a text reader [`LAW_TEXT`]; a numeric
/// reader [`LAW_NUM`], which adds the absent `f` under a DOUBLE pin.
const LAW: &[Variant] = &[Variant::Absent, Variant::ExplicitNull, Variant::NullDouble];
const LAW_TEXT: &[Variant] = &[Variant::Absent, Variant::ExplicitNull];
const LAW_NUM: &[Variant] = &[Variant::Absent, Variant::NullDouble, Variant::PinnedDouble];
/// The variants in which no source carries `f` at all, pinned or not,
/// so `*` output has no `f` column to show.
const NOT_CARRIED: &[Variant] = &[
    Variant::Absent,
    Variant::PinnedVarchar,
    Variant::PinnedDouble,
];
const ABSENT: &[Variant] = &[Variant::Absent];
const IDENTITY: &[Variant] = &[Variant::NullIdentity];

/// One corpus on disk: a split of the events under one variant.
struct Corpus {
    /// Owns the files; dropped with the corpus.
    dir: tempfile::TempDir,
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
            dir,
            cold,
            hot,
            hot_keys,
        }
    }

    /// The pins the snapshot carries a key for, as the server cuts them.
    fn hot_pins(&self, pins: &FieldTypes) -> FieldTypes {
        let mut hot_pins = FieldTypes::new();
        for (field, ty) in pins.iter() {
            if self.hot_keys.iter().any(|key| key == field) {
                hot_pins.insert(field, ty);
            }
        }
        hot_pins
    }

    /// Run `dsl` through the batch lane this corpus calls for: the hot
    /// entry point when the split stores events hot, the cold one
    /// otherwise. `pins` is the catalog snapshot.
    fn run(
        &self,
        exec: &Executor,
        dsl: &str,
        pins: &FieldTypes,
    ) -> Result<QueryResult, EngineError> {
        match &self.hot {
            None => exec.run_query(dsl, &self.cold, pins, usize::MAX, 0),
            Some(hot) => exec.run_query_with_hot(
                dsl,
                &self.cold,
                hot.to_str().expect("utf-8 hot path"),
                &hot_columns_of(hot),
                &self.hot_pins(pins),
                pins,
                usize::MAX,
                0,
            ),
        }
    }

    /// Export `dsl` to the parquet file `out` through the export lane
    /// this corpus calls for, as [`Self::run`] picks the query lane.
    fn export(
        &self,
        exec: &Executor,
        dsl: &str,
        pins: &FieldTypes,
        out: &Path,
    ) -> Result<(), EngineError> {
        match &self.hot {
            None => exec.export_parquet(dsl, &self.cold, pins, out, usize::MAX),
            Some(hot) => exec.export_parquet_with_hot(
                dsl,
                &self.cold,
                hot.to_str().expect("utf-8 hot path"),
                &hot_columns_of(hot),
                &self.hot_pins(pins),
                pins,
                out,
                usize::MAX,
            ),
        }
    }

    /// The cold files as one glob, the shape embedded `--data` hands the
    /// executor.
    fn glob(&self) -> String {
        format!("{}/cold/*/*.parquet", self.dir.path().display())
    }

    /// A path inside the corpus directory, for a file a lane writes.
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

/// Write one cold parquet file. `f` is a column, of the variant's
/// [`Variant::cold_type`], only when some event of the file carries it,
/// even as null. A file in which no event carries `f` has no `f` column at
/// all.
fn write_cold_file(conn: &duckdb::Connection, path: &Path, events: &[&Event], variant: Variant) {
    let with_f = events.iter().any(|event| variant.f(event.id) != F::Absent);
    let ty = variant.cold_type();
    let rows: Vec<String> = events
        .iter()
        .map(|event| {
            let f = match variant.f(event.id) {
                F::Absent | F::Null => format!("CAST(NULL AS {ty})"),
                F::Text(text) => format!("CAST('{text}' AS {ty})"),
            };
            let mut row = format!(
                "({ts}, {ts}, CAST({id} AS BIGINT), '{host}', '{service}', '{message}', \
                 '{message}'",
                ts = event.timestamp_sql(),
                id = event.id,
                host = event.host,
                service = event.service,
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
        "_time, _ingested, id, host, service, message, _raw, f"
    } else {
        "_time, _ingested, id, host, service, message, _raw"
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
    /// The ids of the rows returned, in this order.
    Ordered(Vec<i64>),
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
    /// The whole column set, in any order, when the stage fixes the set
    /// but the lane picks the order: the batch result's columns, and the
    /// keys of every live row.
    column_set: Option<Vec<&'static str>>,
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
            column_set: None,
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

    fn only(mut self, columns: &[&'static str]) -> Self {
        self.column_set = Some(columns.to_vec());
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

/// The corpus event with id `id`.
fn event(id: i64) -> &'static Event {
    EVENTS
        .iter()
        .find(|event| event.id == id)
        .expect("a corpus id")
}

/// `[host, cell]` rows from `(id, cell)` pairs, the host read off
/// [`EVENTS`].
fn host_rows(cells: &[(i64, Cell)]) -> Vec<Vec<Cell>> {
    cells
        .iter()
        .map(|(id, cell)| vec![s(event(*id).host), cell.clone()])
        .collect()
}

/// `[service, cell]` rows from `(id, cell)` pairs, the service read off
/// [`EVENTS`].
fn service_rows(cells: &[(i64, Cell)]) -> Vec<Vec<Cell>> {
    cells
        .iter()
        .map(|(id, cell)| vec![s(event(*id).service), cell.clone()])
        .collect()
}

/// `f` as [`Variant::NullIdentity`] carries it, over every id.
fn identity_f() -> Vec<(i64, Cell)> {
    let mut cells = vec![(1, s("")), (2, s("null"))];
    cells.extend(nulls(&[3, 4, 5, 6]));
    cells
}

/// One row per value: the empty string once, the text `null` once, and
/// the null identity, absent and explicit null together, `nulls` times.
/// The grouped answer over [`Variant::NullIdentity`].
fn identity_groups(nulls: i64) -> Vec<Vec<Cell>> {
    vec![
        vec![s(""), Cell::Int(1)],
        vec![s("null"), Cell::Int(1)],
        vec![Cell::Null, Cell::Int(nulls)],
    ]
}

/// One row of cells.
fn one(cells: &[Cell]) -> Vec<Vec<Cell>> {
    vec![cells.to_vec()]
}

const ALL: &[i64] = &[1, 2, 3, 4, 5, 6];
/// The ids the window that excludes hour 10 keeps.
const LATE: &[i64] = &[3, 4, 5, 6];

/// One query and its answer per variant.
#[derive(Debug)]
struct Case {
    dsl: &'static str,
    answers: Vec<(&'static [Variant], Answer)>,
    /// Whether the live lane runs the case. A stage the stream plan
    /// refuses (`sort`, `pivot`, `eventstats`, a `timechart` bucket other
    /// than `_time`) has no live answer to compare.
    live: bool,
}

impl Case {
    fn new(dsl: &'static str) -> Self {
        Self {
            dsl,
            answers: Vec::new(),
            live: true,
        }
    }

    /// A case the live lane refuses: batch only.
    fn batch_only(mut self) -> Self {
        self.live = false;
        self
    }

    fn ids(mut self, variants: &'static [Variant], ids: &[i64]) -> Self {
        self.answers.push((variants, Answer::Ids(ids.to_vec())));
        self
    }

    fn ordered(mut self, variants: &'static [Variant], ids: &[i64]) -> Self {
        self.answers.push((variants, Answer::Ordered(ids.to_vec())));
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

            if case.live {
                let label = format!("{}/live", variant.name());
                match live_rows(&dsl, variant, &pins).and_then(|rows| judge_live(answer, &rows)) {
                    Ok(()) => passed.push(label),
                    Err(why) => failed.push(format!("{label}: {why}")),
                }
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

/// The outcome of every lane a hand-built test drives, collected so one
/// failing assertion reports all of them, as [`check`] does for a case.
#[derive(Debug, Default)]
struct Matrix {
    passed: Vec<String>,
    failed: Vec<String>,
}

impl Matrix {
    fn judge(&mut self, label: impl Into<String>, outcome: Result<(), String>) {
        let label = label.into();
        match outcome {
            Ok(()) => self.passed.push(label),
            Err(why) => self.failed.push(format!("{label}: {why}")),
        }
    }

    fn assert(self, what: &str) {
        assert!(
            self.failed.is_empty(),
            "{what}: {} of {} checks failed\n  {}\npassed: {}",
            self.failed.len(),
            self.failed.len() + self.passed.len(),
            self.failed.join("\n  "),
            self.passed.join(", ")
        );
    }
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
    let ids = || {
        rows.iter()
            .map(|row| match row.get("id") {
                Some(EvalValue::Int(id)) => Ok(*id),
                other => Err(format!("a row with id {other:?}")),
            })
            .collect::<Result<Vec<i64>, String>>()
    };
    match answer {
        Answer::Ids(want) => compare_ids(want, ids()?),
        Answer::Ordered(want) => compare_order(want, &ids()?),
        Answer::Rows(spec) => {
            for name in &spec.gone {
                if rows.iter().any(|row| row.contains_key(*name)) {
                    return Err(format!("expected no `{name}` key, got rows {rows:?}"));
                }
            }
            if let Some(set) = &spec.column_set {
                let want = sorted_names(set.iter().copied());
                for row in rows {
                    let got = sorted_names(row.keys().map(String::as_str));
                    if got != want {
                        return Err(format!("expected keys {want:?}, got a row with {got:?}"));
                    }
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
    let ids = || {
        let column = index("id").ok_or_else(|| format!("no `id` column in {names:?}"))?;
        result
            .rows
            .iter()
            .map(|row| match &row[column] {
                Value::Integer(id) => Ok(*id),
                other => Err(format!("a row with id {other:?}")),
            })
            .collect::<Result<Vec<i64>, String>>()
    };
    match answer {
        Answer::Ids(want) => compare_ids(want, ids()?),
        Answer::Ordered(want) => compare_order(want, &ids()?),
        Answer::Rows(spec) => {
            if let Some(exact) = &spec.exact_columns
                && names != *exact
            {
                return Err(format!("expected columns {exact:?}, got {names:?}"));
            }
            if let Some(set) = &spec.column_set {
                let want = sorted_names(set.iter().copied());
                let got = sorted_names(names.iter().copied());
                if got != want {
                    return Err(format!("expected the columns {want:?}, got {got:?}"));
                }
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
        Answer::Ordered(ids) => format!("ids in the order {ids:?}"),
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

fn compare_order(want: &[i64], got: &[i64]) -> Result<(), String> {
    if got == want {
        Ok(())
    } else {
        Err(format!("expected ids in the order {want:?}, got {got:?}"))
    }
}

fn sorted_names<'a>(names: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut names: Vec<&str> = names.collect();
    names.sort_unstable();
    names
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

// ── AC 2: absent equals explicit null ───────────────────────────────

/// The metamorphic law: every stage that reads `f` answers the same over
/// input that lacks `f` as over input that carries an all-null `f` of the
/// type the stage reads. Each case states one answer for the whole of its
/// pair ([`LAW`], [`LAW_TEXT`], [`LAW_NUM`]), so every member, every split
/// and, where the stage exists there, the live lane must give it. The
/// batch-only stages (`sort`, `pivot`, `eventstats`) run every split.
///
/// One read is excluded, as ADR-0062 rules: a `let` that reads an earlier
/// alias of the same stage ([`lateral_alias_is_the_exception`]).
mod absent_equals_explicit_null {
    use super::*;

    #[test]
    fn where_reads_null() {
        check(&Case::new("* | where isnull(f)").ids(LAW, ALL));
        check(&Case::new(r#"* | where f=="x""#).ids(LAW_TEXT, &[]));
    }

    #[test]
    fn let_writes_null() {
        check(
            &Case::new("* | let y = f | table id, y")
                .rows(LAW, Rows::new(&["id", "y"], by_id(&nulls(ALL)))),
        );
        check(
            &Case::new(r#"* | let y = coalesce(f, "d") | table id, y"#).rows(
                LAW_TEXT,
                Rows::new(&["id", "y"], by_id(&each(ALL, &s("d")))),
            ),
        );
    }

    #[test]
    fn table_writes_null() {
        check(&Case::new("* | table id, f").rows(
            LAW,
            Rows::new(&["id", "f"], by_id(&nulls(ALL))).exactly(&["id", "f"]),
        ));
    }

    #[test]
    fn rename_writes_null() {
        check(
            &Case::new("* | rename f as q | table id, q")
                .rows(LAW, Rows::new(&["id", "q"], by_id(&nulls(ALL)))),
        );
    }

    #[test]
    fn extract_writes_null() {
        check(
            &Case::new(r#"* | extract "(?P<k>\w+)" from f | table id, k"#)
                .rows(LAW_TEXT, Rows::new(&["id", "k"], by_id(&nulls(ALL)))),
        );
    }

    /// `count(f)` and `dc(f)` see no value; `min` and `max` see none.
    #[test]
    fn stats_arguments() {
        check(
            &Case::new("* | stats count(f) as c, dc(f) as d, min(f) as lo, max(f) as hi").rows(
                LAW,
                Rows::new(
                    &["c", "d", "lo", "hi"],
                    one(&[Cell::Int(0), Cell::Int(0), Cell::Null, Cell::Null]),
                ),
            ),
        );
    }

    /// `sum` and `avg` over no value are NULL, per group too.
    #[test]
    fn numeric_stats_arguments() {
        let per_host = ["web1", "web2", "web3"]
            .iter()
            .map(|host| vec![s(host), Cell::Null, Cell::Null])
            .collect();
        check(
            &Case::new("* | stats sum(f) as total, avg(f) as mean by host")
                .rows(LAW_NUM, Rows::new(&["host", "total", "mean"], per_host)),
        );
    }

    #[test]
    fn stats_by() {
        check(&Case::new("* | stats count() as n by f").rows(
            LAW,
            Rows::new(&["f", "n"], one(&[Cell::Null, Cell::Int(6)])),
        ));
    }

    /// Batch only: the live lane refuses `eventstats`. The partition and
    /// the argument both read `f`.
    #[test]
    fn eventstats() {
        check(
            &Case::new("* | eventstats count(f) as c, count() as n by f | table id, c, n")
                .batch_only()
                .rows(
                    LAW,
                    Rows::new(
                        &["id", "c", "n"],
                        ALL.iter()
                            .map(|id| vec![Cell::Int(*id), Cell::Int(0), Cell::Int(6)])
                            .collect(),
                    ),
                ),
        );
    }

    #[test]
    fn top() {
        check(&Case::new("* | top 10 f").rows(
            LAW,
            Rows::new(&["f", "count"], one(&[Cell::Null, Cell::Int(6)])),
        ));
    }

    #[test]
    fn rare() {
        check(&Case::new("* | rare 10 f").rows(
            LAW,
            Rows::new(&["f", "count"], one(&[Cell::Null, Cell::Int(6)])),
        ));
    }

    /// One day bucket holds every event, so the answer is one row.
    #[test]
    fn timechart_by() {
        check(&Case::new("* | timechart span=1d count() as n by f").rows(
            LAW,
            Rows::new(&["f", "n"], one(&[Cell::Null, Cell::Int(6)])),
        ));
    }

    /// Batch only: the live lane refuses `sort`. A null key ties every
    /// row, so the second key alone orders them.
    #[test]
    fn sort() {
        check(
            &Case::new("* | sort f, -id")
                .batch_only()
                .ordered(LAW, &[6, 5, 4, 3, 2, 1]),
        );
        check(
            &Case::new("* | sort -f, id")
                .batch_only()
                .ordered(LAW, &[1, 2, 3, 4, 5, 6]),
        );
    }

    /// Every event shares the null identity, so `dedup f` keeps one row,
    /// and `dedup f, host` one per host. The count is the answer because
    /// the lanes keep different representatives: batch the newest row
    /// of a partition, live the first to arrive.
    #[test]
    fn dedup() {
        check(
            &Case::new("* | dedup f | stats count() as n")
                .rows(LAW, Rows::new(&["n"], one(&[Cell::Int(1)]))),
        );
        check(
            &Case::new("* | dedup f, host | stats count() as n")
                .rows(LAW, Rows::new(&["n"], one(&[Cell::Int(3)]))),
        );
    }

    /// Batch only: the live lane refuses `pivot`. `f` as a pivot key, as
    /// a counted argument, and as a summed one.
    #[test]
    fn pivot() {
        check(
            &Case::new("* | pivot count() on host by f")
                .batch_only()
                .rows(
                    LAW,
                    Rows::new(
                        &["f", "web1", "web2", "web3"],
                        one(&[Cell::Null, Cell::Int(3), Cell::Int(2), Cell::Int(1)]),
                    ),
                ),
        );
        check(&Case::new("* | pivot count(f) on host").batch_only().rows(
            LAW,
            Rows::new(
                &["web1", "web2", "web3"],
                one(&[Cell::Int(0), Cell::Int(0), Cell::Int(0)]),
            ),
        ));
        check(&Case::new("* | pivot sum(f) on host").batch_only().rows(
            LAW_NUM,
            Rows::new(
                &["web1", "web2", "web3"],
                one(&[Cell::Null, Cell::Null, Cell::Null]),
            ),
        ));
    }

    /// The law's one exclusion, which the DSL reference lists. Inside one
    /// `let`, an input column binds first and an earlier alias of the same
    /// stage second, so `let f = 1, b = f` reads the alias when the input
    /// lacks `f` and the input's null when it carries one. The live lane
    /// binds the same way.
    ///
    /// The hot-only split of the explicit null reads the alias too. A hot
    /// key that was only ever null, with no pin and no cold carrier, is
    /// left out of the hot branch, as compaction leaves an unpinned
    /// all-null column out of parquet, so batch reads it as absent. The
    /// live lane still sees the event's null, so there the two lanes part.
    #[test]
    fn lateral_alias_is_the_exception() {
        let dsl = "* | let f = 1, b = f | table id, f, b";
        let answer = |b: &Cell| {
            Answer::Rows(Rows::new(
                &["id", "f", "b"],
                ALL.iter()
                    .map(|id| vec![Cell::Int(*id), Cell::Int(1), b.clone()])
                    .collect(),
            ))
        };
        let alias = answer(&Cell::Int(1));
        let input_null = answer(&Cell::Null);

        let exec = Executor::new().expect("executor");
        let pins = FieldTypes::new();
        let mut matrix = Matrix::default();
        for (variant, live, splits) in [
            (
                Variant::Absent,
                &alias,
                [
                    (Split::ColdOnly, &alias),
                    (Split::HotOnly, &alias),
                    (Split::Mixed, &alias),
                ],
            ),
            (
                Variant::ExplicitNull,
                &input_null,
                [
                    (Split::ColdOnly, &input_null),
                    (Split::HotOnly, &alias),
                    (Split::Mixed, &input_null),
                ],
            ),
        ] {
            matrix.judge(
                format!("{}/live", variant.name()),
                live_rows(dsl, variant, &pins).and_then(|rows| judge_live(live, &rows)),
            );
            for (split, answer) in splits {
                let corpus = Corpus::build(variant, split);
                matrix.judge(
                    format!("{}/{}", variant.name(), split.name()),
                    judge_batch(answer, corpus.run(&exec, dsl, &pins)),
                );
            }
        }
        matrix.assert(dsl);
    }

    /// One argument of a function fixture: a column of its natural type,
    /// or a literal the position requires.
    #[derive(Debug, Clone, Copy)]
    enum Arg {
        /// `s`, VARCHAR.
        Text,
        /// `n`, DOUBLE.
        Num,
        /// `i`, BIGINT.
        Int,
        /// `t`, TIMESTAMP.
        Time,
        /// `b`, BOOLEAN.
        Bool,
        /// `j`, VARCHAR holding JSON text.
        Json,
        /// `p`, VARCHAR holding a JSON path.
        Path,
        /// A literal, written as is: a unit, a format (`DuckDB` refuses a
        /// format that is not a constant, column or not), an index.
        Lit(&'static str),
    }

    impl Arg {
        /// The present column and its `DuckDB` type; `None` for a literal.
        fn column(self) -> Option<(&'static str, &'static str)> {
            Some(match self {
                Self::Text => ("s", "VARCHAR"),
                Self::Num => ("n", "DOUBLE"),
                Self::Int => ("i", "BIGINT"),
                Self::Time => ("t", "TIMESTAMP"),
                Self::Bool => ("b", "BOOLEAN"),
                Self::Json => ("j", "VARCHAR"),
                Self::Path => ("p", "VARCHAR"),
                Self::Lit(_) => return None,
            })
        }
    }

    /// Every known function, in each arity shape worth reading, with its
    /// arguments' natural types. `every_known_function_has_an_emission_fixture`
    /// (`trawl-core`) keeps the same inventory for emission bounds.
    fn function_fixtures() -> Vec<(&'static str, Vec<Arg>)> {
        use Arg::{Bool, Int, Json, Lit, Num, Path, Text, Time};
        let day = Lit("\"day\"");
        vec![
            ("count", vec![Text]),
            ("avg", vec![Num]),
            ("sum", vec![Num]),
            ("min", vec![Num]),
            ("max", vec![Num]),
            ("dc", vec![Text]),
            ("distinct_count", vec![Text]),
            ("p50", vec![Num]),
            ("p90", vec![Num]),
            ("p95", vec![Num]),
            ("p99", vec![Num]),
            ("first", vec![Text]),
            ("last", vec![Text]),
            ("values", vec![Text]),
            ("list", vec![Text]),
            ("median", vec![Num]),
            ("stddev", vec![Num]),
            ("lower", vec![Text]),
            ("upper", vec![Text]),
            ("length", vec![Text]),
            ("len", vec![Text]),
            ("coalesce", vec![Text, Text]),
            ("if", vec![Bool, Text, Text]),
            ("replace", vec![Text, Text, Text]),
            ("substr", vec![Text, Int]),
            ("substr", vec![Text, Int, Int]),
            ("trim", vec![Text]),
            ("ltrim", vec![Text]),
            ("rtrim", vec![Text]),
            ("isnull", vec![Text]),
            ("isnotnull", vec![Text]),
            ("abs", vec![Num]),
            ("ceil", vec![Num]),
            ("ceiling", vec![Num]),
            ("floor", vec![Num]),
            ("round", vec![Num]),
            ("round", vec![Num, Lit("1")]),
            ("now", vec![]),
            ("typeof", vec![Text]),
            ("tonumber", vec![Text]),
            ("tostring", vec![Num]),
            ("sev", vec![Text]),
            ("sev", vec![Text, Lit("\"syslog\"")]),
            ("contains", vec![Text, Text]),
            ("startswith", vec![Text, Text]),
            ("endswith", vec![Text, Text]),
            ("split", vec![Text, Text, Lit("0")]),
            ("concat", vec![Text, Text]),
            ("date_part", vec![day, Time]),
            ("date_trunc", vec![day, Time]),
            ("date_diff", vec![day, Time, Time]),
            ("strftime", vec![Time, Lit("\"%Y\"")]),
            ("strptime", vec![Text, Lit("\"%Y\"")]),
            ("case", vec![Bool, Text]),
            ("case", vec![Bool, Text, Text]),
            ("json", vec![Json, Path]),
            ("json_extract", vec![Json, Path]),
            ("json_extract_string", vec![Json, Path]),
            ("json_valid", vec![Json]),
            ("json_keys", vec![Json]),
            ("json_array_length", vec![Json]),
        ]
    }

    /// The fixture source: two rows carrying a column of every natural
    /// type, plus each `(name, type)` of `nulls` as a column holding a
    /// NULL of that type. Read as the subquery source `from saved` also
    /// hands the executor, so the probe describes it like any other
    /// reader.
    fn function_source(nulls: &[(String, &str)]) -> String {
        let nulls = nulls.iter().fold(String::new(), |mut acc, (name, ty)| {
            use std::fmt::Write as _;
            let _ = write!(acc, ", CAST(NULL AS {ty}) AS {name}");
            acc
        });
        format!(
            "(SELECT *{nulls} FROM (VALUES \
             (TIMESTAMP '2024-01-15 10:00:00', TIMESTAMP '2024-01-15 10:00:00', 1, \
              'Web1 ', 2.5::DOUBLE, 3::BIGINT, TIMESTAMP '2024-01-15 10:00:00', true, \
              '{{\"a\":[1,2]}}', '$.a'), \
             (TIMESTAMP '2024-01-15 11:00:00', TIMESTAMP '2024-01-15 11:00:00', 2, \
              'x,y', -1.5::DOUBLE, 1::BIGINT, TIMESTAMP '2024-02-16 11:30:00', false, \
              '[1,2,3]', '$[0]')) \
             AS v(_time, _ingested, id, s, n, i, t, b, j, p))"
        )
    }

    /// The sets of field positions a fixture reads absent: each one in
    /// turn, then all of them at once when there are several, which is
    /// where no present argument is left to pick the overload
    /// (`date_diff("day", g0, g1)`).
    fn absent_sets(args: &[Arg]) -> Vec<Vec<usize>> {
        let fields: Vec<usize> = (0..args.len())
            .filter(|&i| args[i].column().is_some())
            .collect();
        let mut sets: Vec<Vec<usize>> = fields.iter().map(|&i| vec![i]).collect();
        if fields.len() > 1 {
            sets.push(fields);
        }
        sets
    }

    /// `y` per row in id order, or the one aggregate row's `y`.
    fn y_cells(result: &QueryResult) -> Result<Vec<Cell>, String> {
        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        let y = names
            .iter()
            .position(|n| *n == "y")
            .ok_or_else(|| format!("no `y` column in {names:?}"))?;
        let mut rows: Vec<(Cell, Cell)> = result
            .rows
            .iter()
            .map(|row| {
                let id = names
                    .iter()
                    .position(|n| *n == "id")
                    .map_or(Cell::Null, |i| Cell::from(&row[i]));
                (id, Cell::from(&row[y]))
            })
            .collect();
        rows.sort_by(|a, b| format!("{:?}", a.0).cmp(&format!("{:?}", b.0)));
        Ok(rows.into_iter().map(|(_, y)| y).collect())
    }

    /// The overloaded date functions over the parquet corpus, where no
    /// event carries `f`: each reads a TIMESTAMP NULL and answers NULL.
    #[test]
    fn overloaded_date_functions() {
        let nulls4 = |ids: &[i64]| {
            ids.iter()
                .map(|id| vec![Cell::Int(*id), Cell::Null, Cell::Null, Cell::Null])
                .collect()
        };
        check(
            &Case::new(
                r#"* | let y = date_part("day", f), z = date_trunc("day", f), w = date_diff("day", f, f) | table id, y, z, w"#,
            )
            .rows(ABSENT, Rows::new(&["id", "y", "z", "w"], nulls4(ALL))),
        );
    }

    /// How the sweep spells an absent argument from the two absent fields
    /// `a` and `b`: bare, or composed only of them, so its value is NULL by
    /// construction while its SQL is no bare field read.
    #[derive(Debug, Clone, Copy)]
    enum Form {
        Bare,
        Coalesce,
        If,
        Case,
        /// The three branch-selecting forms nested in one another.
        Nested,
    }

    impl Form {
        const ALL: [Self; 5] = [
            Self::Bare,
            Self::Coalesce,
            Self::If,
            Self::Case,
            Self::Nested,
        ];

        fn spell(self, a: &str, b: &str) -> String {
            match self {
                Self::Bare => a.to_owned(),
                Self::Coalesce => format!("coalesce({a}, {b})"),
                Self::If => format!("if(true, {a}, {b})"),
                Self::Case => format!("case(false, {a}, {b})"),
                Self::Nested => format!("coalesce(if(true, {a}, {b}), case(true, {b}))"),
            }
        }

        /// The fields the spelling reads.
        fn fields(self, a: String, b: String) -> Vec<String> {
            match self {
                Self::Bare => vec![a],
                _ => vec![a, b],
            }
        }
    }

    /// Every known function, with each field argument absent in turn and
    /// then all of them at once, binds, and answers what the same call
    /// answers over a NULL of each argument's natural type: an absent
    /// field is no reason for `DuckDB` to lose the overload it picks for
    /// a typed column.
    ///
    /// Each absent argument is spelled every [`Form`] way: a bare field,
    /// and the branch-selecting functions over absent fields only, alone
    /// and nested. The control spells the call the same way over columns
    /// holding a NULL of the argument's natural type.
    ///
    /// One exception, stated rather than skipped: `typeof` names its
    /// operand's type, and an unpinned absent field has none, so it
    /// answers `DuckDB`'s name for the NULL type where a typed NULL
    /// answers its type.
    #[test]
    fn every_known_function_with_each_argument_absent() {
        let fixtures = function_fixtures();
        for &name in trawl_core::parser::suggest::KNOWN_FUNCTIONS {
            assert!(
                fixtures.iter().any(|(f, _)| *f == name),
                "{name} has no fixture in this sweep"
            );
        }

        let exec = Executor::new().expect("executor");
        let absent_source = function_source(&[]);
        let mut matrix = Matrix::default();
        for (name, args) in &fixtures {
            for absent in absent_sets(args) {
                for form in Form::ALL {
                    let call_args: Vec<String> = args
                        .iter()
                        .enumerate()
                        .map(|(i, arg)| match (absent.contains(&i), arg.column(), arg) {
                            (true, _, _) => form.spell(&format!("g{i}"), &format!("h{i}")),
                            (false, Some((column, _)), _) => column.to_owned(),
                            (false, None, Arg::Lit(text)) => (*text).to_owned(),
                            (false, None, _) => unreachable!("only a literal has no column"),
                        })
                        .collect();
                    let call = format!("{name}({})", call_args.join(", "));
                    let dsl = if trawl_core::emitter::is_aggregate_function(name) {
                        format!("* | stats {call} as y")
                    } else {
                        format!("* | let y = {call} | table id, y")
                    };
                    let label = format!("`{dsl}`");
                    let run = |source: &str| {
                        exec.run_query(&dsl, source, &FieldTypes::new(), usize::MAX, 0)
                            .map_err(|error| error.to_string())
                            .and_then(|result| y_cells(&result))
                    };
                    let nulls: Vec<(String, &str)> = absent
                        .iter()
                        .filter_map(|&i| {
                            let (_, ty) = args[i].column()?;
                            Some(
                                form.fields(format!("g{i}"), format!("h{i}"))
                                    .into_iter()
                                    .map(move |field| (field, ty)),
                            )
                        })
                        .flatten()
                        .collect();
                    let control = match run(&function_source(&nulls)) {
                        Ok(cells) => cells,
                        Err(why) => {
                            matrix.judge(
                                &label,
                                Err(format!("the typed-NULL control failed: {why}")),
                            );
                            continue;
                        }
                    };
                    let want = if *name == "typeof" {
                        vec![s("\"NULL\""); control.len()]
                    } else {
                        control
                    };
                    let outcome = run(&absent_source).and_then(|got| {
                        if got == want {
                            Ok(())
                        } else {
                            Err(format!("expected {want:?}, got {got:?}"))
                        }
                    });
                    matrix.judge(label, outcome);
                }
            }
        }
        matrix.assert("every known function with each argument absent");
    }

    /// A query text the corpus [`Case`] can hold.
    fn leak(dsl: String) -> &'static str {
        Box::leak(dsl.into_boxed_str())
    }

    /// The overloaded positions over the parquet corpus, with each absent
    /// argument spelled every [`Form`] way. Batch reads a typed NULL there
    /// and the live lane a NULL, so every lane answers NULL on every row.
    #[test]
    fn composed_absent_arguments_at_overloaded_positions() {
        for form in Form::ALL {
            let x = form.spell("f", "g");
            for call in [
                format!(r#"date_part("day", {x})"#),
                format!(r#"date_trunc("day", {x})"#),
                format!(r#"date_diff("day", {x}, {x})"#),
                format!("contains({x}, {x})"),
            ] {
                check(
                    &Case::new(leak(format!("* | let y = {call} | table id, y")))
                        .rows(ABSENT, Rows::new(&["id", "y"], by_id(&nulls(ALL)))),
                );
            }
            // The live lane aggregates a bare field only, so a composed
            // percentile operand runs batch only.
            let percentile = Case::new(leak(format!("* | stats p95({x}) as y")))
                .rows(ABSENT, Rows::new(&["y"], one(&[Cell::Null])));
            check(&match form {
                Form::Bare => percentile,
                _ => percentile.batch_only(),
            });
        }
        // The `null` literal is a NULL by construction too.
        check(
            &Case::new(r#"* | let y = date_trunc("day", coalesce(f, null)) | table id, y"#)
                .rows(ABSENT, Rows::new(&["id", "y"], by_id(&nulls(ALL)))),
        );
    }

    /// A column an earlier stage wrote, whose every value is NULL by
    /// construction, read at an overloaded position: a `let` of a
    /// composed absent argument, and the aggregates that keep their
    /// argument's type.
    #[test]
    fn null_columns_of_earlier_stages_at_overloaded_positions() {
        let rows = || Rows::new(&["id", "y"], by_id(&nulls(ALL)));
        check(
            &Case::new(r#"* | let a = coalesce(f, g) | let y = date_part("day", a) | table id, y"#)
                .rows(ABSENT, rows()),
        );
        check(
            &Case::new("* | let a = if(true, f, g), y = contains(a, a) | table id, y")
                .rows(ABSENT, rows()),
        );
        check(
            &Case::new(r#"* | stats max(f) as m | let y = date_part("day", m) | table y"#)
                .rows(ABSENT, Rows::new(&["y"], one(&[Cell::Null]))),
        );
        check(
            &Case::new(
                r#"* | eventstats first(f) as m | let y = date_trunc("day", m) | table id, y"#,
            )
            .batch_only()
            .rows(ABSENT, rows()),
        );
        check(
            &Case::new(r#"* | pivot max(f) on host | let y = date_part("day", web1) | table y"#)
                .batch_only()
                .rows(ABSENT, Rows::new(&["y"], one(&[Cell::Null]))),
        );
    }

    /// A pivot value column no event produced has the type the pivot gives
    /// every value column, so a call reads it as it reads a column the
    /// data made: an overloaded call binds and answers NULL, here, later,
    /// and through a stage that carries it. Over a `count()` pivot that
    /// type is BIGINT, and `date_part` refuses it as it refuses the
    /// produced `web1`.
    ///
    /// One event, `host` web1: `web1` is produced, `web2` and `web3` are
    /// not.
    #[test]
    fn pivot_value_columns_nobody_produced_read_the_pivots_type() {
        let exec = Executor::new().expect("executor");
        let source = "(SELECT TIMESTAMP '2024-01-15 10:00:00' AS _time, \
                      TIMESTAMP '2024-01-15 10:00:00' AS _ingested, \
                      'web1' AS host, 'svc' AS service)";
        let run = |dsl: &str| {
            exec.run_query(dsl, source, &FieldTypes::new(), usize::MAX, 0)
                .map(|result| result.rows.iter().map(|row| Cell::from(&row[0])).collect())
                .map_err(|error| error.to_string())
        };
        let mut matrix = Matrix::default();
        let latest = "* | pivot max(_time) on host by service";
        let text = "* | pivot max(service) on host by service";
        let counted = "* | pivot count() on host by service";
        for (dsl, want) in [
            (
                format!(r#"{latest} | let y = date_part("day", web1) | table y"#),
                Cell::Int(15),
            ),
            (
                format!(r#"{latest} | let y = date_part("day", web2) | table y"#),
                Cell::Null,
            ),
            (
                format!(r#"{latest} | let y = date_trunc("day", web2) | table y"#),
                Cell::Null,
            ),
            (
                format!(r#"{latest} | let y = date_diff("day", web1, web2) | table y"#),
                Cell::Null,
            ),
            (
                format!(r#"{latest} | let y = date_part("day", coalesce(web2, web3)) | table y"#),
                Cell::Null,
            ),
            (
                format!(r#"{latest} | table web2 | let y = date_part("day", web2) | table y"#),
                Cell::Null,
            ),
            (
                format!(
                    r#"{latest} | stats max(web2) as m | let y = date_part("day", m) | table y"#
                ),
                Cell::Null,
            ),
            (
                format!(r#"{text} | let y = contains(web1, "v") | table y"#),
                Cell::Bool(true),
            ),
            (
                format!(r#"{text} | let y = contains(web2, "v") | table y"#),
                Cell::Null,
            ),
            (format!("{counted} | stats p95(web2) as y"), Cell::Null),
        ] {
            matrix.judge(
                format!("`{dsl}`"),
                run(&dsl).and_then(|got: Vec<Cell>| {
                    if got == [want.clone()] {
                        Ok(())
                    } else {
                        Err(format!("expected [{want:?}], got {got:?}"))
                    }
                }),
            );
        }
        for column in ["web1", "web2"] {
            let dsl = format!(r#"{counted} | let y = date_part("day", {column}) | table y"#);
            matrix.judge(
                format!("`{dsl}` is refused"),
                match run(&dsl) {
                    Err(error) if error.contains("date_part") => Ok(()),
                    other => Err(format!("expected a date_part binder error, got {other:?}")),
                },
            );
        }
        matrix.assert("pivot value columns nobody produced");
    }
}

// ── AC 3: alias capture ─────────────────────────────────────────────

/// An absent read never binds an alias the same `SELECT` defines. Every
/// absent read is a NULL literal, so no identifier is left for `DuckDB` to
/// resolve against an output alias, and every case gives the live answer.
mod alias_capture {
    use super::*;

    /// `[id, host]` over `ids`: what `rename host as f` writes into `f`.
    fn hosts(ids: &[i64]) -> Rows {
        let cells: Vec<(i64, Cell)> = ids.iter().map(|id| (*id, s(event(*id).host))).collect();
        Rows::new(&["id", "f"], by_id(&cells)).without("host")
    }

    /// The defect ADR-0062 records on main: with no event carrying `f`,
    /// `DuckDB` bound the search `WHERE`'s `f` to the `host AS f` alias of
    /// the same `SELECT`, and filtered on `host`.
    #[test]
    fn search_filter_then_rename_onto_its_field() {
        check(
            &Case::new("f!=web1 | rename host as f")
                .rows(NO_F_TEXT, hosts(ALL))
                .rows(COLD_F, hosts(ALL))
                .rows(HOT_F, hosts(ALL))
                .rows(OUT, hosts(LATE)),
        );
    }

    /// Batch only: the live lane refuses `eventstats`.
    #[test]
    fn search_filter_then_eventstats_onto_its_field() {
        let counted = |ids: &[i64]| {
            let n = i64::try_from(ids.len()).expect("a small count");
            Rows::new(&["id", "f"], by_id(&each(ids, &Cell::Int(n))))
                .typed("f", ColumnType::Integer)
        };
        check(
            &Case::new("f!=x | eventstats count() as f")
                .batch_only()
                .rows(NO_F_TEXT, counted(ALL))
                .rows(COLD_F, counted(&[1, 3, 4, 5, 6]))
                .rows(HOT_F, counted(&[1, 2, 3, 4, 5]))
                .rows(OUT, counted(LATE)),
        );
    }

    #[test]
    fn search_filter_then_stats_onto_its_field() {
        let counted = |n: i64| {
            Rows::new(&["f"], one(&[Cell::Int(n)]))
                .typed("f", ColumnType::Integer)
                .exactly(&["f"])
        };
        check(
            &Case::new("f!=x | stats count() as f")
                .rows(NO_F_TEXT, counted(6))
                .rows(COLD_F, counted(5))
                .rows(HOT_F, counted(5))
                .rows(OUT, counted(4)),
        );
    }

    /// `ms` is no input column, so `total` reads the alias the same `let`
    /// wrote before it: 2000, never NULL.
    #[test]
    fn where_then_let_reads_its_own_alias() {
        let rows = |ids: &[i64]| {
            Rows::new(
                &["id", "ms", "total"],
                ids.iter()
                    .map(|id| vec![Cell::Int(*id), Cell::Int(1000), Cell::Int(2000)])
                    .collect(),
            )
        };
        check(
            &Case::new("* | where isnull(ms) | let ms = 1000, total = ms * 2")
                .rows(NO_F, rows(ALL))
                .rows(OUT, rows(LATE)),
        );
    }

    /// `a` is no input column, so `b` reads the alias.
    #[test]
    fn let_reads_an_earlier_alias() {
        let rows = |ids: &[i64]| {
            Rows::new(
                &["id", "a", "b"],
                ids.iter()
                    .map(|id| vec![Cell::Int(*id), Cell::Int(1), Cell::Int(1)])
                    .collect(),
            )
        };
        check(
            &Case::new("* | let a = 1, b = a")
                .rows(NO_F, rows(ALL))
                .rows(OUT, rows(LATE)),
        );
    }

    /// After a pivot the relation is the data's to decide, and a name a
    /// `let` of the post-pivot scope assigns is never padded with the
    /// pivot's null fill, so `b` reads the alias: the answer the live lane
    /// gives the same `let`. The live lane refuses `pivot` itself, so its
    /// half is the `let` alone over the same events.
    #[test]
    fn pivot_then_let_reads_an_earlier_alias() {
        let dsl = "* | pivot count() on host | let a = 1, b = a";
        check(&Case::new(dsl).batch_only().rows(
            NO_F,
            Rows::new(
                &["web1", "web2", "web3", "a", "b"],
                one(&[
                    Cell::Int(3),
                    Cell::Int(2),
                    Cell::Int(1),
                    Cell::Int(1),
                    Cell::Int(1),
                ]),
            ),
        ));

        let pins = FieldTypes::new();
        let refused =
            live_rows(dsl, Variant::Absent, &pins).expect_err("the live lane has no pivot");
        assert!(
            refused.contains("pivot"),
            "refused for its pivot: {refused}"
        );
        let live = live_rows("* | let a = 1, b = a | table a, b", Variant::Absent, &pins)
            .expect("the let alone runs live");
        assert_eq!(
            judge_live(
                &Answer::Rows(Rows::new(
                    &["a", "b"],
                    (0..EVENTS.len())
                        .map(|_| vec![Cell::Int(1), Cell::Int(1)])
                        .collect(),
                )),
                &live,
            ),
            Ok(()),
            "the live lane's `let` reads the alias"
        );
    }

    /// `x` reads `a` before the same `let` assigns it, and the input lacks
    /// `a`: NULL, and `b` reads the alias. Both lanes, no pivot.
    #[test]
    fn let_reads_a_later_target_as_null_first() {
        let rows = |ids: &[i64]| {
            Rows::new(
                &["id", "x", "a", "b"],
                ids.iter()
                    .map(|id| vec![Cell::Int(*id), Cell::Null, Cell::Int(1), Cell::Int(1)])
                    .collect(),
            )
        };
        check(
            &Case::new("* | let x = a, a = 1, b = a | table id, x, a, b")
                .rows(NO_F, rows(ALL))
                .rows(OUT, rows(LATE)),
        );
    }

    /// The same `let` after a pivot. A name a target of the `let` assigns
    /// is never padded onto the pivot's fill, so the earlier read is NULL
    /// and the later one reads the alias, exactly as without the pivot. A
    /// fill would give the input an `a` column, which `DuckDB` binds
    /// before the alias.
    #[test]
    fn pivot_then_let_reads_a_later_target_as_null_first() {
        let dsl = "* | pivot count() on host by service | let x = a, a = 1, b = a \
                   | table service, x, a, b";
        let row = |service: &str| vec![s(service), Cell::Null, Cell::Int(1), Cell::Int(1)];
        check(&Case::new(dsl).batch_only().rows(
            NO_F,
            Rows::new(
                &["service", "x", "a", "b"],
                ["api", "auth", "cache", "storage"].map(row).to_vec(),
            ),
        ));

        let pins = FieldTypes::new();
        let live = live_rows(
            "* | let x = a, a = 1, b = a | table x, a, b",
            Variant::Absent,
            &pins,
        )
        .expect("the let alone runs live");
        assert_eq!(
            judge_live(
                &Answer::Rows(Rows::new(
                    &["x", "a", "b"],
                    (0..EVENTS.len())
                        .map(|_| vec![Cell::Null, Cell::Int(1), Cell::Int(1)])
                        .collect(),
                )),
                &live,
            ),
            Ok(()),
            "the live lane's `let` reads NULL, then the alias"
        );
    }

    /// Across stages the pivot's own rule holds: a column a later stage
    /// reads and no event produced appears as an all-null column. So the
    /// `where` that reads `ms` makes it an input column of the `let`,
    /// which `total` reads before the alias, as it would read a pivot
    /// value column named `ms`. Without the pivot, `total` is 2000
    /// ([`where_then_let_reads_its_own_alias`]).
    #[test]
    fn pivot_then_where_makes_the_column_a_later_let_reads() {
        check(
            &Case::new(
                "* | pivot count() on host by service | where isnull(ms) \
                 | let ms = 1000, total = ms * 2 | table service, ms, total",
            )
            .batch_only()
            .rows(
                NO_F,
                Rows::new(
                    &["service", "ms", "total"],
                    ["api", "auth", "cache", "storage"]
                        .map(|service| vec![s(service), Cell::Int(1000), Cell::Null])
                        .to_vec(),
                ),
            ),
        );
    }
}

// ── AC 4: reads after a projection ──────────────────────────────────

/// A stage that removes `f` leaves the next stage an input without it,
/// whatever the source carried, so a later read is a NULL read. The
/// variants where `f` holds values ([`COLD_F`], [`HOT_F`]) prove it: the
/// field is there before the projection and NULL after it.
mod post_projection_reads_are_null {
    use super::*;

    /// Every variant, `f` present or not before the projection.
    const BEFORE: &[Variant] = &[
        Variant::Absent,
        Variant::ExplicitNull,
        Variant::PinnedVarchar,
        Variant::PinnedDouble,
        Variant::ColdFileOnly,
        Variant::WindowIncludes,
        Variant::HotOnlyPresence,
    ];

    #[test]
    fn after_table() {
        let hosts = |ids: &[i64]| {
            Rows::new(
                &["host"],
                ids.iter().map(|id| vec![s(event(*id).host)]).collect(),
            )
            .exactly(&["host"])
        };
        check(
            &Case::new("* | table host | where isnull(f)")
                .rows(BEFORE, hosts(ALL))
                .rows(OUT, hosts(LATE)),
        );
        check(
            &Case::new("* | table id, host | where isnull(f)")
                .ids(BEFORE, ALL)
                .ids(OUT, LATE),
        );
    }

    /// `f` is present before the `drop` in [`COLD_F`] and [`HOT_F`].
    #[test]
    fn after_drop() {
        let ids = |ids: &[i64]| {
            Rows::new(&["id"], ids.iter().map(|id| vec![Cell::Int(*id)]).collect()).without("f")
        };
        check(
            &Case::new("* | drop f | where isnull(f)")
                .rows(BEFORE, ids(ALL))
                .rows(OUT, ids(LATE)),
        );
    }

    /// The `stats` output carries `host` and `n` and nothing else.
    #[test]
    fn after_stats() {
        let counts = |rows: &[(&str, i64)]| {
            Rows::new(
                &["host", "n"],
                rows.iter()
                    .map(|(host, n)| vec![s(host), Cell::Int(*n)])
                    .collect(),
            )
            .exactly(&["host", "n"])
        };
        check(
            &Case::new("* | stats count() as n by host | where isnull(f)")
                .rows(BEFORE, counts(&[("web1", 3), ("web2", 2), ("web3", 1)]))
                .rows(OUT, counts(&[("web1", 2), ("web2", 1), ("web3", 1)])),
        );
    }

    /// The rename's source is gone; its values live on under `g`.
    #[test]
    fn after_rename_source() {
        let renamed = |cells: &[(i64, Cell)]| Rows::new(&["id", "g"], by_id(cells)).without("f");
        check(
            &Case::new("* | rename f as g | where isnull(f)")
                .rows(NO_F, renamed(&nulls(ALL)))
                .rows(COLD_F, renamed(&cold_f(&Cell::Null)))
                .rows(HOT_F, renamed(&hot_f(&Cell::Null)))
                .rows(OUT, renamed(&nulls(LATE))),
        );
    }

    /// A query that reads only the envelope timestamps skips the probe,
    /// so nobody describes its root. A stage that removes a timestamp
    /// removes it there too: the read after it is NULL, as on the probed
    /// path, never `unknown field`. The source's own missing timestamp is
    /// the loud case; a stage removing one is not.
    #[test]
    fn envelope_reads_after_a_projection_skip_the_probe() {
        use trawl_engine::executor::schema_probe;

        let count = |n: i64| Rows::new(&["n"], one(&[Cell::Int(n)])).exactly(&["n"]);
        let unprobed = [
            "* | table _time | where isnull(_ingested) | stats count() as n",
            "* | drop _ingested | where isnull(_ingested) | stats count() as n",
        ];
        for dsl in unprobed {
            check(&Case::new(dsl).rows(ABSENT, count(6)));
        }
        // The probed path's answer for the same read.
        check(
            &Case::new("* | table _time, f | where isnull(_ingested) | stats count() as n")
                .rows(ABSENT, count(6)),
        );
        let stats_time = Rows::new(&["_time"], one(&[Cell::Null]))
            .typed("_time", ColumnType::String)
            .exactly(&["_time"]);
        check(&Case::new("* | stats count() | table _time").rows(ABSENT, stats_time));

        // Those reads really skipped the probe.
        let exec = Executor::new().expect("executor");
        let corpus = Corpus::build(Variant::Absent, Split::ColdOnly);
        for dsl in unprobed
            .into_iter()
            .chain(["* | stats count() | table _time"])
        {
            let before = schema_probe::count_on_this_thread();
            corpus
                .run(&exec, dsl, &FieldTypes::new())
                .unwrap_or_else(|error| panic!("`{dsl}`: {error}"));
            assert_eq!(
                schema_probe::count_on_this_thread() - before,
                0,
                "`{dsl}` reads only the envelope, so it skips the probe"
            );
        }
    }

    /// Direct-null lineage: a column that is a direct NULL of an absent
    /// field is read downstream as the NULL literal, never by its name,
    /// so a text comparison does not meet an INTEGER column, and `sum`
    /// does not meet a VARCHAR one.
    #[test]
    fn a_projected_null_compares_as_text() {
        let f = |cells: Vec<Vec<Cell>>| Rows::new(&["f"], cells).exactly(&["f"]);
        check(
            &Case::new(r#"* | table f | where f=="x""#)
                .rows(NO_F_TEXT, f(Vec::new()))
                .rows(COLD_F, f(one(&[s("x")])))
                .rows(HOT_F, f(one(&[s("x")])))
                .rows(OUT, f(Vec::new())),
        );
    }

    /// Only where `f` is absent or numeric: `sum` over a present VARCHAR
    /// column is refused on main too.
    #[test]
    fn a_projected_null_sums_to_null() {
        let numeric = &[Variant::Absent, Variant::PinnedDouble, Variant::NullDouble];
        for dsl in [
            "* | table f | stats sum(f) as total",
            "* | let y = f | stats sum(y) as total",
        ] {
            check(&Case::new(dsl).rows(numeric, Rows::new(&["total"], one(&[Cell::Null]))));
        }
    }
}

// ── AC 6: output shape ──────────────────────────────────────────────

/// A reference never changes the output's columns by itself, and
/// `table` lists every requested name in the requested order.
mod output_shape {
    use super::*;

    /// A filter-only read adds no column: `*` returns the input's columns,
    /// in whatever order the lane reads them, and no `f`.
    #[test]
    fn a_filter_read_adds_no_column() {
        let input = |ids: &[i64]| {
            Rows::new(&["id"], ids.iter().map(|id| vec![Cell::Int(*id)]).collect())
                .only(INPUT_COLUMNS)
        };
        check(
            &Case::new("* | where isnull(f)")
                .rows(NOT_CARRIED, input(ALL))
                .rows(OUT, input(LATE)),
        );
    }

    /// A name `table` or `fields` lists twice is one column, at its first
    /// position, as the live lane keeps it. `DuckDB` would name a repeat
    /// `host_1` in a CTE, a column no lane's row carries, so a later read
    /// of `host_1` is an absent read in both lanes.
    #[test]
    fn a_repeated_name_projects_once() {
        let hosts = |ids: &[i64]| {
            let rows = ids
                .iter()
                .map(|id| vec![Cell::Int(*id), s(event(*id).host)])
                .collect();
            Rows::new(&["id", "host"], rows).exactly(&["id", "host"])
        };
        for dsl in ["* | table id, host, host", "* | fields id, host, HOST, id"] {
            check(&Case::new(dsl).rows(ABSENT, hosts(ALL)));
        }
        check(
            &Case::new("* | table id, f, f").rows(
                ABSENT,
                Rows::new(&["id", "f"], by_id(&nulls(ALL)))
                    .typed("f", ColumnType::String)
                    .exactly(&["id", "f"]),
            ),
        );
        check(
            &Case::new("* | table id, host, host | table id, host_1").rows(
                ABSENT,
                Rows::new(&["id", "host_1"], by_id(&nulls(ALL))).exactly(&["id", "host_1"]),
            ),
        );
        check(&Case::new(r#"* | table id, host, host | where host_1 == "web1""#).ids(ABSENT, &[]));
    }

    /// The pivot's narrowing projection names each input column once: a
    /// repeat would be a `host_1` column the `PIVOT` takes as a row key.
    #[test]
    fn a_pivot_reads_a_repeated_input_once() {
        check(
            &Case::new("* | pivot count(host) on host")
                .batch_only()
                .rows(
                    ABSENT,
                    Rows::new(
                        &["web1", "web2", "web3"],
                        one(&[Cell::Int(3), Cell::Int(2), Cell::Int(1)]),
                    )
                    .exactly(&["web1", "web2", "web3"]),
                ),
        );
    }

    /// `service` then an all-null `f`, VARCHAR unless a pin types it.
    #[test]
    fn table_lists_absent_names_in_order() {
        let rows = |cells: &[(i64, Cell)], ty: ColumnType| {
            Rows::new(&["service", "f"], service_rows(cells))
                .typed("f", ty)
                .exactly(&["service", "f"])
        };
        check(
            &Case::new("* | table service, f")
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
}

// ── AC 7: every batch lane ──────────────────────────────────────────

/// Every batch lane resolves absent reads, not only the query lane the
/// other groups drive: the hot-less query, the union with hot, the
/// cold-start hot-only route, embedded `--data` over a parquet glob and
/// over ndjson, the export read back, the timechart input probes, the SQL
/// prefix before `extract kv`, and the `from saved` sources as far as the
/// engine API reaches. The server's own `from saved` resolution is
/// `trawl-server`'s `from_saved` tests.
mod every_lane {
    use super::*;

    /// What every lane must answer over the base corpus, where no event
    /// carries `f`: an `OR` branch survives, a projected absent field is a
    /// VARCHAR null, a filter-only read counts every row, and a group key
    /// is the null identity.
    fn battery() -> Vec<(&'static str, Answer)> {
        vec![
            ("host=web1 OR f=x", Answer::Ids(vec![1, 3, 5])),
            (
                "f!=x | table id, f",
                Answer::Rows(
                    Rows::new(&["id", "f"], by_id(&nulls(ALL)))
                        .typed("f", ColumnType::String)
                        .exactly(&["id", "f"]),
                ),
            ),
            (
                "* | where isnull(f) | stats count() as n",
                Answer::Rows(Rows::new(&["n"], one(&[Cell::Int(6)])).exactly(&["n"])),
            ),
            (
                "f!=x | stats count() as n by f",
                Answer::Rows(
                    Rows::new(&["f", "n"], one(&[Cell::Null, Cell::Int(6)]))
                        .typed("f", ColumnType::String)
                        .exactly(&["f", "n"]),
                ),
            ),
        ]
    }

    /// Run the battery through one lane.
    fn through(
        lane: &str,
        matrix: &mut Matrix,
        mut run: impl FnMut(&str) -> Result<QueryResult, EngineError>,
    ) {
        for (dsl, answer) in battery() {
            matrix.judge(format!("{lane} `{dsl}`"), judge_batch(&answer, run(dsl)));
        }
    }

    /// The query lane per split: no hot snapshot, the union, and the cold
    /// start, where the cold source reaches no file.
    fn query(split: Split) {
        let exec = Executor::new().expect("executor");
        let corpus = Corpus::build(Variant::Absent, split);
        let pins = FieldTypes::new();
        let mut matrix = Matrix::default();
        through(split.name(), &mut matrix, |dsl| {
            corpus.run(&exec, dsl, &pins)
        });
        matrix.assert("the query lane");
    }

    #[test]
    fn query_without_hot() {
        query(Split::ColdOnly);
    }

    #[test]
    fn query_with_hot() {
        query(Split::Mixed);
    }

    #[test]
    fn cold_start_hot_only() {
        query(Split::HotOnly);
    }

    /// Embedded `--data` hands the executor its argument as the source,
    /// with no catalog pins: a parquet glob, not the server's list.
    #[test]
    fn embedded_data_over_a_parquet_glob() {
        let exec = Executor::new().expect("executor");
        let corpus = Corpus::build(Variant::Absent, Split::ColdOnly);
        let mut matrix = Matrix::default();
        through("glob", &mut matrix, |dsl| {
            exec.run_query(dsl, &corpus.glob(), &FieldTypes::new(), usize::MAX, 0)
        });
        matrix.assert("embedded --data over a parquet glob");
    }

    /// Embedded `--data` over an ndjson file: the `read_json` reader.
    #[test]
    fn embedded_data_over_ndjson() {
        let exec = Executor::new().expect("executor");
        let dir = tempfile::tempdir().expect("ndjson tempdir");
        let path = dir.path().join("events.ndjson");
        let mut text = String::new();
        for event in &EVENTS {
            text.push_str(&Json::Object(event.json(Variant::Absent)).to_string());
            text.push('\n');
        }
        std::fs::write(&path, text).expect("ndjson file");
        let source = path.to_str().expect("utf-8 path");
        let mut matrix = Matrix::default();
        through("ndjson", &mut matrix, |dsl| {
            exec.run_query(dsl, source, &FieldTypes::new(), usize::MAX, 0)
        });
        matrix.assert("embedded --data over ndjson");
    }

    /// The export lane per split, its parquet file read back.
    #[test]
    fn export_read_back() {
        let exec = Executor::new().expect("executor");
        let pins = FieldTypes::new();
        let mut matrix = Matrix::default();
        for split in Split::ALL {
            let corpus = Corpus::build(Variant::Absent, split);
            let mut n = 0;
            through(&format!("export/{}", split.name()), &mut matrix, |dsl| {
                n += 1;
                let out = corpus.path(&format!("export-{n}.parquet"));
                corpus.export(&exec, dsl, &pins, &out)?;
                exec.read_parquet_to_result(&out, usize::MAX)
            });
        }
        matrix.assert("the export lane");
    }

    /// A `timechart on <column>` probes its input relation before the
    /// statement binds. The probe carries the stages before it, so an
    /// absent read there must resolve too. `_ingested` is a bucket column
    /// the live lane refuses, so these are batch only.
    #[test]
    fn timechart_input_probes() {
        check(
            &Case::new("f!=x | timechart on _ingested span=1d count() as n")
                .batch_only()
                .rows(ABSENT, Rows::new(&["n"], one(&[Cell::Int(6)]))),
        );
        check(
            &Case::new("* | where isnull(f) | timechart on _ingested span=1d count() as n by f")
                .batch_only()
                .rows(
                    ABSENT,
                    Rows::new(&["f", "n"], one(&[Cell::Null, Cell::Int(6)])),
                ),
        );
    }

    /// Bucketing an absent field keeps the refusal unless a TIMESTAMP pin
    /// types it, and then every event lands in the null bucket, as the
    /// DSL reference states.
    #[test]
    fn timechart_on_an_absent_field() {
        let exec = Executor::new().expect("executor");
        let dsl = "* | timechart on f span=1d count() as n";
        let mut pinned = FieldTypes::new();
        pinned.insert("f", CanonicalType::Timestamp);
        let null_bucket =
            Answer::Rows(Rows::new(&["_time", "n"], one(&[Cell::Null, Cell::Int(6)])));
        let mut matrix = Matrix::default();
        for split in Split::ALL {
            let corpus = Corpus::build(Variant::Absent, split);
            let refused = match corpus.run(&exec, dsl, &FieldTypes::new()) {
                Err(error)
                    if error
                        .to_string()
                        .contains("timechart on 'f' is not a timestamp") =>
                {
                    Ok(())
                }
                other => Err(format!(
                    "expected the not-a-timestamp refusal, got {other:?}"
                )),
            };
            matrix.judge(format!("unpinned/{}", split.name()), refused);
            matrix.judge(
                format!("timestamp-pinned/{}", split.name()),
                judge_batch(&null_bucket, corpus.run(&exec, dsl, &pinned)),
            );
        }
        matrix.assert(dsl);
    }

    /// The SQL half before `extract kv` reads `f` as NULL, and the Rust
    /// tail after it reads `f` as absent from its rows.
    #[test]
    fn sql_prefix_before_extract_kv() {
        let users = [
            (1, s("alice")),
            (2, s("bob")),
            (3, Cell::Null),
            (4, Cell::Null),
            (5, s("carol")),
            (6, Cell::Null),
        ];
        check(
            &Case::new("f!=x | extract kv | where isnull(f) | table id, user")
                .rows(ABSENT, Rows::new(&["id", "user"], by_id(&users))),
        );
        check(
            &Case::new("* | let y = f | extract kv | table id, y")
                .rows(ABSENT, Rows::new(&["id", "y"], by_id(&nulls(ALL)))),
        );
    }

    /// Two saved runs of `stats count() as n by host` over the base
    /// corpus, written by the saved-result writer, and `run=all`'s reader
    /// over them in the shape `from_saved::resolve_all` builds: neither
    /// run carries `f`.
    fn saved_runs(exec: &Executor, corpus: &Corpus) -> (String, String) {
        let result = corpus
            .run(exec, "* | stats count() as n by host", &FieldTypes::new())
            .expect("the saved query runs");
        let mut parts = Vec::new();
        for run in 1..=2 {
            let path = corpus.path(&format!("run_{run}.parquet"));
            exec.write_query_result_to_parquet(&result, &path)
                .expect("the saved result writes");
            parts.push(format!(
                "SELECT *, {run} AS _run_id, TIMESTAMP '2024-01-1{run} 00:00:00' AS _run_time \
                 FROM read_parquet('{}', union_by_name=true)",
                path.display()
            ));
        }
        let latest = format!(
            "(SELECT * FROM read_parquet('{}', union_by_name=true))",
            corpus.path("run_2.parquet").display()
        );
        (format!("({})", parts.join(" UNION ALL BY NAME ")), latest)
    }

    /// `from saved` hands the executor the run's reader as the source and
    /// the rest of the pipeline after a `*`, with no catalog pins.
    #[test]
    fn from_saved_runs() {
        let exec = Executor::new().expect("executor");
        let corpus = Corpus::build(Variant::Absent, Split::ColdOnly);
        let (all, latest) = saved_runs(&exec, &corpus);
        let pins = FieldTypes::new();
        let host_null = |run: i64| {
            ["web1", "web2", "web3"]
                .iter()
                .map(move |host| vec![Cell::Int(run), s(host), Cell::Null])
        };
        let mut matrix = Matrix::default();
        for (label, dsl, source, answer) in [
            (
                "run=all",
                "* | where isnull(f) | stats sum(n) as total by _run_id",
                &all,
                Rows::new(
                    &["_run_id", "total"],
                    vec![
                        vec![Cell::Int(1), Cell::Int(6)],
                        vec![Cell::Int(2), Cell::Int(6)],
                    ],
                ),
            ),
            (
                "run=all",
                "* | table _run_id, host, f",
                &all,
                Rows::new(
                    &["_run_id", "host", "f"],
                    host_null(1).chain(host_null(2)).collect(),
                )
                .typed("f", ColumnType::String)
                .exactly(&["_run_id", "host", "f"]),
            ),
            (
                "run=all",
                r#"* | where f!="x""#,
                &all,
                Rows::new(&["host"], Vec::new()).without("f"),
            ),
            (
                "run=latest",
                "* | table host, f",
                &latest,
                Rows::new(
                    &["host", "f"],
                    host_rows(&[(1, Cell::Null), (2, Cell::Null), (4, Cell::Null)]),
                )
                .typed("f", ColumnType::String)
                .exactly(&["host", "f"]),
            ),
        ] {
            matrix.judge(
                format!("{label} `{dsl}`"),
                judge_batch(
                    &Answer::Rows(answer),
                    exec.run_query(dsl, source, &pins, usize::MAX, 0),
                ),
            );
        }
        matrix.assert("from saved");
    }

    /// A run that recorded no schema reads through `ZERO_COLUMN_SOURCE`.
    /// Its dummy column is the reader's, never a field, so `table n` is an
    /// empty table with one VARCHAR column `n`.
    #[test]
    fn from_saved_zero_column_source() {
        let exec = Executor::new().expect("executor");
        let source = trawl_core::emitter::ZERO_COLUMN_SOURCE;
        let pins = FieldTypes::new();
        let mut matrix = Matrix::default();
        for (dsl, answer) in [
            (
                "* | table n",
                Rows::new(&["n"], Vec::new())
                    .typed("n", ColumnType::String)
                    .exactly(&["n"]),
            ),
            (
                "* | where isnull(f) | stats count() as c",
                Rows::new(&["c"], one(&[Cell::Int(0)])).exactly(&["c"]),
            ),
        ] {
            matrix.judge(
                format!("`{dsl}`"),
                judge_batch(
                    &Answer::Rows(answer),
                    exec.run_query(dsl, source, &pins, usize::MAX, 0),
                ),
            );
        }
        matrix.assert("ZERO_COLUMN_SOURCE");
    }
}

// ── AC 11: result types ─────────────────────────────────────────────

/// One output column's type, as the query answers it, as the saved-result
/// writer stores it, and as the export writes it.
///
/// The query lane hands back the column's wire type. The saved-result
/// writer and the export each write a parquet file: each file states its
/// column's `DuckDB` type, and read back it must answer the query's wire
/// type. A direct NULL of an unpinned absent field is VARCHAR in all
/// three, and a pinned one is the pin's type in all three.
///
/// Two physical exceptions hold for every column of their kind, present
/// fields included, and each case that meets one names it
/// ([`INTEGER_WIDTH`], [`HUGEINT_IN_PARQUET`]). The `sum(id)` control
/// shows the second is the writer's, not absence's.
mod result_types_agree_across_query_saved_and_export {
    use super::*;

    /// What one column must type as.
    struct Typed {
        dsl: &'static str,
        variant: Variant,
        column: &'static str,
        /// The query's wire type.
        wire: ColumnType,
        saved: Stored,
        export: Stored,
    }

    /// One file's column: its `DuckDB` type, the wire type it reads back
    /// as, and the exception that lets either differ from the query.
    struct Stored {
        ty: &'static str,
        wire: ColumnType,
        why: Option<&'static str>,
    }

    /// A file that stores the column as `ty` and reads back as the query's
    /// wire type `wire`.
    fn agrees(ty: &'static str, wire: ColumnType) -> Stored {
        Stored {
            ty,
            wire,
            why: None,
        }
    }

    /// The saved-result writer stores each wire type in one exact width:
    /// `integer` as BIGINT (UBIGINT when a cell needs it), whatever width
    /// the statement had. The wire type is the query's.
    const INTEGER_WIDTH: &str = "the saved-result writer stores an `integer` column as BIGINT";

    /// Parquet has no 128-bit integer, so `DuckDB`'s parquet writer stores
    /// a HUGEINT column, which every integer `sum` is, as DOUBLE, and the
    /// export reads back `float` where the query answered `integer`.
    const HUGEINT_IN_PARQUET: &str = "parquet stores a HUGEINT column as DOUBLE";

    /// Every column's `DuckDB` type in a parquet file.
    fn file_types(path: &Path) -> Vec<(String, String)> {
        let conn = duckdb::Connection::open_in_memory().expect("reader database");
        let mut stmt = conn
            .prepare(&format!(
                "DESCRIBE SELECT * FROM read_parquet('{}')",
                path.display()
            ))
            .expect("describe a parquet file");
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("describe rows")
            .collect::<Result<_, _>>()
            .expect("describe rows read")
    }

    fn wire_type(result: &QueryResult, column: &str) -> Result<&'static str, String> {
        result
            .columns
            .iter()
            .find(|c| c.name == column)
            .map(|c| c.ty.name())
            .ok_or_else(|| format!("no `{column}` column"))
    }

    fn expect_eq(what: &str, got: &str, want: &str) -> Result<(), String> {
        if got == want {
            Ok(())
        } else {
            Err(format!("{what}: expected {want}, got {got}"))
        }
    }

    /// One file against its [`Stored`] answer.
    fn judge_file(
        exec: &Executor,
        what: &str,
        path: &Path,
        column: &str,
        want: &Stored,
    ) -> Result<(), String> {
        let types = file_types(path);
        let ty = types
            .iter()
            .find(|(name, _)| name == column)
            .map(|(_, ty)| ty.as_str())
            .ok_or_else(|| format!("{what}: no `{column}` in {types:?}"))?;
        expect_eq(&format!("{what} file type"), ty, want.ty)?;
        let back = exec
            .read_parquet_to_result(path, usize::MAX)
            .map_err(|e| format!("{what} read back: {e}"))?;
        expect_eq(
            &format!("{what} wire type read back"),
            wire_type(&back, column)?,
            want.wire.name(),
        )
    }

    /// The three answers for one split.
    fn judge(exec: &Executor, case: &Typed, split: Split) -> Result<(), String> {
        let corpus = Corpus::build(case.variant, split);
        let pins = case.variant.pins();

        let query = corpus
            .run(exec, case.dsl, &pins)
            .map_err(|e| format!("query: {e}"))?;
        expect_eq(
            "query wire type",
            wire_type(&query, case.column)?,
            case.wire.name(),
        )?;

        let saved = corpus.path("saved.parquet");
        exec.write_query_result_to_parquet(&query, &saved)
            .map_err(|e| format!("saved write: {e}"))?;
        judge_file(exec, "saved", &saved, case.column, &case.saved)?;

        let export = corpus.path("export.parquet");
        corpus
            .export(exec, case.dsl, &pins, &export)
            .map_err(|e| format!("export: {e}"))?;
        judge_file(exec, "export", &export, case.column, &case.export)
    }

    fn cases() -> Vec<Typed> {
        let text = || agrees("VARCHAR", ColumnType::String);
        let double = || agrees("DOUBLE", ColumnType::Float);
        let integer_sum = || Typed {
            dsl: "",
            variant: Variant::Absent,
            column: "total",
            wire: ColumnType::Integer,
            saved: Stored {
                ty: "BIGINT",
                wire: ColumnType::Integer,
                why: Some(INTEGER_WIDTH),
            },
            export: Stored {
                ty: "DOUBLE",
                wire: ColumnType::Float,
                why: Some(HUGEINT_IN_PARQUET),
            },
        };
        vec![
            // A direct NULL of an unpinned absent field is VARCHAR in all
            // three.
            Typed {
                dsl: "* | table f",
                variant: Variant::Absent,
                column: "f",
                wire: ColumnType::String,
                saved: text(),
                export: text(),
            },
            Typed {
                dsl: "* | let y = f",
                variant: Variant::Absent,
                column: "y",
                wire: ColumnType::String,
                saved: text(),
                export: text(),
            },
            // A pinned absent field keeps the pin's type.
            Typed {
                dsl: "* | table f",
                variant: Variant::PinnedDouble,
                column: "f",
                wire: ColumnType::Float,
                saved: double(),
                export: double(),
            },
            Typed {
                dsl: "* | let y = f",
                variant: Variant::PinnedDouble,
                column: "y",
                wire: ColumnType::Float,
                saved: double(),
                export: double(),
            },
            // An expression keeps the type `DuckDB` gives it: `coalesce` of
            // a NULL and the integer literal, which binds as a BIGINT
            // parameter.
            Typed {
                dsl: "* | let y = coalesce(f, 0)",
                variant: Variant::Absent,
                column: "y",
                wire: ColumnType::Integer,
                saved: agrees("BIGINT", ColumnType::Integer),
                export: agrees("BIGINT", ColumnType::Integer),
            },
            // `sum` of an untyped NULL is HUGEINT, as `sum` of any integer
            // is; the present `id` is the control.
            Typed {
                dsl: "* | stats sum(f) as total",
                ..integer_sum()
            },
            Typed {
                dsl: "* | stats sum(id) as total",
                ..integer_sum()
            },
            // Under a DOUBLE pin, `sum` is DOUBLE and meets neither
            // exception.
            Typed {
                dsl: "* | stats sum(f) as total",
                variant: Variant::PinnedDouble,
                column: "total",
                wire: ColumnType::Float,
                saved: double(),
                export: double(),
            },
        ]
    }

    #[test]
    fn every_case_on_every_split() {
        let exec = Executor::new().expect("executor");
        let mut matrix = Matrix::default();
        for case in cases() {
            // Agreement is the rule, so a departure is stated with its
            // reason: a file that reads back another wire type than the
            // query's, and two files that store different types.
            for (what, file) in [("saved", &case.saved), ("export", &case.export)] {
                assert!(
                    file.wire == case.wire || file.why.is_some(),
                    "`{}`: the {what} file reads back another wire type with no exception named",
                    case.dsl
                );
            }
            assert!(
                case.saved.ty == case.export.ty
                    || (case.saved.why.is_some() && case.export.why.is_some()),
                "`{}`: the saved and export files store different types with no exception named",
                case.dsl
            );
            for split in Split::ALL {
                matrix.judge(
                    format!("{} `{}` {}", case.variant.name(), case.dsl, split.name()),
                    judge(&exec, &case, split),
                );
            }
        }
        matrix.assert("result types");
    }
}

// ── AC 14: one null identity, live and batch ────────────────────────

/// Absent and explicit null are one identity in every lane, apart from
/// the empty string and the text `null`, and an aggregate over no
/// eligible value is NULL. Every case runs live and over every batch
/// split, with exact typed cells, so the lanes agree by stating the same
/// answer.
///
/// [`Variant::NullIdentity`] carries `""` on id 1, `"null"` on id 2, an
/// explicit null on id 5, and nothing on ids 3, 4 and 6.
mod live_null_identity_matches_batch {
    use super::*;

    /// `table` writes the null cell for a requested name the event lacks.
    #[test]
    fn table_writes_the_null_cell() {
        let rows = |cells: &[(i64, Cell)]| {
            Rows::new(&["service", "f"], service_rows(cells))
                .typed("f", ColumnType::String)
                .exactly(&["service", "f"])
        };
        check(
            &Case::new("* | table service, f")
                .rows(IDENTITY, rows(&identity_f()))
                .rows(ABSENT, rows(&nulls(ALL))),
        );
    }

    #[test]
    fn stats_by() {
        check(
            &Case::new("* | stats count() as n by f")
                .rows(IDENTITY, Rows::new(&["f", "n"], identity_groups(4)))
                .rows(
                    LAW,
                    Rows::new(&["f", "n"], one(&[Cell::Null, Cell::Int(6)])),
                ),
        );
    }

    /// A second key splits the null identity by host and no further.
    #[test]
    fn stats_by_two_keys() {
        let rows = [
            ("web1", s(""), 1),
            ("web2", s("null"), 1),
            ("web1", Cell::Null, 2),
            ("web3", Cell::Null, 1),
            ("web2", Cell::Null, 1),
        ]
        .iter()
        .map(|(host, f, n)| vec![s(host), f.clone(), Cell::Int(*n)])
        .collect();
        check(
            &Case::new("* | stats count() as n by host, f")
                .rows(IDENTITY, Rows::new(&["host", "f", "n"], rows)),
        );
    }

    #[test]
    fn top_and_rare() {
        for dsl in ["* | top 10 f", "* | rare 10 f"] {
            check(
                &Case::new(dsl)
                    .rows(IDENTITY, Rows::new(&["f", "count"], identity_groups(4)))
                    .rows(
                        LAW,
                        Rows::new(&["f", "count"], one(&[Cell::Null, Cell::Int(6)])),
                    ),
            );
        }
    }

    /// One day bucket holds every event.
    #[test]
    fn timechart_by() {
        check(
            &Case::new("* | timechart span=1d count() as n by f")
                .rows(IDENTITY, Rows::new(&["f", "n"], identity_groups(4))),
        );
    }

    /// `dedup f` keeps one row per identity: three. `table f` writes the
    /// kept row's cell, the null identity's as NULL.
    #[test]
    fn dedup() {
        check(&Case::new("* | dedup f | table f").rows(
            IDENTITY,
            Rows::new(&["f"], vec![vec![s("")], vec![s("null")], vec![Cell::Null]]),
        ));
    }

    /// `sum`, `avg`, `min` and `max` over no eligible value are NULL, and
    /// `count` is 0, over the whole input and per group.
    #[test]
    fn aggregates_over_no_value_are_null() {
        let empty = [Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Int(0)];
        check(
            &Case::new(
                "* | stats sum(f) as total, avg(f) as mean, min(f) as lo, max(f) as hi, \
                 count(f) as c",
            )
            .rows(
                LAW_NUM,
                Rows::new(&["total", "mean", "lo", "hi", "c"], one(&empty)),
            ),
        );
        let per_host = ["web1", "web2", "web3"]
            .iter()
            .map(|host| std::iter::once(s(host)).chain(empty.clone()).collect())
            .collect();
        check(
            &Case::new(
                "* | stats sum(f) as total, avg(f) as mean, min(f) as lo, max(f) as hi, \
                 count(f) as c by host",
            )
            .rows(
                LAW_NUM,
                Rows::new(&["host", "total", "mean", "lo", "hi", "c"], per_host),
            ),
        );
    }
}
