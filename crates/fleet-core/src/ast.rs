//! AST type definitions for the fleet query language.
//!
//! These types represent the parsed structure of a fleet DSL query.
//! The AST is the contract between the parser and the SQL emitter.

use std::fmt;
use std::ops::Range;

// ---------------------------------------------------------------------------
// Span wrapper
// ---------------------------------------------------------------------------

/// A node annotated with its source location.
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Range<usize>,
}

impl<T> Spanned<T> {
    pub fn new(node: T, span: Range<usize>) -> Self {
        Self { node, span }
    }
}

// ---------------------------------------------------------------------------
// Top-level query
// ---------------------------------------------------------------------------

/// A complete fleet DSL query: an optional search stage followed by zero or
/// more pipe stages.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub search: SearchStage,
    pub pipeline: Vec<Spanned<PipeStage>>,
}

// ---------------------------------------------------------------------------
// Search stage (everything before the first `|`)
// ---------------------------------------------------------------------------

/// The implicit search stage — a list of search tokens combined with AND.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchStage {
    pub tokens: Vec<Spanned<SearchToken>>,
}

/// A single token in the search stage.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchToken {
    /// `field:value`, `field:>100`, `field:200,301,404`
    FieldFilter(FieldFilter),
    /// bare word search, optionally negated with `-`
    TextSearch(TextSearch),
    /// `last:2h`, `last:7d`
    TimeFilter(TimeFilter),
    /// `"exact phrase"`
    QuotedSearch(QuotedSearch),
}

/// A field-value filter with an operator.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldFilter {
    pub field: String,
    pub op: FilterOp,
    pub value: FilterValue,
}

/// Comparison operators for field filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    Glob,
    Regex,
}

impl fmt::Display for FilterOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eq => write!(f, "="),
            Self::Ne => write!(f, "!="),
            Self::Gt => write!(f, ">"),
            Self::Gte => write!(f, ">="),
            Self::Lt => write!(f, "<"),
            Self::Lte => write!(f, "<="),
            Self::Glob => write!(f, "glob"),
            Self::Regex => write!(f, "regex"),
        }
    }
}

/// The right-hand side of a field filter.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterValue {
    /// Single literal value.
    Literal(String),
    /// Comma-separated list — maps to SQL `IN`.
    List(Vec<String>),
}

/// Bare-word text search, optionally negated.
#[derive(Debug, Clone, PartialEq)]
pub struct TextSearch {
    pub term: String,
    pub negated: bool,
}

/// Time-range filter (`last:2h`).
#[derive(Debug, Clone, PartialEq)]
pub struct TimeFilter {
    pub duration: FleetDuration,
}

/// A duration that preserves the original unit as written by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FleetDuration {
    pub quantity: u64,
    pub unit: TimeUnit,
}

impl FleetDuration {
    /// Seconds-per-unit multiplier for this duration's time unit.
    #[must_use]
    pub const fn unit_multiplier(&self) -> u64 {
        match self.unit {
            TimeUnit::Seconds => 1,
            TimeUnit::Minutes => 60,
            TimeUnit::Hours => 3600,
            TimeUnit::Days => 86_400,
            TimeUnit::Weeks => 604_800,
        }
    }

    /// Convert to total seconds for comparison and heuristic purposes.
    ///
    /// Safe from overflow: the parser rejects durations whose
    /// `quantity * unit_multiplier()` would exceed `u64::MAX`.
    #[must_use]
    pub fn to_seconds(&self) -> u64 {
        self.quantity * self.unit_multiplier()
    }

    /// Format as a `DuckDB` interval string like `"2 hours"`.
    #[must_use]
    pub fn to_interval_string(&self) -> String {
        let unit = match self.unit {
            TimeUnit::Seconds => "seconds",
            TimeUnit::Minutes => "minutes",
            TimeUnit::Hours => "hours",
            TimeUnit::Days => "days",
            TimeUnit::Weeks => "weeks",
        };
        format!("{} {unit}", self.quantity)
    }
}

