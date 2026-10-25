// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Typed construction of the correlation reads' DSL (ADR-0058).
//!
//! `context`, `sightings` and `compare` run caller-chosen values (a cell of
//! a captured row, a literal, a filter and field names) as DSL. Nothing
//! here splices that text: each builder assembles an [`ast::Query`] whose
//! values sit in quoted positions, renders it through
//! [`format::format_query`], and proves the rendering with
//! [`Composed::verify`], which parses the text back and requires the same
//! AST. A value holding `"`, `\`, `|`, `#`, `//` or a wildcard therefore
//! stays one quoted value and cannot add a pipe stage, open a comment or
//! turn an exact match into a glob.
//!
//! No composed text carries a time clause. The capture's bounds resolver
//! splices the bounds and, for an event-shaped read, the imposed order and
//! the cut; a builder that also wrote `earliest=` would be refused there.
//!
//! [`ast::Query`]: crate::ast::Query

use std::collections::BTreeSet;
use std::fmt;

use chrono::{DateTime, SecondsFormat, Utc};

use crate::ast::{
    AggExpr, BinaryOp, EventStatsStage, Expr, FieldFilter, FilterOp, FilterValue, LetStage,
    LiteralValue, PipeStage, Query, QuotedSearch, SearchStage, SearchToken, SortDirection,
    SortField, SortStage, Spanned, StatsStage, TimeClause, TimeUnit, TimechartStage, TrawlDuration,
    WhereStage,
};
use crate::format::format_query;
use crate::parser::parse;
use crate::parser::suggest::quote_dsl_field;
use crate::schema::catalog_key;

/// The longest literal `sightings` accepts, in bytes.
pub const MAX_LITERAL_BYTES: usize = 1024;

/// The most group fields `compare` accepts.
pub const MAX_COMPARE_GROUPS: usize = 2;

/// The column `sightings` writes the per-group match count into.
pub const SIGHTING_COUNT: &str = "sighting_count";
/// The column `sightings` writes the per-group first `_time` into.
pub const SIGHTING_FIRST: &str = "sighting_first";
/// The column `sightings` writes the per-group last `_time` into.
pub const SIGHTING_LAST: &str = "sighting_last";
/// The fields `sightings` groups its counts by, in `by` order.
pub const SIGHTING_GROUPS: [&str; 2] = ["service", "env"];

/// The label column `compare` assigns each event's window membership to.
const COMPARE_SIDE: &str = "compare_side";
const COMPARE_FG: &str = "compare_fg";
const COMPARE_BL: &str = "compare_bl";
/// The per-group foreground event count column of a `compare` result.
pub const FOREGROUND_COUNT: &str = "foreground_count";
/// The per-group baseline event count column of a `compare` result.
pub const BASELINE_COUNT: &str = "baseline_count";
/// The per-group foreground events-per-minute column.
pub const FOREGROUND_RATE: &str = "foreground_rate";
/// The per-group baseline events-per-minute column.
pub const BASELINE_RATE: &str = "baseline_rate";
/// `foreground_rate - baseline_rate`.
pub const RATE_CHANGE: &str = "rate_change";
/// `abs(rate_change)`, the ranking key.
pub const ABS_RATE_CHANGE: &str = "abs_rate_change";
/// `rate_change / baseline_rate`, null where the baseline count is zero.
pub const RELATIVE_CHANGE: &str = "relative_change";
/// `new_in_foreground`, `vanished` or `both`.
pub const STATUS: &str = "status";

/// Every column the `compare` pipeline assigns. A group field of one of
/// these names would be overwritten before it is grouped, so it is
/// refused.
pub const COMPARE_COLUMNS: [&str; 11] = [
    COMPARE_SIDE,
    COMPARE_FG,
    COMPARE_BL,
    FOREGROUND_COUNT,
    BASELINE_COUNT,
    FOREGROUND_RATE,
    BASELINE_RATE,
    RATE_CHANGE,
    ABS_RATE_CHANGE,
    RELATIVE_CHANGE,
    STATUS,
];

/// Microseconds in the minute the `compare` rates are per.
const MINUTE_US: i64 = 60_000_000;

