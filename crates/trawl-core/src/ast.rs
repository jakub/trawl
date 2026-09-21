// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! AST type definitions for the trawl query language.
//!
//! These types are the contract between the parser and every consumer of a
//! parsed query: the SQL emitter, the in-memory filter and stream compiler,
//! and the DSL formatter.

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

/// A complete trawl DSL query: an optional search stage followed by zero or
/// more pipe stages.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub search: SearchStage,
    pub pipeline: Vec<Spanned<PipeStage>>,
}

impl Query {
    /// If the first pipeline stage is `from saved`, return a reference to it.
    #[must_use]
    pub fn from_saved_stage(&self) -> Option<&FromSavedStage> {
        self.pipeline.first().and_then(|s| match &s.node {
            PipeStage::FromSaved(fs) => Some(fs),
            _ => None,
        })
    }

    /// Whether the search stage is empty (no groups, no time filters, no bounds).
    #[must_use]
    pub fn has_empty_search(&self) -> bool {
        self.search.groups.iter().all(Vec::is_empty) && self.time_clause().is_none()
    }

    /// The time clause this query carries, if any. Delegates to
    /// [`SearchStage::time_clause`], the ONE predicate write policy reads
    /// (ADR-0018 ruling 7); read it there for why a text scan cannot
    /// stand in for it.
    #[must_use]
    pub fn time_clause(&self) -> Option<TimeClause> {
        self.search.time_clause()
    }

    /// Whether any pipeline stage emits aggregated rows (stats, timechart,
    /// top, rare, pivot).
    ///
    /// Consumers use this to distinguish "raw event stream" queries from
    /// "shaped result snapshot" queries — e.g. the live-tail UI renders
    /// aggregating queries as charts and raw-event queries as tables.
    #[must_use]
    pub fn has_aggregation(&self) -> bool {
        self.pipeline.iter().any(|s| {
            matches!(
                s.node,
                PipeStage::Stats(_)
                    | PipeStage::Timechart(_)
                    | PipeStage::Top(_)
                    | PipeStage::Rare(_)
                    | PipeStage::Pivot(_)
            )
        })
    }
}

// ---------------------------------------------------------------------------
// Search stage (everything before the first `|`)
// ---------------------------------------------------------------------------

/// The implicit search stage — OR-separated groups of AND-joined tokens.
///
/// `a b OR c d` → groups: `[[a, b], [c, d]]`
/// A query without OR has a single group.
///
/// Time filters are hoisted out of groups and applied globally — a query
/// like `service=nginx last=2h OR service=postgres` applies the time
/// filter to both groups.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchStage {
    pub groups: Vec<Vec<Spanned<SearchToken>>>,
    /// Global time filter, hoisted from groups during parsing.
    /// If multiple `last=` tokens appear, last one wins.
    pub time_filter: Option<Spanned<TimeFilter>>,
    /// Absolute lower time bound (`earliest="..."`), hoisted from groups.
    pub earliest: Option<Spanned<String>>,
    /// Absolute upper time bound (`latest="..."`), hoisted from groups.
    pub latest: Option<Spanned<String>>,
}

impl SearchStage {
    /// All tokens across all groups, flattened.
    ///
    /// Convenience for consumers that don't care about OR grouping
    /// (e.g. extracting time filters, counting tokens in tests).
    pub fn all_tokens(&self) -> impl Iterator<Item = &Spanned<SearchToken>> {
        self.groups.iter().flat_map(|g| g.iter())
    }

