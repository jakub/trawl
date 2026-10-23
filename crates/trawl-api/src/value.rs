// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Result types for query execution.
//!
//! These types are deliberately decoupled from `DuckDB`'s internal types
//! to keep the public API stable across duckdb crate version changes.
//!
//! A [`QueryResult`] is the wire format of every API result surface
//! (ADR-0035, amended 2026-10-22). Each [`Column`] carries a
//! [`ColumnType`], and every cell is encoded and decoded under it:
//! integers travel as canonical decimal strings, floats as JSON numbers
//! with `NaN`, `Infinity` and `-Infinity` spelled as strings, and each
//! non-null cell of a `mixed` column carries its own type tag. [`Value`]
//! has no serde of its own, so a cell reaches the wire only through its
//! column's type. Export files want plain JSON numbers instead and ask for
//! them by name with [`Value::to_plain_json`].

use std::cmp::Ordering;
use std::fmt;

use serde::de::Error as _;
use serde::ser::{Error as _, SerializeMap, SerializeSeq, SerializeStruct};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A cell value from a query result row.
///
/// Integers land in the narrowest variant that holds them exactly:
/// [`Integer`](Self::Integer) up to `i64::MAX`, then [`UInt`](Self::UInt)
/// for the unsigned range above it — see [`land_u64`] and [`land_i128`],
/// the one rule every producer and decoder applies. Nothing rounds to
/// `f64` on the way: a rounded reading and an exact one are different
/// answers. A magnitude outside `i64` ∪ `u64` has no variant, and its
/// producer refuses it instead of carrying digits or a float.
///
/// Timestamps, dates and times are [`String`](Self::String) cells; their
/// column's [`ColumnType`] says which. A `Value` has no wire spelling of
/// its own: [`QueryResult`]'s serde encodes each cell under its column
/// type.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Boolean(bool),
    Integer(i64),
    /// An integer above `i64::MAX`. Never holds a magnitude
    /// [`Integer`](Self::Integer) could carry: [`land_u64`] picks the
    /// narrower variant first, so the two never spell the same number.
    UInt(u64),
    Float(f64),
    String(String),
    Array(Vec<Value>),
}

/// Land a `u64` without rounding it.
///
/// Up to `i64::MAX` the value is a [`Value::Integer`]; above that it is a
/// [`Value::UInt`]. Both are `integer` cells on the wire, the rule
/// `trawl-engine`'s executor applies to a `DuckDB` `UBIGINT`. Never a
/// float and never text: a reader sorts, charts and compares a number by
/// its variant, and a rounded reading is a different answer from an exact
/// one.
#[must_use]
pub fn land_u64(v: u64) -> Value {
    i64::try_from(v).map_or(Value::UInt(v), Value::Integer)
}

/// Land an `i128` without rounding it, the signed twin of [`land_u64`]
/// (`DuckDB`'s `HUGEINT`).
///
/// Inside `i64` range the value is a [`Value::Integer`], and inside the
/// unsigned range above it a [`Value::UInt`]. A magnitude past `u64::MAX`,
/// or any negative past `i64::MIN`, has no variant to hold it: `None`, and
/// the caller refuses the cell by its column name. A checked narrowing,
/// never digits as text and never a float.
#[must_use]
pub fn land_i128(v: i128) -> Option<Value> {
    if let Ok(i) = i64::try_from(v) {
        Some(Value::Integer(i))
    } else if let Ok(u) = u64::try_from(v) {
        Some(Value::UInt(u))
    } else {
        None
    }
}

impl Value {
    /// This cell as plain JSON, for export files.
    ///
    /// Export files are not the wire (ADR-0035, amended 2026-10-22): a JSON
    /// or NDJSON export keeps integers as JSON numbers, exact for both
    /// variants, because jq and Vector read them natively and a quoted
    /// number becomes a label there. A non-finite float is `null`, the
    /// spelling JSON exports have always used. An API result response never
    /// goes through here; [`QueryResult`]'s serde is the wire.
    #[must_use]
    pub fn to_plain_json(&self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Boolean(b) => serde_json::Value::Bool(*b),
            Self::Integer(i) => serde_json::Value::from(*i),
            Self::UInt(u) => serde_json::Value::from(*u),
            Self::Float(f) => {
                serde_json::Number::from_f64(*f).map_or(serde_json::Value::Null, Into::into)
            }
            Self::String(s) => serde_json::Value::String(s.clone()),
            Self::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(Self::to_plain_json).collect())
            }
        }
    }

    /// The variant's name, for a refusal that has to say what it found.
    fn kind(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Boolean(_) => "boolean",
            Self::Integer(_) | Self::UInt(_) => "integer",
            Self::Float(_) => "float",
            Self::String(_) => "string",
            Self::Array(_) => "list",
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => Ok(()),
            Self::Boolean(b) => write!(f, "{b}"),
            Self::Integer(i) => write!(f, "{i}"),
            Self::UInt(u) => write!(f, "{u}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::String(s) => write!(f, "{s}"),
            Self::Array(arr) => {
                let items: Vec<std::string::String> = arr.iter().map(ToString::to_string).collect();
                write!(f, "[{}]", items.join(", "))
            }
        }
    }
}

/// The type of a result column (ADR-0035, amended 2026-10-22).
///
/// A closed set. `integer` covers `i64` and `u64` together: its cells are
/// canonical decimal strings, landed by range into [`Value::Integer`] and
/// [`Value::UInt`]. `timestamp`, `date` and `time` cells are text. A
/// `list` names its element type in `items`, recursively. `mixed` is for a
/// column a Rust-side stage produced whose cells are not of one type; each
/// of its non-null cells carries its own tag.
///
/// Spelled as an object, `{"type":"integer"}` or
/// `{"type":"list","items":{"type":"float"}}`, so [`Column`] flattens it
/// beside the name and a mixed cell's tag reuses the same shape. A missing
/// or unknown `type` does not decode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ColumnType {
    Boolean,
    Integer,
    Float,
    String,
    Timestamp,
    Date,
    Time,
    List { items: Box<ColumnType> },
    Mixed,
}

impl ColumnType {
    /// The wire name, the `type` member's value.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Boolean => "boolean",
            Self::Integer => "integer",
            Self::Float => "float",
            Self::String => "string",
            Self::Timestamp => "timestamp",
            Self::Date => "date",
            Self::Time => "time",
            Self::List { .. } => "list",
            Self::Mixed => "mixed",
        }
    }

    /// A list of `items`.
    #[must_use]
    pub fn list(items: Self) -> Self {
        Self::List {
            items: Box::new(items),
        }
    }

    /// The type a single cell shows by its own variant, or `None` for a
    /// null, which shows none. Text is `string`: a cell cannot tell a
    /// timestamp from a label, only its producer can.
    #[must_use]
    pub fn of_value(value: &Value) -> Option<Self> {
        let mut fold = TypeFold::new();
        fold.observe_value(value);
        fold.finish()
    }
}