/// Why a correlation query could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// `context` was given neither an env nor a host to constrain.
    EmptyScope,
    /// The `sightings` literal is empty.
    EmptyLiteral,
    /// A value carries a NUL character, which no stored query text holds.
    Nul {
        /// What the value is: `literal`, `env`, `host` or `filter`.
        what: &'static str,
    },
    /// The `sightings` literal is longer than [`MAX_LITERAL_BYTES`].
    LiteralTooLong {
        /// The literal's length in bytes.
        bytes: usize,
    },
    /// The `compare` filter does not parse.
    FilterParse {
        /// The parser's first message.
        message: String,
    },
    /// The `compare` filter is not search tokens only.
    FilterShape {
        /// What the filter carries that it may not.
        reason: String,
    },
    /// A group field name is refused.
    Field {
        /// The refused name.
        name: String,
        /// Why it is refused.
        reason: &'static str,
    },
    /// More than [`MAX_COMPARE_GROUPS`] group fields.
    TooManyGroups {
        /// How many were given.
        count: usize,
    },
    /// `min_count` is above the largest DSL integer.
    MinCount,
    /// A window has a non-positive duration or an instant the DSL cannot
    /// write.
    Window {
        /// `foreground` or `baseline`.
        which: &'static str,
        /// Why it is refused.
        reason: &'static str,
    },
    /// The foreground and baseline windows overlap.
    WindowsOverlap,
    /// The rendered text parses back to a different query, so the read is
    /// refused instead of run. From the values a builder quotes this is a
    /// builder bug. From a `compare` filter it can also be a filter the
    /// formatter does not render faithfully: a glob value holding `"`
    /// (`host<"*`, which the parser reads as a glob) renders quoted, and a
    /// quoted value reads back as an exact match.
    Reparse,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyScope => write!(f, "the context scope constrains neither env nor host"),
            Self::EmptyLiteral => write!(f, "the literal is empty"),
            Self::Nul { what } => write!(f, "the {what} contains a NUL character"),
            Self::LiteralTooLong { bytes } => write!(
                f,
                "the literal is {bytes} bytes; at most {MAX_LITERAL_BYTES} are accepted"
            ),
            Self::FilterParse { message } => write!(f, "the filter does not parse: {message}"),
            Self::FilterShape { reason } => write!(f, "the filter {reason}"),
            Self::Field { name, reason } => write!(f, "group field {name:?} {reason}"),
            Self::TooManyGroups { count } => write!(
                f,
                "{count} group fields given; at most {MAX_COMPARE_GROUPS} are accepted"
            ),
            Self::MinCount => write!(f, "min_count is above {}", i64::MAX),
            Self::Window { which, reason } => write!(f, "the {which} window {reason}"),
            Self::WindowsOverlap => write!(f, "the foreground and baseline windows overlap"),
            Self::Reparse => write!(
                f,
                "the composed query does not read back as built; the read was not run"
            ),
        }
    }
}

impl std::error::Error for BuildError {}

/// A composed query: the AST a builder assembled and its canonical text.
#[derive(Debug, Clone, PartialEq)]
pub struct Composed {
    query: Query,
    text: String,
}

impl Composed {
    /// Render `query` and prove the rendering.
    fn new(query: Query) -> Result<Self, BuildError> {
        let text = format_query(&query);
        let composed = Self { query, text };
        composed.verify()?;
        Ok(composed)
    }

    /// The query as built.
    #[must_use]
    pub fn query(&self) -> &Query {
        &self.query
    }

    /// The canonical DSL text of [`Self::query`].
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The reparse proof: [`Self::text`] parses to the AST that was built,
    /// spans aside. Every builder runs it before returning; a caller that
    /// carries a `Composed` across a boundary can run it again.
    ///
    /// # Errors
    ///
    /// [`BuildError::Reparse`] when the text does not parse or parses to a
    /// different query.
    pub fn verify(&self) -> Result<(), BuildError> {
        let reparsed = parse(&self.text).map_err(|_| BuildError::Reparse)?;
        if unspanned_query(&reparsed) == unspanned_query(&self.query) {
            Ok(())
        } else {
            Err(BuildError::Reparse)
        }
    }
}

/// A half-open window of UTC microseconds, `[start_us, end_us)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start_us: i64,
    pub end_us: i64,
}

impl Window {
    /// The window's length in microseconds; positive for a valid window.
    #[must_use]
    pub fn duration_us(self) -> i64 {
        self.end_us.saturating_sub(self.start_us)
    }
}

/// The `context` search: `env="…" host="…"`, each given key as a quoted
/// exact match.
///
/// # Errors
///
/// [`BuildError::EmptyScope`] when neither is given,
/// [`BuildError::Nul`] when a value carries NUL.
pub fn context_query(env: Option<&str>, host: Option<&str>) -> Result<Composed, BuildError> {
    let mut group = Vec::new();
    for (what, value) in [("env", env), ("host", host)] {
        if let Some(value) = value {
            refuse_nul(what, value)?;
            group.push(token(eq_filter(what, value)));
        }
    }
    if group.is_empty() {
        return Err(BuildError::EmptyScope);
    }
    Composed::new(Query {
        search: search(vec![group]),
        pipeline: Vec::new(),
    })
}

/// The `sightings` statement: the literal as one quoted text search, then
/// per-(service, env) count, first and last `_time` over every match.
///
/// ```text
/// "<literal>"
/// | eventstats count() as sighting_count, min(_time) as sighting_first, max(_time) as sighting_last by service, env
/// ```
///
/// No sort and no head: the capture imposes the order and the cut, after
/// the window functions have seen every match.
///
/// # Errors
///
/// [`BuildError::EmptyLiteral`], [`BuildError::Nul`] or
/// [`BuildError::LiteralTooLong`].
pub fn sightings_query(literal: &str) -> Result<Composed, BuildError> {
    check_literal(literal)?;
    let eventstats = EventStatsStage {
        aggregations: vec![
            agg("count", None, SIGHTING_COUNT),
            agg("min", Some("_time"), SIGHTING_FIRST),
            agg("max", Some("_time"), SIGHTING_LAST),
        ],
        group_by: SIGHTING_GROUPS.iter().map(|&f| f.to_owned()).collect(),
    };
    Composed::new(Query {
        search: literal_search(literal, None),
        pipeline: vec![stage(PipeStage::EventStats(eventstats))],
    })
}