impl fmt::Display for FleetDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.quantity, self.unit)
    }
}

/// Time units supported by `last:` filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeUnit {
    Seconds,
    Minutes,
    Hours,
    Days,
    Weeks,
}

impl fmt::Display for TimeUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Seconds => write!(f, "s"),
            Self::Minutes => write!(f, "m"),
            Self::Hours => write!(f, "h"),
            Self::Days => write!(f, "d"),
            Self::Weeks => write!(f, "w"),
        }
    }
}

/// A quoted phrase search (`"connection refused"`).
#[derive(Debug, Clone, PartialEq)]
pub struct QuotedSearch {
    pub phrase: String,
}

// ---------------------------------------------------------------------------
// Pipe stages
// ---------------------------------------------------------------------------

/// A stage in the query pipeline (after `|`).
#[derive(Debug, Clone, PartialEq)]
pub enum PipeStage {
    Stats(StatsStage),
    Where(WhereStage),
    Sort(SortStage),
    Limit(LimitStage),
    Table(TableStage),
    Top(TopStage),
    Rare(RareStage),
    Drop(DropStage),
    Let(LetStage),
    Extract(ExtractStage),
    Dedup(DedupStage),
    Timechart(TimechartStage),
    Pivot(PivotStage),
}

impl fmt::Display for PipeStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stats(_) => write!(f, "stats"),
            Self::Where(_) => write!(f, "where"),
            Self::Sort(_) => write!(f, "sort"),
            Self::Limit(_) => write!(f, "limit"),
            Self::Table(_) => write!(f, "table"),
            Self::Top(_) => write!(f, "top"),
            Self::Rare(_) => write!(f, "rare"),
            Self::Drop(_) => write!(f, "drop"),
            Self::Let(_) => write!(f, "let"),
            Self::Extract(_) => write!(f, "extract"),
            Self::Dedup(_) => write!(f, "dedup"),
            Self::Timechart(_) => write!(f, "timechart"),
            Self::Pivot(_) => write!(f, "pivot"),
        }
    }
}

/// `stats count() by host` — aggregate with optional grouping.
#[derive(Debug, Clone, PartialEq)]
pub struct StatsStage {
    pub aggregations: Vec<AggExpr>,
    pub group_by: Vec<String>,
}

/// An aggregation expression like `count()`, `avg(duration)`, or
/// `count() as total`.
#[derive(Debug, Clone, PartialEq)]
pub struct AggExpr {
    pub function: String,
    pub args: Vec<Spanned<Expr>>,
    pub alias: Option<String>,
}

/// `where count > 10` — filter on computed values.
#[derive(Debug, Clone, PartialEq)]
pub struct WhereStage {
    pub condition: Spanned<Expr>,
}

/// `sort -count` — sort by fields with optional direction.
#[derive(Debug, Clone, PartialEq)]
pub struct SortStage {
    pub fields: Vec<SortField>,
}

/// A single field in a sort clause.
#[derive(Debug, Clone, PartialEq)]
pub struct SortField {
    pub field: String,
    pub direction: SortDirection,
}

/// Sort direction — ascending by default, `-` prefix for descending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Asc,
    Desc,
}

impl fmt::Display for SortDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Asc => write!(f, "asc"),
            Self::Desc => write!(f, "desc"),
        }
    }
}

/// `limit 20` — cap the number of results.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitStage {
    pub count: u64,
}

/// `table status, avg_duration` — select output columns.
#[derive(Debug, Clone, PartialEq)]
pub struct TableStage {
    pub fields: Vec<String>,
}

/// `top 10 host` — frequency analysis (most common values).
#[derive(Debug, Clone, PartialEq)]
pub struct TopStage {
    pub count: u64,
    pub field: String,
    pub by: Vec<String>,
}