impl fmt::Display for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::List { items } => write!(f, "list<{items}>"),
            other => f.write_str(other.name()),
        }
    }
}

/// Evidence for a column's type, folded cell by cell.
///
/// One type of cell gives that type, cells of different types give
/// [`ColumnType::Mixed`], and a null adds nothing. A list's elements fold
/// the same way into its `items`, so an empty list is a list whose items
/// have no evidence yet, and lists of integers and lists of floats make
/// `list<mixed>`. A column with no evidence at all has no type of its own;
/// [`finish`](Self::finish) says so with `None` and the caller decides
/// (`string` by the ADR's no-evidence rule, or the type a Rust tail's
/// static output schema carries for the column).
#[derive(Debug, Clone, Default)]
pub struct TypeFold(Option<Shape>);

/// [`ColumnType`] with room for a list whose items have no evidence yet.
#[derive(Debug, Clone, PartialEq)]
enum Shape {
    Leaf(ColumnType),
    List(Option<Box<Shape>>),
    Mixed,
}

impl Shape {
    fn of(ty: &ColumnType) -> Self {
        match ty {
            ColumnType::List { items } => Self::List(Some(Box::new(Self::of(items)))),
            ColumnType::Mixed => Self::Mixed,
            leaf => Self::Leaf(leaf.clone()),
        }
    }

    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::List(a), Self::List(b)) => Self::List(match (a, b) {
                (None, items) | (items, None) => items,
                (Some(a), Some(b)) => Some(Box::new(a.merge(*b))),
            }),
            (a, b) if a == b => a,
            _ => Self::Mixed,
        }
    }

    /// The folded type, with `carried` filling each hole the evidence
    /// left: a list with no item evidence takes the carried list's items,
    /// at any depth, and an evidenced leaf ignores `carried`.
    fn finish_with(self, carried: Option<&ColumnType>) -> ColumnType {
        match self {
            Self::Leaf(ty) => ty,
            Self::List(items) => {
                let carried = match carried {
                    Some(ColumnType::List { items }) => Some(items.as_ref()),
                    _ => None,
                };
                ColumnType::list(match items {
                    Some(items) => items.finish_with(carried),
                    None => carried.cloned().unwrap_or(ColumnType::String),
                })
            }
            Self::Mixed => ColumnType::Mixed,
        }
    }
}

impl TypeFold {
    #[must_use]
    pub fn new() -> Self {
        Self(None)
    }

    /// A non-null cell of exactly `ty`.
    pub fn observe(&mut self, ty: &ColumnType) {
        self.add(Shape::of(ty));
    }

    /// A list cell whose elements folded to `items`.
    pub fn observe_list(&mut self, items: Self) {
        self.add(Shape::List(items.0.map(Box::new)));
    }

    /// A cell, typed by its own variant ([`ColumnType::of_value`]).
    pub fn observe_value(&mut self, value: &Value) {
        let ty = match value {
            Value::Null => return,
            Value::Boolean(_) => ColumnType::Boolean,
            Value::Integer(_) | Value::UInt(_) => ColumnType::Integer,
            Value::Float(_) => ColumnType::Float,
            Value::String(_) => ColumnType::String,
            Value::Array(elements) => {
                let mut items = Self::new();
                for element in elements {
                    items.observe_value(element);
                }
                self.observe_list(items);
                return;
            }
        };
        self.observe(&ty);
    }

    /// A cell of a column a result typed `ty`, for merging typed results
    /// (the live ring's frames, the TUI's live buffer).
    ///
    /// The column's type is the cell's evidence, with two refinements: a
    /// null adds nothing, whatever its column says (a one-row frame types
    /// a null cell `string` for want of evidence), and a list's items fold
    /// element by element, so an empty list adds no item evidence.
    pub fn observe_typed(&mut self, ty: &ColumnType, value: &Value) {
        match (ty, value) {
            (_, Value::Null) => {}
            (ColumnType::List { items }, Value::Array(elements)) => {
                let mut fold = Self::new();
                for element in elements {
                    fold.observe_typed(items, element);
                }
                self.observe_list(fold);
            }
            (ty, _) => self.observe(ty),
        }
    }

    /// Another fold's evidence, as if its cells were observed here.
    pub fn merge(&mut self, other: Self) {
        if let Some(shape) = other.0 {
            self.add(shape);
        }
    }

    /// Whether any non-null cell was observed.
    #[must_use]
    pub fn has_evidence(&self) -> bool {
        self.0.is_some()
    }

    /// The folded type, or `None` when no non-null cell was observed. A
    /// list whose items had no evidence is a list of `string`.
    #[must_use]
    pub fn finish(self) -> Option<ColumnType> {
        self.finish_with(None)
    }

    /// [`finish`](Self::finish) with the type a caller knows the column
    /// to carry (a Rust tail's static output schema) filling what the
    /// cells left open: no evidence at all gives `carried`, and a list
    /// with no item evidence, at any depth, takes the carried list's
    /// items. Evidence wins where it exists.
    #[must_use]
    pub fn finish_with(self, carried: Option<&ColumnType>) -> Option<ColumnType> {
        match self.0 {
            Some(shape) => Some(shape.finish_with(carried)),
            None => carried.cloned(),
        }
    }

    fn add(&mut self, shape: Shape) {
        self.0 = Some(match self.0.take() {
            None => shape,
            Some(current) => current.merge(shape),
        });
    }
}
/// Fields that appear first in reordered query results (ADR-0009 envelope).
///
/// When a query has no explicit column selection (`table`/`fields`) and no
/// aggregation (`stats`/`top`/etc.), columns are reordered so these appear
/// first in this order, followed by remaining columns in their original
/// order, with [`TRAILING_LOG_FIELDS`] demoted to the very end.
///
/// Kept in sync with `trawl_core::schema::LEADING_LOG_FIELDS` (duplicated
/// because trawl-api does not depend on trawl-core; a trawl-engine test
/// asserts parity).
pub const WELL_KNOWN_LOG_FIELDS: &[&str] =
    &["_time", "env", "service", "host", "_severity", "message"];

/// Envelope metadata columns demoted to the end of reordered results.
///
/// Kept in sync with `trawl_core::schema::TRAILING_LOG_FIELDS`.
pub const TRAILING_LOG_FIELDS: &[&str] = &["_raw", "_ingested", "_repairs", "_producer"];

/// Display rank for a field name, mirroring query-result column order:
/// `0` = leading envelope field ([`WELL_KNOWN_LOG_FIELDS`], in declared
/// order), `1` = custom field, `2` = trailing metadata
/// ([`TRAILING_LOG_FIELDS`], in declared order).
///
/// Callers do not sort on this directly — [`sort_by_display_rank`] owns the
/// `(rank, name)` ordering every schema listing shares.
#[must_use]
pub fn field_display_rank(name: &str) -> (u8, usize) {
    if let Some(i) = WELL_KNOWN_LOG_FIELDS.iter().position(|f| *f == name) {
        (0, i)
    } else if let Some(i) = TRAILING_LOG_FIELDS.iter().position(|f| *f == name) {
        (2, i)
    } else {
        (1, 0)
    }
}