/// The `compare` statement: one scan over the filter, each event labelled
/// with the window it falls in, counted per group for both windows at
/// once, then rates per minute, change and status, ranked.
///
/// `filter` is DSL search tokens with no pipe stage and no time clause at
/// any depth, under `NOT` or in a group;
/// `groups` are 0 to [`MAX_COMPARE_GROUPS`] distinct field names. The
/// windows are half-open and may touch but not overlap. A filter whose
/// canonical text reads back differently is refused with
/// [`BuildError::Reparse`] rather than run as something else. Rates are per
/// minute: a window of whole minutes divides by its minutes, any other by
/// `count * 60000000 / <microseconds>`, so the divisor is always exact.
///
/// # Errors
///
/// [`BuildError::FilterParse`], [`BuildError::FilterShape`],
/// [`BuildError::Field`], [`BuildError::TooManyGroups`],
/// [`BuildError::MinCount`], [`BuildError::Window`] or
/// [`BuildError::WindowsOverlap`].
pub fn compare_query(
    filter: &str,
    foreground: Window,
    baseline: Window,
    groups: &[String],
    min_count: u64,
) -> Result<Composed, BuildError> {
    refuse_nul("filter", filter)?;
    let search = filter_search(filter)?;
    check_groups(groups)?;
    let min = i64::try_from(min_count).map_err(|_| BuildError::MinCount)?;
    let (fg_start, fg_end) = window_literals("foreground", foreground)?;
    let (bl_start, bl_end) = window_literals("baseline", baseline)?;
    if foreground.start_us.max(baseline.start_us) < foreground.end_us.min(baseline.end_us) {
        return Err(BuildError::WindowsOverlap);
    }

    let in_window = |start: String, end: String| {
        binary(
            binary(field("_time"), BinaryOp::Gte, string(start)),
            BinaryOp::And,
            binary(field("_time"), BinaryOp::Lt, string(end)),
        )
    };
    let side = call(
        "case",
        vec![
            in_window(fg_start, fg_end),
            string("foreground"),
            in_window(bl_start, bl_end),
            string("baseline"),
        ],
    );
    let flag = |label: &str| {
        call(
            "if",
            vec![
                binary(field(COMPARE_SIDE), BinaryOp::Eq, string(label)),
                int(1),
                int(0),
            ],
        )
    };
    let at_least = |column: &str| binary(field(column), BinaryOp::Gte, int(min));

    let mut sort_keys = vec![
        sort_key(ABS_RATE_CHANGE, SortDirection::Desc),
        sort_key(FOREGROUND_COUNT, SortDirection::Desc),
        sort_key(BASELINE_COUNT, SortDirection::Desc),
    ];
    sort_keys.extend(groups.iter().map(|g| sort_key(g, SortDirection::Asc)));

    let pipeline = vec![
        let_stage(vec![(COMPARE_SIDE, side)]),
        where_stage(call("isnotnull", vec![field(COMPARE_SIDE)])),
        let_stage(vec![
            (COMPARE_FG, flag("foreground")),
            (COMPARE_BL, flag("baseline")),
        ]),
        stage(PipeStage::Stats(StatsStage {
            aggregations: vec![
                agg("sum", Some(COMPARE_FG), FOREGROUND_COUNT),
                agg("sum", Some(COMPARE_BL), BASELINE_COUNT),
            ],
            group_by: groups.to_vec(),
        })),
        where_stage(binary(
            at_least(FOREGROUND_COUNT),
            BinaryOp::Or,
            at_least(BASELINE_COUNT),
        )),
        rates_stage(foreground, baseline),
        stage(PipeStage::Sort(SortStage { fields: sort_keys })),
    ];
    Composed::new(Query { search, pipeline })
}

/// The `compare` stage after the counts: rates per minute, their change,
/// the relative change where the baseline is not zero, and the status.
fn rates_stage(foreground: Window, baseline: Window) -> Spanned<PipeStage> {
    let_stage(vec![
        (FOREGROUND_RATE, per_minute(FOREGROUND_COUNT, foreground)),
        (BASELINE_RATE, per_minute(BASELINE_COUNT, baseline)),
        (
            RATE_CHANGE,
            binary(field(FOREGROUND_RATE), BinaryOp::Sub, field(BASELINE_RATE)),
        ),
        (ABS_RATE_CHANGE, call("abs", vec![field(RATE_CHANGE)])),
        (
            RELATIVE_CHANGE,
            call(
                "case",
                vec![
                    binary(field(BASELINE_COUNT), BinaryOp::Eq, int(0)),
                    Expr::Literal(LiteralValue::Null),
                    binary(
                        binary(field(FOREGROUND_RATE), BinaryOp::Sub, field(BASELINE_RATE)),
                        BinaryOp::Div,
                        field(BASELINE_RATE),
                    ),
                ],
            ),
        ),
        (
            STATUS,
            call(
                "case",
                vec![
                    binary(field(BASELINE_COUNT), BinaryOp::Eq, int(0)),
                    string("new_in_foreground"),
                    binary(field(FOREGROUND_COUNT), BinaryOp::Eq, int(0)),
                    string("vanished"),
                    string("both"),
                ],
            ),
        ),
    ])
}