/// `rare 5 status` — inverse frequency analysis (least common values).
#[derive(Debug, Clone, PartialEq)]
pub struct RareStage {
    pub count: u64,
    pub field: String,
    pub by: Vec<String>,
}

/// `drop message, raw` — exclude specific columns from output.
#[derive(Debug, Clone, PartialEq)]
pub struct DropStage {
    pub fields: Vec<String>,
}

/// `let duration_ms = duration * 1000` — computed/derived field.
#[derive(Debug, Clone, PartialEq)]
pub struct LetStage {
    pub field: String,
    pub expr: Spanned<Expr>,
}

/// The mode of extraction for `extract`.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtractMode {
    /// Named-group regex extraction.
    Regex(String),
    /// Key-value pair extraction (`extract kv`).
    KeyValue,
}

/// `extract "(?P<ip>\\d+)" from message` — regex-based field extraction.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractStage {
    pub mode: ExtractMode,
    /// Source field for extraction. `None` defaults to `message`.
    pub source_field: Option<String>,
}

/// `dedup host, service` — deduplicate rows by field(s), keeping most recent.
#[derive(Debug, Clone, PartialEq)]
pub struct DedupStage {
    pub fields: Vec<String>,
}

/// `timechart span=5m count() by service` — time-bucketed aggregation.
#[derive(Debug, Clone, PartialEq)]
pub struct TimechartStage {
    pub span: Option<FleetDuration>,
    pub aggregations: Vec<AggExpr>,
    pub group_by: Vec<String>,
}

/// `pivot count() on status by host` — pivot table transformation.
#[derive(Debug, Clone, PartialEq)]
pub struct PivotStage {
    pub aggregation: AggExpr,
    pub on_field: String,
    pub by: Vec<String>,
}

// ---------------------------------------------------------------------------
// Expressions (used in where clauses, aggregation args, etc.)
// ---------------------------------------------------------------------------

/// An expression node in the AST.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Binary operation: `a + b`, `count > 10`, `a and b`
    Binary {
        lhs: Box<Spanned<Expr>>,
        op: BinaryOp,
        rhs: Box<Spanned<Expr>>,
    },
    /// Unary operation: `not x`, `-x`
    Unary {
        op: UnaryOp,
        operand: Box<Spanned<Expr>>,
    },
    /// Function call: `count()`, `avg(duration)`
    FunctionCall {
        name: String,
        args: Vec<Spanned<Expr>>,
    },
    /// Field reference: `host`, `host.name`, `@timestamp`
    FieldRef(String),
    /// Literal value
    Literal(LiteralValue),
    /// `x in (1, 2, 3)`
    InList {
        expr: Box<Spanned<Expr>>,
        list: Vec<Spanned<Expr>>,
    },
}

/// Binary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    // arithmetic
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    // comparison
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    // logical
    And,
    Or,
    // pattern
    Matches,
}

impl fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Add => write!(f, "+"),
            Self::Sub => write!(f, "-"),
            Self::Mul => write!(f, "*"),
            Self::Div => write!(f, "/"),
            Self::Mod => write!(f, "%"),
            Self::Eq => write!(f, "=="),
            Self::Ne => write!(f, "!="),
            Self::Gt => write!(f, ">"),
            Self::Gte => write!(f, ">="),
            Self::Lt => write!(f, "<"),
            Self::Lte => write!(f, "<="),
            Self::And => write!(f, "and"),
            Self::Or => write!(f, "or"),
            Self::Matches => write!(f, "matches"),
        }
    }
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Neg,
}

impl fmt::Display for UnaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Not => write!(f, "not"),
            Self::Neg => write!(f, "-"),
        }
    }
}

/// Literal values in expressions.
#[derive(Debug, Clone, PartialEq)]
pub enum LiteralValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}

impl fmt::Display for LiteralValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(s) => write!(f, "\"{s}\""),
            Self::Int(n) => write!(f, "{n}"),
            Self::Float(n) => write!(f, "{n}"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Null => write!(f, "null"),
        }
    }
}