/// Compare two field names in schema display order: [`field_display_rank`]
/// first, name as the tie-break.
#[must_use]
pub fn cmp_by_display_rank(a: &str, b: &str) -> Ordering {
    field_display_rank(a)
        .cmp(&field_display_rank(b))
        .then_with(|| a.cmp(b))
}

/// Sort a schema listing into display order, keying each item by its field
/// name via `name`.
///
/// The single home of this ordering, so the `/api/v1/schema` columns,
/// `/api/v1/schema/fields` rows and `/api/v1/schema/services` columns cannot
/// drift apart, and the CLI tables that print those rows inherit it: they
/// present columns the way query results order them, instead of a raw
/// alphabetical sort that would put `_ingested` first.
pub fn sort_by_display_rank<T>(items: &mut [T], name: impl Fn(&T) -> &str) {
    items.sort_by(|a, b| cmp_by_display_rank(name(a), name(b)));
}

/// A result column: its name and its [`ColumnType`].
///
/// The type is flattened beside the name on the wire,
/// `{"name":"status","type":"integer"}`, and required: a column without
/// one does not decode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    #[serde(flatten)]
    pub ty: ColumnType,
}

impl Column {
    #[must_use]
    pub fn new(name: impl Into<String>, ty: ColumnType) -> Self {
        Self {
            name: name.into(),
            ty,
        }
    }
}

/// The complete result of a query execution.
///
/// Its serde is the result wire: see the module docs, [`ColumnType`] and
/// [`CellError`].
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
}

impl QueryResult {
    /// An empty result set with no columns or rows.
    ///
    /// The answer when a query's source matches no parquet file: no data is
    /// not an error.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
        }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Whether the result carries no rows (columns are not consulted).
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Reorder columns for log display: [`WELL_KNOWN_LOG_FIELDS`] first,
    /// [`TRAILING_LOG_FIELDS`] last, everything else in between in its
    /// original order.
    pub fn reorder_log_columns(&mut self) {
        self.reorder_columns_full(WELL_KNOWN_LOG_FIELDS, TRAILING_LOG_FIELDS);
    }

    /// Reorder columns so `preferred` field names appear first.
    ///
    /// Fields in `preferred` that exist in the result are moved to the front
    /// (in the order they appear in `preferred`), followed by all remaining
    /// columns in their original order. Missing fields are silently skipped.
    ///
    /// Both `columns` and every row in `rows` are permuted together.
    pub fn reorder_columns(&mut self, preferred: &[&str]) {
        self.reorder_columns_full(preferred, &[]);
    }

    /// Reorder columns: `preferred` first, `trailing` demoted to the end,
    /// remaining columns in their original order between them.
    pub fn reorder_columns_full(&mut self, preferred: &[&str], trailing: &[&str]) {
        if self.columns.is_empty() {
            return;
        }

        let mut order: Vec<usize> = Vec::with_capacity(self.columns.len());
        let mut used = vec![false; self.columns.len()];

        for &pref in preferred {
            if let Some(idx) = self.columns.iter().position(|c| c.name == pref)
                && !used[idx]
            {
                order.push(idx);
                used[idx] = true;
            }
        }
        for (i, &u) in used.iter().enumerate() {
            if !u && !trailing.contains(&self.columns[i].name.as_str()) {
                order.push(i);
            }
        }
        for &tr in trailing {
            if let Some(idx) = self.columns.iter().position(|c| c.name == tr)
                && !used[idx]
            {
                order.push(idx);
                used[idx] = true;
            }
        }

        // Skip allocation if already in order.
        if order.iter().enumerate().all(|(new, &old)| new == old) {
            return;
        }

        self.columns = order.iter().map(|&i| self.columns[i].clone()).collect();
        for row in &mut self.rows {
            let orig = std::mem::take(row);
            *row = order.iter().map(|&i| orig[i].clone()).collect();
        }
    }

    /// Apply offset/limit pagination to result rows (post-executor).
    ///
    /// This is a convenience method for client-side pagination. If `offset`
    /// exceeds the number of rows, the result will be empty. The `limit` is
    /// capped at the remaining rows after the offset.
    ///
    /// Uses in-place operations (no new Vec allocation): `drain(..offset)`
    /// removes leading elements, then `truncate(limit)` caps the remainder.
    #[must_use]
    pub fn paginate(mut self, offset: usize, limit: usize) -> Self {
        if offset >= self.rows.len() {
            self.rows.clear();
        } else {
            drop(self.rows.drain(..offset));
            self.rows.truncate(limit);
        }
        self
    }
}
// -- the result wire -----------------------------------------------------------

/// A cell that does not fit its column's type, refused by the encoder or
/// the decoder. Rendered as `column "status": …`, so the refusal names the
/// column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellError {
    /// The column the cell belongs to.
    pub column: String,
    /// What was wrong with it.
    pub reason: String,
}

impl fmt::Display for CellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "column {:?}: {}", self.column, self.reason)
    }
}

impl std::error::Error for CellError {}

/// The encoder: `columns` as they are, then every cell under its column's
/// type.
///
/// The encoder also checks that each cell's variant agrees with its column
/// type and refuses one that does not, naming the column. That check is a
/// backstop: producers type their columns from the cells they emit, and a
/// disagreement is a producer bug, never a value to spell some other way.
impl Serialize for QueryResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut result = serializer.serialize_struct("QueryResult", 2)?;
        result.serialize_field("columns", &self.columns)?;
        result.serialize_field(
            "rows",
            &RowsRef {
                columns: &self.columns,
                rows: &self.rows,
            },
        )?;
        result.end()
    }
}

struct RowsRef<'a> {
    columns: &'a [Column],
    rows: &'a [Vec<Value>],
}

impl Serialize for RowsRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut rows = serializer.serialize_seq(Some(self.rows.len()))?;
        for (index, row) in self.rows.iter().enumerate() {
            if row.len() != self.columns.len() {
                return Err(S::Error::custom(ragged(
                    index,
                    row.len(),
                    self.columns.len(),
                )));
            }
            rows.serialize_element(&RowRef {
                columns: self.columns,
                row,
            })?;
        }
        rows.end()
    }
}

struct RowRef<'a> {
    columns: &'a [Column],
    row: &'a [Value],
}

impl Serialize for RowRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(
            self.columns
                .iter()
                .zip(self.row)
                .map(|(column, value)| CellRef {
                    column: &column.name,
                    ty: &column.ty,
                    value,
                }),
        )
    }
}

/// One cell, encoded under `ty`.
struct CellRef<'a> {
    column: &'a str,
    ty: &'a ColumnType,
    value: &'a Value,
}