    /// The time clause this search stage carries, if any: the first
    /// present of `last=`, `earliest=`, `latest=`.
    ///
    /// This is the ONE answer to "does this query own its own window",
    /// and write policy reads it here (ADR-0018 ruling 7: a schedule
    /// window and a query time clause may not coexist, in either
    /// direction). Reading it off the parsed AST is what makes the
    /// refusal exact. A text scan over the query source
    /// ([`crate::parser::scan`]) cannot answer the question, in both
    /// directions. The grammar keywords are also reachable as ordinary
    /// field names through backticks, so `` `last`=5 `` is a filter on a
    /// column called `last` and carries no window, while a scanner
    /// looking for the word would refuse it. And the parser hoists a
    /// clause out of `NOT` and out of an OR group, so
    /// `service=x OR last=1h` carries a window that a per-group reading
    /// of the text would miss.
    ///
    /// Which of the three is reported matters only for the message a
    /// refusal prints. A query carrying two clauses is already the
    /// emitter's error (`last=` beside an absolute bound).
    #[must_use]
    pub fn time_clause(&self) -> Option<TimeClause> {
        if self.time_filter.is_some() {
            Some(TimeClause::Last)
        } else if self.earliest.is_some() {
            Some(TimeClause::Earliest)
        } else if self.latest.is_some() {
            Some(TimeClause::Latest)
        } else {
            None
        }
    }
}

/// Which time clause a search stage carries.
///
/// The variants are the grammar's closed keyword set
/// ([`crate::parser::suggest::GRAMMAR_KEYWORDS`]), and
/// `grammar_keywords_and_time_clause_variants_agree` holds the two in
/// step: a fourth keyword must teach this enum before it parses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeClause {
    /// `last=2h`, a duration measured back from now.
    Last,
    /// `earliest="…"`, an absolute lower bound, inclusive.
    Earliest,
    /// `latest="…"`, an absolute upper bound, exclusive.
    Latest,
}

impl TimeClause {
    /// The DSL keyword that spells this clause.
    #[must_use]
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Last => "last",
            Self::Earliest => "earliest",
            Self::Latest => "latest",
        }
    }
}

/// A single token in the search stage.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchToken {
    /// `field=value`, `field>=100`, `field=200,301,404`
    FieldFilter(FieldFilter),
    /// bare word search, optionally negated with `-`
    TextSearch(TextSearch),
    /// `last=2h`, `last=7d`
    TimeFilter(TimeFilter),
    /// `"exact phrase"`
    QuotedSearch(QuotedSearch),
    /// `earliest="2026-03-14T03:00:00Z"` — absolute lower time bound
    EarliestFilter(String),
    /// `latest="2026-03-14T03:15:00Z"` — absolute upper time bound
    LatestFilter(String),
    /// `NOT token` or `NOT (group)` — negation of a search token
    Not(Box<Spanned<SearchToken>>),
    /// `(token1 token2 OR token3)` — parenthesized OR-of-AND group
    Group(Vec<Vec<Spanned<SearchToken>>>),
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

/// Time-range filter (`last=2h`).
#[derive(Debug, Clone, PartialEq)]
pub struct TimeFilter {
    pub duration: TrawlDuration,
}

/// A duration that preserves the original unit as written by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrawlDuration {
    pub quantity: u64,
    pub unit: TimeUnit,
}

impl TrawlDuration {
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

impl fmt::Display for TrawlDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.quantity, self.unit)
    }
}

/// Time units supported by `last=` filters.
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
    Tail(TailStage),
    Rename(RenameStage),
    Sample(SampleStage),
    EventStats(EventStatsStage),
    FromSaved(FromSavedStage),
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
            Self::Tail(_) => write!(f, "tail"),
            Self::Rename(_) => write!(f, "rename"),
            Self::Sample(_) => write!(f, "sample"),
            Self::EventStats(_) => write!(f, "eventstats"),
            Self::FromSaved(_) => write!(f, "from"),
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
///
/// Also parsed from `head 20` (SPL alias). The `keyword` field preserves
/// which form the user wrote so the formatter can round-trip it.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitStage {
    pub count: u64,
    /// Original keyword: `"limit"` or `"head"`.
    pub keyword: &'static str,
}