/// At most two next queries for a `sightings` answer: the literal on the
/// busiest service, and the literal's hourly volume by service. The
/// caller runs them over the same bounds.
///
/// A literal [`sightings_query`] refuses proposes nothing, and a busiest
/// service carrying NUL drops the first proposal.
#[must_use]
pub fn follow_ups(literal: &str, busiest_service: Option<&str>) -> Vec<Composed> {
    if check_literal(literal).is_err() {
        return Vec::new();
    }
    let mut proposals = Vec::new();
    if let Some(service) = busiest_service.filter(|s| !s.contains('\0'))
        && let Ok(composed) = Composed::new(Query {
            search: literal_search(literal, Some(service)),
            pipeline: Vec::new(),
        })
    {
        proposals.push(composed);
    }
    let timechart = TimechartStage {
        on: None,
        span: Some(TrawlDuration {
            quantity: 1,
            unit: TimeUnit::Hours,
        }),
        aggregations: vec![agg_bare("count", None)],
        group_by: vec!["service".to_owned()],
    };
    if let Ok(composed) = Composed::new(Query {
        search: literal_search(literal, None),
        pipeline: vec![stage(PipeStage::Timechart(timechart))],
    }) {
        proposals.push(composed);
    }
    proposals
}

/// The attribution rule `sightings` applies in Rust to the captured rows,
/// standing for the text search's `ILIKE` containment.
pub use crate::text::contains_ci;

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn refuse_nul(what: &'static str, value: &str) -> Result<(), BuildError> {
    if value.contains('\0') {
        Err(BuildError::Nul { what })
    } else {
        Ok(())
    }
}

fn check_literal(literal: &str) -> Result<(), BuildError> {
    if literal.is_empty() {
        return Err(BuildError::EmptyLiteral);
    }
    refuse_nul("literal", literal)?;
    if literal.len() > MAX_LITERAL_BYTES {
        return Err(BuildError::LiteralTooLong {
            bytes: literal.len(),
        });
    }
    Ok(())
}

/// The caller's filter as a search stage: parsed by the DSL parser, and
/// refused unless it is search tokens alone.
fn filter_search(filter: &str) -> Result<SearchStage, BuildError> {
    let parsed = parse(filter).map_err(|errors| BuildError::FilterParse {
        message: errors
            .first()
            .map_or_else(|| "parse error".to_owned(), |e| e.message.clone()),
    })?;
    if let Some(first) = parsed.pipeline.first() {
        return Err(BuildError::FilterShape {
            reason: format!("has a `{}` pipe stage; give search tokens only", first.node),
        });
    }
    // The parser hoists a clause out of a group and from directly under
    // `NOT`, but leaves one under `NOT (…)` or `NOT NOT …` in the tree, so
    // the tree is walked as well.
    let clause = parsed
        .search
        .hoisted
        .first()
        .map(|clause| clause.node)
        .or_else(|| nested_time_clause(parsed.search.all_tokens()));
    if let Some(clause) = clause {
        return Err(BuildError::FilterShape {
            reason: format!(
                "has a `{}=` time clause; the windows are the only bounds",
                clause.keyword()
            ),
        });
    }
    Ok(unspanned_query(&parsed).search)
}

/// The first time clause anywhere in `tokens`, under every `NOT` and in
/// every parenthesised group.
fn nested_time_clause<'a>(
    tokens: impl IntoIterator<Item = &'a Spanned<SearchToken>>,
) -> Option<TimeClause> {
    tokens.into_iter().find_map(|token| match &token.node {
        SearchToken::TimeFilter(_) => Some(TimeClause::Last),
        SearchToken::EarliestFilter(_) => Some(TimeClause::Earliest),
        SearchToken::LatestFilter(_) => Some(TimeClause::Latest),
        SearchToken::Not(inner) => nested_time_clause(std::iter::once(inner.as_ref())),
        SearchToken::Group(groups) => nested_time_clause(groups.iter().flatten()),
        SearchToken::FieldFilter(_) | SearchToken::TextSearch(_) | SearchToken::QuotedSearch(_) => {
            None
        }
    })
}

/// Refuse group names the statement cannot carry. Names compare under the
/// DSL's own field fold ([`catalog_key`]): `Compare_Side` is the column
/// `compare_side` and `Host` and `host` are one column, so the reserved
/// and duplicate tests fold, while the emitted text and every error keep
/// the caller's spelling.
fn check_groups(groups: &[String]) -> Result<(), BuildError> {
    if groups.len() > MAX_COMPARE_GROUPS {
        return Err(BuildError::TooManyGroups {
            count: groups.len(),
        });
    }
    let mut seen = BTreeSet::new();
    for name in groups {
        if quote_dsl_field(name).is_none() {
            return Err(BuildError::Field {
                name: name.clone(),
                reason: "cannot be written as a DSL field name",
            });
        }
        let folded = catalog_key(name);
        if COMPARE_COLUMNS.contains(&folded.as_str()) {
            return Err(BuildError::Field {
                name: name.clone(),
                reason: "is a column the compare statement assigns",
            });
        }
        if !seen.insert(folded) {
            return Err(BuildError::Field {
                name: name.clone(),
                reason: "is given twice",
            });
        }
    }
    Ok(())
}