impl Serialize for CellRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match (self.ty, self.value) {
            (_, Value::Null) => serializer.serialize_unit(),
            (ColumnType::Integer, Value::Integer(i)) => serializer.collect_str(i),
            (ColumnType::Integer, Value::UInt(u)) => serializer.collect_str(u),
            (ColumnType::Float, Value::Float(f)) => serialize_float(*f, serializer),
            (ColumnType::Boolean, Value::Boolean(b)) => serializer.serialize_bool(*b),
            (
                ColumnType::String | ColumnType::Timestamp | ColumnType::Date | ColumnType::Time,
                Value::String(text),
            ) => serializer.serialize_str(text),
            (ColumnType::List { items }, Value::Array(elements)) => {
                serializer.collect_seq(elements.iter().map(|value| CellRef {
                    column: self.column,
                    ty: items,
                    value,
                }))
            }
            (ColumnType::Mixed, value) => {
                let Some(tag) = ColumnType::of_value(value) else {
                    return serializer.serialize_unit();
                };
                let mut cell = serializer.serialize_map(None)?;
                cell.serialize_entry("type", tag.name())?;
                if let ColumnType::List { items } = &tag {
                    cell.serialize_entry("items", items)?;
                }
                cell.serialize_entry(
                    "value",
                    &CellRef {
                        column: self.column,
                        ty: &tag,
                        value,
                    },
                )?;
                cell.end()
            }
            (ty, value) => Err(S::Error::custom(CellError {
                column: self.column.to_owned(),
                reason: format!("a {} cell cannot be encoded as {ty}", value.kind()),
            })),
        }
    }
}

/// A float as a JSON number, or one of the three strings JSON has no
/// number for.
fn serialize_float<S: Serializer>(f: f64, serializer: S) -> Result<S::Ok, S::Error> {
    if f.is_nan() {
        serializer.serialize_str("NaN")
    } else if f.is_infinite() {
        serializer.serialize_str(if f.is_sign_positive() {
            "Infinity"
        } else {
            "-Infinity"
        })
    } else {
        serializer.serialize_f64(f)
    }
}

fn ragged(row: usize, cells: usize, columns: usize) -> String {
    format!("row {row} has {cells} cells for {columns} columns")
}

/// The wire shape before the cells are landed: types are only known once
/// `columns` is read, and inside a `#[serde(flatten)]` parent (the query
/// and run responses) serde buffers the whole body anyway, so reading the
/// rows as plain JSON first and landing them second costs nothing a
/// streaming decode would save. `serde_json::Value` keeps a `u64` exact on
/// the way through that buffer.
#[derive(Deserialize)]
#[serde(rename = "QueryResult")]
struct WireResult {
    columns: Vec<Column>,
    rows: Vec<Vec<serde_json::Value>>,
}

/// The decoder: one schema-directed landing for every result consumer, so
/// the client, CLI, TUI and SPA all classify cells by the variants it
/// lands. A cell that does not fit its column's type is refused with the
/// column named ([`CellError`]); nothing is guessed from a cell's spelling.
impl<'de> Deserialize<'de> for QueryResult {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let WireResult { columns, rows } = WireResult::deserialize(deserializer)?;
        let rows = rows
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                if row.len() != columns.len() {
                    return Err(D::Error::custom(ragged(index, row.len(), columns.len())));
                }
                columns
                    .iter()
                    .zip(row)
                    .map(|(column, cell)| {
                        land_cell(&column.ty, cell).map_err(|reason| {
                            D::Error::custom(CellError {
                                column: column.name.clone(),
                                reason,
                            })
                        })
                    })
                    .collect()
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { columns, rows })
    }
}

/// Land one wire cell under `ty`, or say why it does not fit.
fn land_cell(ty: &ColumnType, cell: serde_json::Value) -> Result<Value, String> {
    use serde_json::Value as Json;

    match (ty, cell) {
        (_, Json::Null) => Ok(Value::Null),
        (ColumnType::Integer, Json::String(text)) => land_decimal(&text),
        (ColumnType::Integer, Json::Number(n)) => Err(format!(
            "an integer cell is a decimal string, not the JSON number {n}"
        )),
        (ColumnType::Float, Json::Number(n)) => n
            .as_f64()
            .map(Value::Float)
            .ok_or_else(|| format!("{n} is not a float")),
        (ColumnType::Float, Json::String(text)) => match text.as_str() {
            "NaN" => Ok(Value::Float(f64::NAN)),
            "Infinity" => Ok(Value::Float(f64::INFINITY)),
            "-Infinity" => Ok(Value::Float(f64::NEG_INFINITY)),
            _ => Err(format!(
                "{text:?} is not a float; only NaN, Infinity and -Infinity are strings"
            )),
        },
        (ColumnType::Boolean, Json::Bool(b)) => Ok(Value::Boolean(b)),
        (ColumnType::Timestamp, Json::String(text)) => {
            if is_timestamp_text(&text) {
                Ok(Value::String(text))
            } else {
                Err(format!("{text:?} is not an RFC 3339 timestamp"))
            }
        }
        (ColumnType::String | ColumnType::Date | ColumnType::Time, Json::String(text)) => {
            Ok(Value::String(text))
        }
        (ColumnType::List { items }, Json::Array(elements)) => elements
            .into_iter()
            .enumerate()
            .map(|(index, element)| {
                land_cell(items, element).map_err(|reason| format!("item {index}: {reason}"))
            })
            .collect::<Result<_, _>>()
            .map(Value::Array),
        (ColumnType::Mixed, Json::Object(mut tagged)) => {
            let value = tagged
                .remove("value")
                .ok_or_else(|| "a mixed cell has no value".to_owned())?;
            if value.is_null() {
                return Err("a null mixed cell is a bare null, not a tagged one".to_owned());
            }
            let tag: ColumnType = serde_json::from_value(Json::Object(tagged))
                .map_err(|e| format!("a mixed cell's tag: {e}"))?;
            if tag == ColumnType::Mixed {
                return Err("a mixed cell's tag names its own type, never mixed".to_owned());
            }
            land_cell(&tag, value)
        }
        (ColumnType::Mixed, other) => Err(format!(
            "a mixed cell is a {{\"type\", \"value\"}} object, not {}",
            json_kind(&other)
        )),
        (ty, other) => Err(format!("{} is not a {ty} cell", json_kind(&other))),
    }
}

/// Land a decimal-string integer by range: `i64` first, then the unsigned
/// range above it, else refuse.
///
/// Only the canonical spelling `-?(0|[1-9][0-9]*)` decodes, exactly what
/// the encoder writes: no sign on a positive, no leading zeros, no `-0`,
/// no whitespace. A lax reader would let hand-written clients drift into
/// spellings other readers take differently.
fn land_decimal(text: &str) -> Result<Value, String> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let canonical = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'))
        && text != "-0";
    if !canonical {
        return Err(format!("{text:?} is not a canonical decimal integer"));
    }
    if let Ok(i) = text.parse::<i64>() {
        Ok(Value::Integer(i))
    } else if let Ok(u) = text.parse::<u64>() {
        Ok(Value::UInt(u))
    } else {
        Err(format!("{text} is outside the integer range (i64 and u64)"))
    }
}