/// `table status, avg_duration` — select output columns.
///
/// Also parsed from `fields` (SPL alias). The `keyword` field preserves
/// which form the user wrote so the formatter can round-trip it.
#[derive(Debug, Clone, PartialEq)]
pub struct TableStage {
    pub fields: Vec<String>,
    /// Original keyword: `"table"` or `"fields"`.
    pub keyword: &'static str,
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

/// `let duration_ms = duration * 1000` — computed/derived field(s).
///
/// Supports comma-separated multi-assignment:
/// `let a = lower(service), b = length(service)`
///
/// Also parsed from `eval` (SPL alias). The `keyword` field preserves
/// which form the user wrote so the formatter can round-trip it.
#[derive(Debug, Clone, PartialEq)]
pub struct LetStage {
    pub assignments: Vec<(String, Spanned<Expr>)>,
    /// Original keyword: `"let"` or `"eval"`.
    pub keyword: &'static str,
}

/// The mode of extraction for `extract`.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtractMode {
    /// Named-group regex extraction.
    Regex(String),
    /// Key-value pair extraction (`extract kv [sep="X"]`).
    KeyValue { separator: char },
}

/// `extract "(?P<ip>\\d+)" from message` — regex-based field extraction.
///
/// Also parsed from `rex` (SPL alias). The `keyword` field preserves
/// which form the user wrote so the formatter can round-trip it.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractStage {
    pub mode: ExtractMode,
    /// Source field for extraction. `None` defaults to `message`.
    pub source_field: Option<String>,
    /// Original keyword: `"extract"` or `"rex"`.
    pub keyword: &'static str,
}

/// `dedup host, service` — deduplicate rows by field(s), keeping most recent.
#[derive(Debug, Clone, PartialEq)]
pub struct DedupStage {
    pub fields: Vec<String>,
}

/// `timechart span=5m count() by service` — time-bucketed aggregation.
#[derive(Debug, Clone, PartialEq)]
pub struct TimechartStage {
    /// The column the buckets are cut from. `None` is `_time`, which the
    /// emitter buckets through the envelope's `TRY_CAST` (ADR-0008).
    /// An explicit name is bucketed as stored, with no cast, so a column
    /// that is not already a timestamp is refused by the engine rather
    /// than silently coerced.
    pub on: Option<String>,
    pub span: Option<TrawlDuration>,
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

/// `tail 5` — last N rows (inverse of `limit`/`head`).
#[derive(Debug, Clone, PartialEq)]
pub struct TailStage {
    pub count: u64,
}

/// `rename old AS new` — rename columns in the output.
#[derive(Debug, Clone, PartialEq)]
pub struct RenameStage {
    pub renames: Vec<(String, String)>,
}

/// `eventstats avg(duration) by service` — non-reducing aggregation.
///
/// Like `stats` but appends aggregation results back to every row
/// as window functions instead of collapsing rows.
#[derive(Debug, Clone, PartialEq)]
pub struct EventStatsStage {
    pub aggregations: Vec<AggExpr>,
    pub group_by: Vec<String>,
}

/// `from saved daily_ip_rollup [run=latest|all|N]` — load saved query results.
///
/// Must be the first pipe stage in a pipeline and cannot be combined
/// with a search stage. The server resolves the name to parquet files
/// before query execution.
#[derive(Debug, Clone, PartialEq)]
pub struct FromSavedStage {
    pub name: String,
    pub run: SavedRunSelector,
}

/// Which run(s) to load for a `from saved` query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SavedRunSelector {
    /// Most recent successful run (default).
    #[default]
    Latest,
    /// All historical runs — injects `_run_id` and `_run_time` columns.
    All,
    /// A specific run by numeric ID.
    Specific(i64),
}

impl fmt::Display for SavedRunSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Latest => write!(f, "latest"),
            Self::All => write!(f, "all"),
            Self::Specific(id) => write!(f, "{id}"),
        }
    }
}

/// `sample 10%` or `sample 1000` — statistical sampling.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleStage {
    pub mode: SampleMode,
}

/// Sampling mode: percentage or row count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleMode {
    /// `sample 10%` — bernoulli sampling at given percentage.
    Percent(u64),
    /// `sample 1000` — reservoir sampling of N rows.
    Count(u64),
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
    Like,
    ILike,
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
            Self::Like => write!(f, "like"),
            Self::ILike => write!(f, "ilike"),
        }
    }
}

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