/// A window's bounds as DSL timestamp literals, UTC with `Z` and
/// microseconds.
fn window_literals(which: &'static str, window: Window) -> Result<(String, String), BuildError> {
    if window.end_us <= window.start_us {
        return Err(BuildError::Window {
            which,
            reason: "ends at or before its start",
        });
    }
    let text = |us: i64| {
        DateTime::<Utc>::from_timestamp_micros(us)
            .map(|at| at.to_rfc3339_opts(SecondsFormat::Micros, true))
            .ok_or(BuildError::Window {
                which,
                reason: "has an instant outside the representable range",
            })
    };
    Ok((text(window.start_us)?, text(window.end_us)?))
}

// ---------------------------------------------------------------------------
// AST construction
// ---------------------------------------------------------------------------

fn spanned<T>(node: T) -> Spanned<T> {
    Spanned::new(node, 0..0)
}

fn token(node: SearchToken) -> Spanned<SearchToken> {
    spanned(node)
}

fn stage(node: PipeStage) -> Spanned<PipeStage> {
    spanned(node)
}

fn search(groups: Vec<Vec<Spanned<SearchToken>>>) -> SearchStage {
    SearchStage {
        groups,
        time_filter: None,
        earliest: None,
        latest: None,
        hoisted: Vec::new(),
    }
}

/// `"<literal>"`, optionally followed by `service="<service>"`.
fn literal_search(literal: &str, service: Option<&str>) -> SearchStage {
    let mut group = vec![token(SearchToken::QuotedSearch(QuotedSearch {
        phrase: literal.to_owned(),
    }))];
    if let Some(service) = service {
        group.push(token(eq_filter("service", service)));
    }
    search(vec![group])
}

fn eq_filter(field: &str, value: &str) -> SearchToken {
    SearchToken::FieldFilter(FieldFilter {
        field: field.to_owned(),
        op: FilterOp::Eq,
        value: FilterValue::Literal(value.to_owned()),
    })
}

fn agg_bare(function: &str, arg: Option<&str>) -> AggExpr {
    AggExpr {
        function: function.to_owned(),
        args: arg.map(|a| vec![spanned(field(a))]).unwrap_or_default(),
        alias: None,
    }
}

fn agg(function: &str, arg: Option<&str>, alias: &str) -> AggExpr {
    AggExpr {
        alias: Some(alias.to_owned()),
        ..agg_bare(function, arg)
    }
}

fn field(name: &str) -> Expr {
    Expr::FieldRef(name.to_owned())
}

fn string(text: impl Into<String>) -> Expr {
    Expr::Literal(LiteralValue::String(text.into()))
}

fn int(n: i64) -> Expr {
    Expr::Literal(LiteralValue::Int(n))
}

fn binary(lhs: Expr, op: BinaryOp, rhs: Expr) -> Expr {
    Expr::Binary {
        lhs: Box::new(spanned(lhs)),
        op,
        rhs: Box::new(spanned(rhs)),
    }
}

fn call(name: &str, args: Vec<Expr>) -> Expr {
    Expr::FunctionCall {
        name: name.to_owned(),
        args: args.into_iter().map(spanned).collect(),
    }
}

fn let_stage(assignments: Vec<(&str, Expr)>) -> Spanned<PipeStage> {
    stage(PipeStage::Let(LetStage {
        assignments: assignments
            .into_iter()
            .map(|(name, expr)| (name.to_owned(), spanned(expr)))
            .collect(),
        keyword: "let",
    }))
}

fn where_stage(condition: Expr) -> Spanned<PipeStage> {
    stage(PipeStage::Where(WhereStage {
        condition: spanned(condition),
    }))
}

fn sort_key(name: &str, direction: SortDirection) -> SortField {
    SortField {
        field: name.to_owned(),
        direction,
    }
}

/// `<count> / <minutes>` for a window of whole minutes, else
/// `<count> * 60000000 / <microseconds>`: events per minute with an exact
/// divisor either way.
fn per_minute(count: &str, window: Window) -> Expr {
    let us = window.duration_us();
    if us % MINUTE_US == 0 {
        binary(field(count), BinaryOp::Div, int(us / MINUTE_US))
    } else {
        binary(
            binary(field(count), BinaryOp::Mul, int(MINUTE_US)),
            BinaryOp::Div,
            int(us),
        )
    }
}

// ---------------------------------------------------------------------------
// Span-blind comparison
// ---------------------------------------------------------------------------

/// `query` with every span set to `0..0`, so a built AST and a parsed one
/// compare by what they say.
fn unspanned_query(query: &Query) -> Query {
    Query {
        search: unspanned_search(&query.search),
        pipeline: query
            .pipeline
            .iter()
            .map(|s| spanned(unspanned_stage(&s.node)))
            .collect(),
    }
}

fn unspanned_search(search: &SearchStage) -> SearchStage {
    SearchStage {
        groups: unspanned_groups(&search.groups),
        time_filter: search.time_filter.as_ref().map(|t| spanned(t.node.clone())),
        earliest: search.earliest.as_ref().map(|e| spanned(e.node.clone())),
        latest: search.latest.as_ref().map(|l| spanned(l.node.clone())),
        hoisted: search.hoisted.iter().map(|h| spanned(h.node)).collect(),
    }
}

fn unspanned_groups(groups: &[Vec<Spanned<SearchToken>>]) -> Vec<Vec<Spanned<SearchToken>>> {
    groups
        .iter()
        .map(|group| {
            group
                .iter()
                .map(|t| spanned(unspanned_token(&t.node)))
                .collect()
        })
        .collect()
}