/// Whether `text` spells a `timestamp` cell: RFC 3339, or `infinity` and
/// `-infinity` for `DuckDB`'s infinite timestamps.
///
/// RFC 3339 has four-digit years only, and `DuckDB` holds years from
/// 290309 BC to 294247 AD. The encoder writes a year outside 0000–9999
/// the way chrono does, in ISO 8601's expanded form: a sign and at least
/// four digits (`+10000-01-01T00:00:00.000000Z`, `-0001-12-31T…`). That
/// form is read as RFC 3339 over a four-digit year with the same leap
/// rule, so 29 February is valid exactly where the real year has one.
fn is_timestamp_text(text: &str) -> bool {
    if text == "infinity" || text == "-infinity" {
        return true;
    }
    let Some(unsigned) = text.strip_prefix(['+', '-']) else {
        return chrono::DateTime::parse_from_rfc3339(text).is_ok();
    };
    let Some((digits, rest)) = unsigned.split_once('-') else {
        return false;
    };
    if digits.len() < 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Ok(year) = digits.parse::<i64>() else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let proxy = if leap { "2000" } else { "2001" };
    chrono::DateTime::parse_from_rfc3339(&format!("{proxy}-{rest}")).is_ok()
}

fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

// -- schema introspection types -----------------------------------------------

/// A column descriptor from schema introspection (name + `DuckDB` type).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SchemaColumn {
    /// Column name as declared in the parquet file(s).
    pub name: String,
    /// `DuckDB` logical type (e.g. "VARCHAR", "TIMESTAMP", "BIGINT").
    pub data_type: String,
}

/// The result of a schema introspection query.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SchemaResult {
    /// Columns discovered in the data source.
    pub columns: Vec<SchemaColumn>,
    /// Number of parquet files matching the configured glob.
    pub file_count: u64,
}

/// Per-column statistics aggregated from parquet file footers.
///
/// Produced by the footer-stats reader in `trawl-engine`'s `parquet_stats`
/// module, summed across all row groups in the matched parquet files.
#[derive(Debug, Clone)]
pub struct ParquetColumnStats {
    /// Column name (`path_in_schema`).
    pub column_name: String,
    /// Total values across all row groups.
    pub total_count: u64,
    /// Total null values across all row groups.
    pub null_count: u64,
    /// Minimum value (stringified).
    pub min_value: Option<String>,
    /// Maximum value (stringified).
    pub max_value: Option<String>,
    /// Total compressed size in bytes.
    pub compressed_bytes: u64,
}

/// The result wire, golden in both directions: every type, the integer
/// boundaries, the float specials, lists and tagged mixed cells.
#[cfg(test)]
mod wire_tests {
    use super::*;
    use serde_json::json;

    fn result(columns: Vec<Column>, rows: Vec<Vec<Value>>) -> QueryResult {
        QueryResult { columns, rows }
    }

    fn decode(wire: serde_json::Value) -> Result<QueryResult, String> {
        serde_json::from_value(wire).map_err(|e| e.to_string())
    }

    /// Encode to text and back, so the integer boundaries cross a real
    /// JSON parse, not just a `serde_json::Value`.
    fn round_trip(result: &QueryResult) -> QueryResult {
        let text = serde_json::to_string(result).expect("encodes");
        serde_json::from_str(&text).expect("decodes")
    }

    #[test]
    fn every_type_has_a_golden_spelling() {
        let typed = result(
            vec![
                Column::new("b", ColumnType::Boolean),
                Column::new("i", ColumnType::Integer),
                Column::new("f", ColumnType::Float),
                Column::new("s", ColumnType::String),
                Column::new("t", ColumnType::Timestamp),
                Column::new("d", ColumnType::Date),
                Column::new("tm", ColumnType::Time),
                Column::new("l", ColumnType::list(ColumnType::Integer)),
                Column::new("ll", ColumnType::list(ColumnType::list(ColumnType::Float))),
                Column::new("m", ColumnType::Mixed),
            ],
            vec![
                vec![
                    Value::Boolean(true),
                    Value::Integer(-42),
                    Value::Float(1.5),
                    Value::String("nginx".into()),
                    Value::String("2026-01-15T09:00:00.000000Z".into()),
                    Value::String("2026-01-15".into()),
                    Value::String("09:00:00.5".into()),
                    Value::Array(vec![Value::Integer(1), Value::Null, Value::UInt(u64::MAX)]),
                    Value::Array(vec![Value::Array(vec![Value::Float(0.5)])]),
                    Value::Integer(7),
                ],
                vec![
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                ],
            ],
        );
        let wire = json!({
            "columns": [
                {"name": "b", "type": "boolean"},
                {"name": "i", "type": "integer"},
                {"name": "f", "type": "float"},
                {"name": "s", "type": "string"},
                {"name": "t", "type": "timestamp"},
                {"name": "d", "type": "date"},
                {"name": "tm", "type": "time"},
                {"name": "l", "type": "list", "items": {"type": "integer"}},
                {"name": "ll", "type": "list",
                 "items": {"type": "list", "items": {"type": "float"}}},
                {"name": "m", "type": "mixed"},
            ],
            "rows": [
                [true, "-42", 1.5, "nginx", "2026-01-15T09:00:00.000000Z", "2026-01-15",
                 "09:00:00.5", ["1", null, "18446744073709551615"], [[0.5]],
                 {"type": "integer", "value": "7"}],
                [null, null, null, null, null, null, null, null, null, null],
            ],
        });
        assert_eq!(serde_json::to_value(&typed).unwrap(), wire);
        assert_eq!(decode(wire).unwrap(), typed);
    }

    /// The integer boundaries: 2^53+1 is the first an `f64` reader would
    /// round; `i64::MAX + 1` is the first `UInt`. Every one survives a
    /// text round trip exactly and lands in the narrowest variant.
    #[test]
    fn integers_round_trip_exactly_at_every_boundary() {
        let cells = [
            (Value::Integer(9_007_199_254_740_993), "9007199254740993"),
            (Value::Integer(-9_007_199_254_740_993), "-9007199254740993"),
            (Value::Integer(i64::MIN), "-9223372036854775808"),
            (Value::Integer(i64::MAX), "9223372036854775807"),
            (
                Value::UInt(9_223_372_036_854_775_808),
                "9223372036854775808",
            ),
            (Value::UInt(u64::MAX), "18446744073709551615"),
            (Value::Integer(0), "0"),
        ];
        for (cell, spelled) in cells {
            let one = result(
                vec![Column::new("n", ColumnType::Integer)],
                vec![vec![cell.clone()]],
            );
            let wire = serde_json::to_value(&one).unwrap();
            assert_eq!(wire["rows"][0][0], json!(spelled));
            assert_eq!(round_trip(&one), one, "{spelled}");
        }
    }