/// A float literal: the parsed value together with the source text it was
/// written as.
///
/// The text is not decoration. A pipeline comparison against a VARCHAR pin
/// binds the literal's text into `DECIMAL(38,6)` instead of round-tripping
/// through `f64` (ADR-0011 ruling #6), which is lossy the moment a literal
/// needs more than 53 bits: `9007199254740993.0` parses as `9007199254740992`,
/// so rendering the parsed double back would compare against the adjacent
/// identifier, and the same query written `"9007199254740993.0"` — a string
/// literal, carried verbatim — would answer differently, contradicting the
/// quote-insensitivity of pipeline comparisons. Keeping the token means the
/// pipeline binds exactly what the search stage binds for the same text.
#[derive(Debug, Clone, PartialEq)]
pub struct FloatLiteral {
    value: f64,
    text: String,
}

impl FloatLiteral {
    /// A float literal read from source: the parsed value and its token.
    pub fn new(value: f64, text: impl Into<String>) -> Self {
        Self {
            value,
            text: text.into(),
        }
    }

    /// The parsed double — what arithmetic and native binding use.
    #[must_use]
    pub fn value(&self) -> f64 {
        self.value
    }

    /// The source token — what pin-aware comparison binds.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl From<f64> for FloatLiteral {
    /// A literal with no token behind it (a programmatically built AST).
    /// Rust renders the shortest round-tripping decimal, so the text reads
    /// back as exactly this double — the best a value with no source can do.
    fn from(value: f64) -> Self {
        Self {
            text: value.to_string(),
            value,
        }
    }
}

impl fmt::Display for FloatLiteral {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// Literal values in expressions.
#[derive(Debug, Clone, PartialEq)]
pub enum LiteralValue {
    String(String),
    Int(i64),
    Float(FloatLiteral),
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;
    use crate::parser::suggest::GRAMMAR_KEYWORDS;

    fn clause(dsl: &str) -> Option<TimeClause> {
        parser::parse(dsl)
            .expect("parse should succeed")
            .time_clause()
    }

    /// Drift guard: the enum and the grammar's keyword set are the same
    /// three words in the same order. A fourth keyword added to the
    /// parser fails here until [`TimeClause`] learns it, which is what
    /// keeps the write-policy predicate total (ADR-0018 ruling 7).
    #[test]
    fn grammar_keywords_and_time_clause_variants_agree() {
        let variants = [TimeClause::Last, TimeClause::Earliest, TimeClause::Latest];
        let spelled: Vec<&'static str> = variants.iter().map(|c| c.keyword()).collect();
        assert_eq!(spelled.as_slice(), GRAMMAR_KEYWORDS);
    }

    #[test]
    fn each_keyword_reports_its_own_variant() {
        assert_eq!(clause("last=1h"), Some(TimeClause::Last));
        assert_eq!(
            clause(r#"earliest="2026-01-01T00:00:00Z""#),
            Some(TimeClause::Earliest)
        );
        assert_eq!(
            clause(r#"latest="2026-01-01T00:00:00Z""#),
            Some(TimeClause::Latest)
        );
    }

    /// The parser hoists a time clause out of `NOT` and out of an OR
    /// group, so both queries carry a window even though the keyword
    /// never sits at the top level of the token list.
    #[test]
    fn a_hoisted_clause_still_counts() {
        assert_eq!(clause("NOT last=1h"), Some(TimeClause::Last));
        assert_eq!(clause("service=x OR last=1h"), Some(TimeClause::Last));
    }

    /// A backticked `last` is a field name, not the keyword: the whole
    /// reason this predicate reads the AST instead of the query text.
    #[test]
    fn a_backticked_keyword_is_a_field_filter_not_a_window() {
        assert_eq!(clause("`last`=5"), None);
    }

    #[test]
    fn a_query_without_a_time_clause_carries_none() {
        assert_eq!(clause("service=nginx"), None);
        assert_eq!(clause("| stats count()"), None);
    }
}