fn unspanned_token(token: &SearchToken) -> SearchToken {
    match token {
        SearchToken::Not(inner) => {
            SearchToken::Not(Box::new(spanned(unspanned_token(&inner.node))))
        }
        SearchToken::Group(groups) => SearchToken::Group(unspanned_groups(groups)),
        other => other.clone(),
    }
}

fn unspanned_stage(stage: &PipeStage) -> PipeStage {
    let aggs = |aggs: &[AggExpr]| aggs.iter().map(unspanned_agg).collect::<Vec<_>>();
    match stage {
        PipeStage::Stats(s) => PipeStage::Stats(StatsStage {
            aggregations: aggs(&s.aggregations),
            group_by: s.group_by.clone(),
        }),
        PipeStage::EventStats(s) => PipeStage::EventStats(EventStatsStage {
            aggregations: aggs(&s.aggregations),
            group_by: s.group_by.clone(),
        }),
        PipeStage::Timechart(s) => PipeStage::Timechart(TimechartStage {
            aggregations: aggs(&s.aggregations),
            ..s.clone()
        }),
        PipeStage::Pivot(s) => PipeStage::Pivot(crate::ast::PivotStage {
            aggregation: unspanned_agg(&s.aggregation),
            ..s.clone()
        }),
        PipeStage::Where(s) => PipeStage::Where(WhereStage {
            condition: spanned(unspanned_expr(&s.condition.node)),
        }),
        PipeStage::Let(s) => PipeStage::Let(LetStage {
            assignments: s
                .assignments
                .iter()
                .map(|(name, expr)| (name.clone(), spanned(unspanned_expr(&expr.node))))
                .collect(),
            keyword: s.keyword,
        }),
        other => other.clone(),
    }
}

fn unspanned_agg(agg: &AggExpr) -> AggExpr {
    AggExpr {
        function: agg.function.clone(),
        args: agg
            .args
            .iter()
            .map(|a| spanned(unspanned_expr(&a.node)))
            .collect(),
        alias: agg.alias.clone(),
    }
}