    /// Outside `i64` ∪ `u64` the decoder refuses, naming the column, and
    /// never collapses the value to a float.
    #[test]
    fn out_of_range_integers_are_refused_with_the_column_named() {
        for text in ["18446744073709551616", "-9223372036854775809"] {
            let err = decode(json!({
                "columns": [{"name": "total", "type": "integer"}],
                "rows": [[text]],
            }))
            .unwrap_err();
            assert!(
                err.starts_with(&format!(
                    "column \"total\": {text} is outside the integer range"
                )),
                "{err}"
            );
        }
    }

    /// Only the canonical spelling decodes, and a JSON number is not an
    /// integer cell at all.
    #[test]
    fn non_canonical_integers_are_refused() {
        for text in ["+1", "01", "-0", " 1", "1 ", "", "-", "1e3", "1.0", "0x1"] {
            let err = decode(json!({
                "columns": [{"name": "n", "type": "integer"}],
                "rows": [[text]],
            }))
            .unwrap_err();
            assert!(
                err.starts_with(&format!(
                    "column \"n\": {text:?} is not a canonical decimal integer"
                )),
                "{text:?}: {err}"
            );
        }
        let err = decode(json!({
            "columns": [{"name": "n", "type": "integer"}],
            "rows": [[18_446_744_073_709_551_615_u64]],
        }))
        .unwrap_err();
        assert!(
            err.starts_with("column \"n\": an integer cell is a decimal string"),
            "{err}"
        );
    }

    /// A `timestamp` cell is RFC 3339, or `infinity`/`-infinity`; a year
    /// RFC 3339 cannot spell takes ISO 8601's expanded form, with the
    /// real year's leap rule. Anything else is refused by column.
    #[test]
    fn timestamp_cells_are_rfc3339() {
        for text in [
            "2026-01-15T09:00:00.000000Z",
            "2026-01-15T14:30:00.123456+05:30",
            "1969-12-31T23:59:59.999999-08:00",
            "0000-01-01T00:00:00.000000Z",
            "infinity",
            "-infinity",
            "+10000-01-01T00:00:00.000000Z",
            "-0001-12-31T23:59:59.000000Z",
            "-290308-12-22T00:00:00.000000Z",
            "+294247-01-10T04:00:54.775806Z",
            "+2000000-02-29T00:00:00.000000Z",
        ] {
            let one = result(
                vec![Column::new("t", ColumnType::Timestamp)],
                vec![vec![Value::String(text.into())]],
            );
            assert_eq!(round_trip(&one), one, "{text}");
        }
        for text in [
            "2026-01-15",
            "09:00:00",
            "2026-01-15T09:00:00",
            "2026-02-30T00:00:00Z",
            "Infinity",
            "now",
            "+2001-02-29T00:00:00Z",
            "+999-01-01T00:00:00Z",
            "+-2000-01-01T00:00:00Z",
            "-",
            "",
        ] {
            let err = decode(json!({
                "columns": [{"name": "t", "type": "timestamp"}],
                "rows": [[text]],
            }))
            .unwrap_err();
            assert_eq!(
                err,
                format!("column \"t\": {text:?} is not an RFC 3339 timestamp"),
                "{text:?}"
            );
        }
    }

    /// A decimal string under any type but `integer` is refused, and so is
    /// any cell whose JSON kind its type does not take.
    #[test]
    fn wrong_type_cells_are_refused_with_the_column_named() {
        let cases = [
            ("float", json!("12")),
            ("boolean", json!("1")),
            ("boolean", json!(1)),
            ("string", json!(12)),
            ("timestamp", json!(1_700_000_000)),
            ("integer", json!(true)),
            ("integer", json!(["1"])),
            ("mixed", json!("12")),
            ("float", json!("nan")),
        ];
        for (ty, cell) in cases {
            let err = decode(json!({
                "columns": [{"name": "x", "type": ty}],
                "rows": [[cell]],
            }))
            .unwrap_err();
            assert!(err.starts_with("column \"x\": "), "{ty} {cell}: {err}");
        }
        let err = decode(json!({
            "columns": [{"name": "l", "type": "list", "items": {"type": "integer"}}],
            "rows": [[["1", "01"]]],
        }))
        .unwrap_err();
        assert!(
            err.starts_with("column \"l\": item 1: \"01\" is not a canonical"),
            "{err}"
        );
    }

    #[test]
    fn a_missing_or_unknown_type_does_not_decode() {
        let err = decode(json!({"columns": [{"name": "x"}], "rows": []})).unwrap_err();
        assert!(err.contains("type"), "{err}");
        let err =
            decode(json!({"columns": [{"name": "x", "type": "BIGINT"}], "rows": []})).unwrap_err();
        assert!(err.contains("BIGINT"), "{err}");
        let err =
            decode(json!({"columns": [{"name": "x", "type": "list"}], "rows": []})).unwrap_err();
        assert!(err.contains("items"), "{err}");
    }

    /// Floats are JSON numbers, and the three values JSON has no number
    /// for are the strings a JavaScript reader parses back.
    #[test]
    fn float_specials_are_strings() {
        let floats = result(
            vec![Column::new("f", ColumnType::Float)],
            vec![
                vec![Value::Float(f64::NAN)],
                vec![Value::Float(f64::INFINITY)],
                vec![Value::Float(f64::NEG_INFINITY)],
                vec![Value::Float(-0.0)],
                vec![Value::Float(0.1)],
            ],
        );
        let wire = serde_json::to_value(&floats).unwrap();
        assert_eq!(
            wire["rows"],
            json!([["NaN"], ["Infinity"], ["-Infinity"], [-0.0], [0.1]])
        );
        let back = round_trip(&floats);
        let bits: Vec<u64> = back
            .rows
            .iter()
            .map(|row| match row[0] {
                Value::Float(f) => f.to_bits(),
                ref other => panic!("not a float: {other:?}"),
            })
            .collect();
        assert!(f64::from_bits(bits[0]).is_nan());
        assert_eq!(
            &bits[1..],
            &[
                f64::INFINITY.to_bits(),
                f64::NEG_INFINITY.to_bits(),
                (-0.0_f64).to_bits(),
                0.1_f64.to_bits(),
            ]
        );
    }

    /// Every non-null cell of a `mixed` column carries its own tag, a list
    /// cell's tag names the items its elements fold to, and a null stays
    /// bare. Each cell decodes from the column type and itself.
    #[test]
    fn mixed_cells_are_tagged_and_round_trip() {
        let mixed = result(
            vec![Column::new("m", ColumnType::Mixed)],
            vec![
                vec![Value::Integer(200)],
                vec![Value::UInt(u64::MAX)],
                vec![Value::String("200".into())],
                vec![Value::Float(f64::NAN)],
                vec![Value::Boolean(false)],
                vec![Value::Array(vec![
                    Value::Integer(1),
                    Value::String("a".into()),
                ])],
                vec![Value::Array(vec![])],
                vec![Value::Null],
            ],
        );
        let wire = serde_json::to_value(&mixed).unwrap();
        assert_eq!(
            wire["rows"],
            json!([
                [{"type": "integer", "value": "200"}],
                [{"type": "integer", "value": "18446744073709551615"}],
                [{"type": "string", "value": "200"}],
                [{"type": "float", "value": "NaN"}],
                [{"type": "boolean", "value": false}],
                [{"type": "list", "items": {"type": "mixed"},
                  "value": [{"type": "integer", "value": "1"},
                            {"type": "string", "value": "a"}]}],
                [{"type": "list", "items": {"type": "string"}, "value": []}],
                [null],
            ])
        );
        let back = round_trip(&mixed);
        assert_eq!(back.columns, mixed.columns);
        for (got, want) in back.rows.iter().zip(&mixed.rows) {
            match (&got[0], &want[0]) {
                (Value::Float(a), Value::Float(b)) => assert!(a.is_nan() && b.is_nan()),
                (a, b) => assert_eq!(a, b),
            }
        }
    }

    #[test]
    fn mixed_cell_tags_are_checked() {
        let cases = [
            (json!({"value": "1"}), "type"),
            (json!({"type": "integer"}), "no value"),
            (json!({"type": "integer", "value": null}), "bare null"),
            (json!({"type": "mixed", "value": "1"}), "never mixed"),
            (json!({"type": "integer", "value": 1}), "decimal string"),
            (json!({"type": "BIGINT", "value": "1"}), "BIGINT"),
        ];
        for (cell, needle) in cases {
            let err = decode(json!({
                "columns": [{"name": "m", "type": "mixed"}],
                "rows": [[cell]],
            }))
            .unwrap_err();
            assert!(
                err.starts_with("column \"m\": ") && err.contains(needle),
                "{needle}: {err}"
            );
        }
    }

    /// The encoder refuses a cell its column type disagrees with, naming
    /// the column; a producer bug is never spelled as some other type.
    #[test]
    fn the_encoder_refuses_a_cell_its_type_disagrees_with() {
        let cases = [
            (ColumnType::Integer, Value::Float(1.0)),
            (ColumnType::Integer, Value::String("1".into())),
            (ColumnType::Float, Value::Integer(1)),
            (ColumnType::String, Value::Integer(1)),
            (ColumnType::Timestamp, Value::Boolean(true)),
            (ColumnType::list(ColumnType::Integer), Value::Integer(1)),
            (
                ColumnType::list(ColumnType::Integer),
                Value::Array(vec![Value::Float(1.0)]),
            ),
        ];
        for (ty, cell) in cases {
            let bad = result(vec![Column::new("x", ty.clone())], vec![vec![cell]]);
            let err = serde_json::to_string(&bad).unwrap_err().to_string();
            assert!(err.starts_with("column \"x\": a "), "{ty}: {err}");
        }
    }

    #[test]
    fn ragged_rows_are_refused_both_ways() {
        let ragged = result(
            vec![Column::new("a", ColumnType::Integer)],
            vec![vec![Value::Integer(1), Value::Integer(2)]],
        );
        let err = serde_json::to_string(&ragged).unwrap_err().to_string();
        assert!(err.contains("row 0 has 2 cells for 1 columns"), "{err}");
        let err = decode(json!({
            "columns": [{"name": "a", "type": "integer"}],
            "rows": [[]],
        }))
        .unwrap_err();
        assert!(err.contains("row 0 has 0 cells for 1 columns"), "{err}");
    }

    /// The fold behind a mixed cell's tag and the client-side merges: one
    /// type stays itself, two make `mixed`, nulls add nothing, and list
    /// items fold the same way.
    #[test]
    fn type_fold_rules() {
        let fold = |cells: &[Value]| {
            let mut fold = TypeFold::new();
            for cell in cells {
                fold.observe_value(cell);
            }
            fold.finish()
        };
        assert_eq!(fold(&[Value::Null]), None);
        assert_eq!(
            fold(&[Value::Integer(1), Value::Null, Value::UInt(u64::MAX)]),
            Some(ColumnType::Integer)
        );
        assert_eq!(
            fold(&[Value::Integer(1), Value::Float(1.0)]),
            Some(ColumnType::Mixed)
        );
        assert_eq!(
            fold(&[Value::Array(vec![]), Value::Array(vec![Value::Integer(1)]),]),
            Some(ColumnType::list(ColumnType::Integer))
        );
        assert_eq!(
            fold(&[
                Value::Array(vec![Value::Integer(1)]),
                Value::Array(vec![Value::Float(1.0)]),
            ]),
            Some(ColumnType::list(ColumnType::Mixed))
        );
        assert_eq!(
            fold(&[Value::Array(vec![]), Value::Integer(1)]),
            Some(ColumnType::Mixed)
        );
        let mut timestamps = TypeFold::new();
        timestamps.observe(&ColumnType::Timestamp);
        timestamps.observe(&ColumnType::String);
        assert_eq!(timestamps.finish(), Some(ColumnType::Mixed));
    }

    /// A carried type fills the holes the cells left and nothing else:
    /// no evidence takes it whole, an empty list takes its items at any
    /// depth, and an evidenced leaf or item keeps its own type.
    #[test]
    fn a_carried_type_fills_only_the_holes_in_the_evidence() {
        let fold = |cells: &[Value], carried: &ColumnType| {
            let mut fold = TypeFold::new();
            for cell in cells {
                fold.observe_value(cell);
            }
            fold.finish_with(Some(carried))
        };
        let ints = ColumnType::list(ColumnType::Integer);
        assert_eq!(fold(&[], &ints), Some(ints.clone()));
        assert_eq!(
            fold(&[Value::Null, Value::Array(vec![])], &ints),
            Some(ints.clone())
        );
        assert_eq!(
            fold(
                &[Value::Array(vec![Value::Array(vec![])])],
                &ColumnType::list(ints.clone())
            ),
            Some(ColumnType::list(ints.clone()))
        );
        assert_eq!(
            fold(&[Value::Array(vec![Value::Float(1.0)])], &ints),
            Some(ColumnType::list(ColumnType::Float))
        );
        assert_eq!(
            fold(&[Value::Integer(1)], &ColumnType::String),
            Some(ColumnType::Integer)
        );
        assert_eq!(
            fold(&[Value::Array(vec![])], &ColumnType::String),
            Some(ColumnType::list(ColumnType::String))
        );
        assert_eq!(TypeFold::new().finish_with(None), None);
    }