fn unspanned_expr(expr: &Expr) -> Expr {
    let boxed = |e: &Spanned<Expr>| Box::new(spanned(unspanned_expr(&e.node)));
    let list = |items: &[Spanned<Expr>]| {
        items
            .iter()
            .map(|e| spanned(unspanned_expr(&e.node)))
            .collect()
    };
    match expr {
        Expr::Binary { lhs, op, rhs } => Expr::Binary {
            lhs: boxed(lhs),
            op: *op,
            rhs: boxed(rhs),
        },
        Expr::Unary { op, operand } => Expr::Unary {
            op: *op,
            operand: boxed(operand),
        },
        Expr::FunctionCall { name, args } => Expr::FunctionCall {
            name: name.clone(),
            args: list(args),
        },
        Expr::InList { expr, list: items } => Expr::InList {
            expr: boxed(expr),
            list: list(items),
        },
        Expr::FieldRef(_) | Expr::Literal(_) => expr.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR_US: i64 = 3_600_000_000;
    /// 2026-10-24T10:00:00Z.
    const T0: i64 = 1_792_836_000_000_000;

    fn window(start_us: i64, end_us: i64) -> Window {
        Window { start_us, end_us }
    }

    fn groups(names: &[&str]) -> Vec<String> {
        names.iter().map(|&n| n.to_owned()).collect()
    }

    #[test]
    fn context_text_is_quoted_exact_matches() {
        let composed = context_query(Some("prod"), Some("web-1")).unwrap();
        assert_eq!(composed.text(), "env=prod host=web-1");
        let composed = context_query(Some("a*b"), None).unwrap();
        assert_eq!(composed.text(), r#"env="a*b""#);
        let composed = context_query(None, Some("x | stats count() #c")).unwrap();
        assert_eq!(composed.text(), r#"host="x | stats count() #c""#);
    }

    #[test]
    fn context_needs_a_scope_and_refuses_nul() {
        assert_eq!(context_query(None, None), Err(BuildError::EmptyScope));
        assert_eq!(
            context_query(Some("a\0b"), None),
            Err(BuildError::Nul { what: "env" })
        );
    }

    #[test]
    fn sightings_golden_text() {
        let composed = sightings_query("10.0.4.17").unwrap();
        assert_eq!(
            composed.text(),
            "\"10.0.4.17\"\n| eventstats count() as sighting_count, min(_time) as \
             sighting_first, max(_time) as sighting_last by service, env"
        );
        let hostile = sightings_query(r#"a" | stats count() #"#).unwrap();
        assert!(
            hostile.text().starts_with(
                r#""a\" | stats count() #"
| eventstats"#
            ),
            "{}",
            hostile.text()
        );
    }

    #[test]
    fn sightings_refuses_bad_literals_by_name() {
        assert_eq!(sightings_query(""), Err(BuildError::EmptyLiteral));
        assert_eq!(
            sightings_query("a\0"),
            Err(BuildError::Nul { what: "literal" })
        );
        let long = "x".repeat(MAX_LITERAL_BYTES + 1);
        assert_eq!(
            sightings_query(&long),
            Err(BuildError::LiteralTooLong {
                bytes: MAX_LITERAL_BYTES + 1
            })
        );
        assert!(sightings_query(&"x".repeat(MAX_LITERAL_BYTES)).is_ok());
    }

    #[test]
    fn compare_golden_text() {
        let composed = compare_query(
            r#"service=auth "failed password""#,
            window(T0, T0 + HOUR_US),
            window(T0 - HOUR_US, T0),
            &groups(&["host", "user"]),
            3,
        )
        .unwrap();
        let expected = [
            r#"service=auth "failed password""#,
            r#"| let compare_side = case(_time >= "2026-10-24T10:00:00.000000Z" and _time < "2026-10-24T11:00:00.000000Z", "foreground", _time >= "2026-10-24T09:00:00.000000Z" and _time < "2026-10-24T10:00:00.000000Z", "baseline")"#,
            "| where isnotnull(compare_side)",
            r#"| let compare_fg = if(compare_side == "foreground", 1, 0), compare_bl = if(compare_side == "baseline", 1, 0)"#,
            "| stats sum(compare_fg) as foreground_count, sum(compare_bl) as baseline_count by host, user",
            "| where foreground_count >= 3 or baseline_count >= 3",
            r#"| let foreground_rate = foreground_count / 60, baseline_rate = baseline_count / 60, rate_change = foreground_rate - baseline_rate, abs_rate_change = abs(rate_change), relative_change = case(baseline_count == 0, null, (foreground_rate - baseline_rate) / baseline_rate), status = case(baseline_count == 0, "new_in_foreground", foreground_count == 0, "vanished", "both")"#,
            "| sort -abs_rate_change, -foreground_count, -baseline_count, host, user",
        ]
        .join("\n");
        assert_eq!(composed.text(), expected);
    }

    #[test]
    fn compare_rates_divide_exactly_for_a_window_of_partial_minutes() {
        let composed = compare_query(
            "",
            window(T0, T0 + 90_000_001),
            window(T0 - HOUR_US, T0),
            &[],
            1,
        )
        .unwrap();
        assert!(
            composed
                .text()
                .contains("foreground_rate = foreground_count * 60000000 / 90000001,"),
            "{}",
            composed.text()
        );
        assert!(
            composed
                .text()
                .contains("baseline_rate = baseline_count / 60,")
        );
        assert!(
            composed
                .text()
                .contains("| sort -abs_rate_change, -foreground_count, -baseline_count")
        );
    }

    #[test]
    fn compare_windows_may_touch_but_not_overlap() {
        let fg = window(T0, T0 + HOUR_US);
        assert!(compare_query("", fg, window(T0 - HOUR_US, T0), &[], 1).is_ok());
        assert!(compare_query("", window(T0 - HOUR_US, T0), fg, &[], 1).is_ok());
        for bl in [
            window(T0 - HOUR_US, T0 + 1),
            window(T0 + HOUR_US - 1, T0 + 2 * HOUR_US),
            window(T0 + 1, T0 + 2),
            window(T0 - HOUR_US, T0 + 2 * HOUR_US),
            fg,
        ] {
            assert_eq!(
                compare_query("", fg, bl, &[], 1),
                Err(BuildError::WindowsOverlap),
                "{bl:?}"
            );
        }
    }

    #[test]
    fn compare_refuses_an_empty_or_inverted_window() {
        let bl = window(T0 - HOUR_US, T0);
        for fg in [window(T0, T0), window(T0 + 1, T0)] {
            assert_eq!(
                compare_query("", fg, bl, &[], 1),
                Err(BuildError::Window {
                    which: "foreground",
                    reason: "ends at or before its start"
                })
            );
        }
        assert!(matches!(
            compare_query("", window(T0, T0 + 1), window(5, 5), &[], 1),
            Err(BuildError::Window {
                which: "baseline",
                ..
            })
        ));
        assert!(matches!(
            compare_query("", window(i64::MAX - 1, i64::MAX), bl, &[], 1),
            Err(BuildError::Window {
                which: "foreground",
                ..
            })
        ));
    }

    #[test]
    fn compare_filter_is_search_tokens_only() {
        let (fg, bl) = (window(T0, T0 + HOUR_US), window(T0 - HOUR_US, T0));
        for filter in [
            "service=auth | stats count()",
            "| from saved x",
            "service=auth last=1h",
            r#"earliest="2026-10-24T00:00:00Z" service=auth"#,
            "NOT latest=\"2026-10-24T00:00:00Z\"",
            "service=auth OR last=2h",
        ] {
            assert!(
                matches!(
                    compare_query(filter, fg, bl, &[], 1),
                    Err(BuildError::FilterShape { .. })
                ),
                "{filter}"
            );
        }
        assert!(matches!(
            compare_query("service=(", fg, bl, &[], 1),
            Err(BuildError::FilterParse { .. })
        ));
        assert_eq!(
            compare_query("a\0", fg, bl, &[], 1),
            Err(BuildError::Nul { what: "filter" })
        );
        // The parser reads `host<"*` as a glob on the value `"*`; its
        // canonical text quotes the value, which reads back as an exact
        // match, so the read is refused rather than run as something else.
        assert_eq!(
            compare_query(r#"host<"*"#, fg, bl, &[], 1),
            Err(BuildError::Reparse)
        );
        let kept = compare_query("service=auth NOT host=a* (x OR y)", fg, bl, &[], 1).unwrap();
        assert!(
            kept.text()
                .starts_with("service=auth NOT host=a* (x OR y)\n")
        );
    }

    /// The parser hoists a time clause out of a group and from directly
    /// under `NOT`, but not out of `NOT (…)` or `NOT NOT …`: the filter
    /// check walks the whole token tree and refuses those too, naming the
    /// clause. A quoted pipe is a phrase, not a stage.
    #[test]
    fn compare_filter_refuses_a_time_clause_at_any_depth() {
        let (fg, bl) = (window(T0, T0 + HOUR_US), window(T0 - HOUR_US, T0));
        for (filter, keyword) in [
            ("* NOT (last=1h)", "last"),
            ("* NOT NOT last=1h", "last"),
            ("x NOT (a OR (b NOT NOT last=1h))", "last"),
            (
                r#"NOT (service=a earliest="2026-10-24T00:00:00Z")"#,
                "earliest",
            ),
            (r#"NOT NOT latest="2026-10-24T00:00:00Z""#, "latest"),
        ] {
            let refused = compare_query(filter, fg, bl, &[], 1);
            let Err(BuildError::FilterShape { reason }) = &refused else {
                panic!("{filter}: {refused:?}");
            };
            assert!(
                reason.contains(&format!("`{keyword}=` time clause")),
                "{filter}: {reason}"
            );
        }
        let kept = compare_query(r#""a | b""#, fg, bl, &[], 1).unwrap();
        assert!(kept.text().starts_with("\"a | b\"\n"), "{}", kept.text());
    }

    #[test]
    fn compare_refuses_bad_group_fields() {
        let (fg, bl) = (window(T0, T0 + HOUR_US), window(T0 - HOUR_US, T0));
        assert_eq!(
            compare_query("", fg, bl, &groups(&["a", "b", "c"]), 1),
            Err(BuildError::TooManyGroups { count: 3 })
        );
        for (names, reason) in [
            (&["a", "a"][..], "is given twice"),
            (&["Host", "host"][..], "is given twice"),
            (&["SERVICE", "Service"][..], "is given twice"),
            (&[""][..], "cannot be written as a DSL field name"),
            (&["a\u{202e}b"][..], "cannot be written as a DSL field name"),
            (&["status"][..], "is a column the compare statement assigns"),
            (
                &["Compare_Side"][..],
                "is a column the compare statement assigns",
            ),
            (
                &["FOREGROUND_COUNT"][..],
                "is a column the compare statement assigns",
            ),
            (
                &["Baseline_Rate"][..],
                "is a column the compare statement assigns",
            ),
            (
                &["compare_side"][..],
                "is a column the compare statement assigns",
            ),
        ] {
            assert!(
                matches!(
                    compare_query("", fg, bl, &groups(names), 1),
                    Err(BuildError::Field { reason: r, .. }) if r == reason
                ),
                "{names:?}"
            );
        }
        let mixed = compare_query("", fg, bl, &groups(&["Host", "SERVICE"]), 1).unwrap();
        assert!(
            mixed.text().contains("by Host, SERVICE"),
            "{}",
            mixed.text()
        );
        let quoted = compare_query("", fg, bl, &groups(&["k8s pod", "`odd`"]), 1).unwrap();
        assert!(
            quoted.text().contains("by `k8s pod`, ```odd```"),
            "{}",
            quoted.text()
        );
        assert_eq!(
            compare_query("", fg, bl, &[], u64::MAX),
            Err(BuildError::MinCount)
        );
    }

    #[test]
    fn follow_ups_name_the_busiest_service_and_the_hourly_volume() {
        let proposals = follow_ups("10.0.4.17", Some("auth api"));
        let texts: Vec<&str> = proposals.iter().map(Composed::text).collect();
        assert_eq!(
            texts,
            [
                r#""10.0.4.17" service="auth api""#,
                "\"10.0.4.17\"\n| timechart span=1h count() by service",
            ]
        );
        let texts: Vec<String> = follow_ups("x", None)
            .iter()
            .map(|c| c.text().to_owned())
            .collect();
        assert_eq!(texts, ["\"x\"\n| timechart span=1h count() by service"]);
        assert!(follow_ups("", Some("auth")).is_empty());
        assert_eq!(follow_ups("x", Some("a\0")).len(), 1);
    }

    #[test]
    fn contains_ci_folds_unicode_and_matches_literally() {
        assert!(contains_ci("xxÄBxx", "äb"));
        assert!(contains_ci("äb", "ÄB"));
        assert!(contains_ci("user_42 logged in", "USER_42"));
        assert!(!contains_ci("userX42 logged in", "user_42"));
        assert!(!contains_ci("100 percent", "100%"));
        assert!(contains_ci("anything", ""));
        assert!(!contains_ci("", "a"));
    }

    #[test]
    fn verify_catches_a_text_that_drifted_from_its_query() {
        let mut composed = sightings_query("abc").unwrap();
        composed.text = "\"abd\"\n| eventstats count() as sighting_count, min(_time) as \
                         sighting_first, max(_time) as sighting_last by service, env"
            .to_owned();
        assert_eq!(composed.verify(), Err(BuildError::Reparse));
        composed.text = "\"abc".to_owned();
        assert_eq!(composed.verify(), Err(BuildError::Reparse));
    }
}