    /// Merging typed results: the column's type is a cell's evidence, but
    /// a null cell (typed `string` for want of any) and an empty list's
    /// items add none, so a sparse field never turns `mixed`.
    #[test]
    fn typed_cells_fold_by_their_column_type() {
        let fold = |cells: &[(ColumnType, Value)]| {
            let mut fold = TypeFold::new();
            for (ty, cell) in cells {
                fold.observe_typed(ty, cell);
            }
            fold.finish()
        };
        let text = |s: &str| Value::String(s.to_owned());
        assert_eq!(
            fold(&[
                (ColumnType::String, Value::Null),
                (ColumnType::Timestamp, text("2026-01-15T09:00:00.000000Z")),
                (ColumnType::Integer, Value::Null),
            ]),
            Some(ColumnType::Timestamp)
        );
        assert_eq!(fold(&[(ColumnType::String, Value::Null)]), None);
        assert_eq!(
            fold(&[
                (ColumnType::Timestamp, text("2026-01-15T09:00:00.000000Z")),
                (ColumnType::String, text("soon")),
            ]),
            Some(ColumnType::Mixed)
        );
        assert_eq!(
            fold(&[
                (ColumnType::list(ColumnType::String), Value::Array(vec![])),
                (
                    ColumnType::list(ColumnType::Integer),
                    Value::Array(vec![Value::UInt(u64::MAX)])
                ),
            ]),
            Some(ColumnType::list(ColumnType::Integer))
        );
        assert_eq!(
            fold(&[(ColumnType::Mixed, Value::Integer(1))]),
            Some(ColumnType::Mixed)
        );
    }

    #[test]
    fn huge_integers_have_no_variant() {
        assert_eq!(land_u64(u64::from(u32::MAX)), Value::Integer(4_294_967_295));
        assert_eq!(
            land_u64(9_223_372_036_854_775_807),
            Value::Integer(i64::MAX)
        );
        assert_eq!(
            land_u64(9_223_372_036_854_775_808),
            Value::UInt(9_223_372_036_854_775_808)
        );
        assert_eq!(land_u64(u64::MAX), Value::UInt(u64::MAX));
        assert_eq!(
            land_i128(i128::from(i64::MIN)),
            Some(Value::Integer(i64::MIN))
        );
        assert_eq!(
            land_i128(i128::from(i64::MAX) + 1),
            Some(Value::UInt(9_223_372_036_854_775_808))
        );
        assert_eq!(land_i128(i128::from(u64::MAX)), Some(Value::UInt(u64::MAX)));
        assert_eq!(land_i128(i128::from(u64::MAX) + 1), None);
        assert_eq!(land_i128(i128::from(i64::MIN) - 1), None);
    }

    /// Export files keep plain JSON numbers, exact for both integer
    /// variants, with a non-finite float as `null`.
    #[test]
    fn plain_json_keeps_numbers_for_exports() {
        let row = [
            Value::Integer(9_007_199_254_740_993),
            Value::UInt(u64::MAX),
            Value::Float(f64::NAN),
            Value::Float(2.5),
            Value::Array(vec![Value::UInt(u64::MAX), Value::Null]),
        ];
        let plain: Vec<String> = row
            .iter()
            .map(|v| serde_json::to_string(&v.to_plain_json()).unwrap())
            .collect();
        assert_eq!(
            plain,
            [
                "9007199254740993",
                "18446744073709551615",
                "null",
                "2.5",
                "[18446744073709551615,null]",
            ]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str) -> Column {
        Column::new(name, ColumnType::String)
    }

    #[test]
    fn field_display_rank_orders_envelope_custom_trailing() {
        // Envelope fields rank first, in declared order.
        assert_eq!(field_display_rank("_time"), (0, 0));
        assert_eq!(field_display_rank("_severity"), (0, 4));
        assert_eq!(field_display_rank("message"), (0, 5));
        // Custom fields sit between envelope and trailing metadata; callers
        // break ties by name.
        assert_eq!(field_display_rank("duration").0, 1);
        // Trailing metadata is demoted to the very end, in declared order.
        assert_eq!(field_display_rank("_raw"), (2, 0));
        assert_eq!(field_display_rank("_repairs"), (2, 2));

        // Sorting by (rank, name) reproduces query-result column order:
        // envelope first, custom alphabetical, metadata last — never
        // `_ingested` alphabetically first.
        let mut names = vec!["duration", "_ingested", "service", "_time", "alpha"];
        sort_by_display_rank(&mut names, |n| n);
        assert_eq!(
            names,
            vec!["_time", "service", "alpha", "duration", "_ingested"]
        );
    }

    #[test]
    fn reorder_columns_moves_well_known_first() {
        let mut result = QueryResult {
            columns: vec![
                col("pid"),
                col("host"),
                col("message"),
                col("_time"),
                col("status"),
            ],
            rows: vec![vec![
                Value::Integer(1),
                Value::String("web-1".into()),
                Value::String("ok".into()),
                Value::String("2026-01-01".into()),
                Value::Integer(200),
            ]],
        };

        result.reorder_columns(WELL_KNOWN_LOG_FIELDS);

        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["_time", "host", "message", "pid", "status"]);
        // Row data must follow the same permutation.
        assert_eq!(result.rows[0][0], Value::String("2026-01-01".into()));
        assert_eq!(result.rows[0][1], Value::String("web-1".into()));
        assert_eq!(result.rows[0][2], Value::String("ok".into()));
        assert_eq!(result.rows[0][3], Value::Integer(1));
        assert_eq!(result.rows[0][4], Value::Integer(200));
    }

    /// The full log ordering: leading envelope first, `_raw`/`_ingested`/
    /// `_repairs` demoted to the very end, user fields in between.
    #[test]
    fn reorder_log_columns_demotes_trailing() {
        let mut result = QueryResult {
            columns: vec![
                col("_raw"),
                col("pid"),
                col("_ingested"),
                col("host"),
                col("_repairs"),
                col("message"),
                col("_time"),
            ],
            rows: vec![vec![
                Value::String("raw".into()),
                Value::Integer(1),
                Value::String("ing".into()),
                Value::String("web-1".into()),
                Value::Null,
                Value::String("ok".into()),
                Value::String("t".into()),
            ]],
        };

        result.reorder_log_columns();

        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "_time",
                "host",
                "message",
                "pid",
                "_raw",
                "_ingested",
                "_repairs"
            ]
        );
        // Rows permute with the columns.
        assert_eq!(result.rows[0][0], Value::String("t".into()));
        assert_eq!(result.rows[0][4], Value::String("raw".into()));
        assert_eq!(result.rows[0][6], Value::Null);
    }

    #[test]
    fn reorder_columns_skips_missing_fields() {
        let mut result = QueryResult {
            columns: vec![col("status"), col("uri")],
            rows: vec![vec![Value::Integer(200), Value::String("/api".into())]],
        };

        result.reorder_columns(WELL_KNOWN_LOG_FIELDS);

        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["status", "uri"]); // unchanged
    }

    #[test]
    fn reorder_columns_noop_when_already_ordered() {
        let mut result = QueryResult {
            columns: vec![col("_time"), col("host"), col("extra")],
            rows: vec![vec![
                Value::String("t".into()),
                Value::String("h".into()),
                Value::Integer(1),
            ]],
        };

        result.reorder_columns(WELL_KNOWN_LOG_FIELDS);

        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["_time", "host", "extra"]);
    }

    #[test]
    fn reorder_columns_empty_result() {
        let mut result = QueryResult::empty();
        result.reorder_columns(WELL_KNOWN_LOG_FIELDS); // should not panic
        assert!(result.columns.is_empty());
    }
}
