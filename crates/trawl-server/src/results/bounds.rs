// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Capture bounds, the window splice and the result shape (ADR-0057).
//!
//! A capture covers one window, resolved once at one instant `T` from one
//! source: a structured window in the request, or the query's own `last=`,
//! `earliest=` and `latest=`. [`resolve_as`] reads it, cuts the query's own
//! clauses out of the text, splices the window back in as absolute
//! microsecond bounds through [`materialize_window`], and proves the rest of
//! the query unchanged. [`classify`] decides the result's [`Shape`], unless
//! a correlation read imposes one. An
//! event-shaped capture is ordered `_time DESC, _ingested DESC` and the
//! order is written into the text, so the recorded text is exactly what
//! runs.
//!
//! Everything here is a pure function of the text, the window, a clock
//! reading and the limits: no IO, no permit and no guard, so a refusal
//! leaves nothing taken. A [`BoundsError`]'s message is written for the API
//! caller.
//!
//! # Where the parser keeps a time clause
//!
//! The parser lifts every `last=`, `earliest=` and `latest=` it can reach
//! out of the token tree into the search stage, where the last of each kind
//! wins and the earlier spans are lost. It reaches a clause at the top
//! level, under `OR`, inside parentheses, and directly under `NOT`, where a
//! `NOT` token stays behind with nothing to negate. It does not reach
//! through `NOT (...)` or `NOT NOT`, and a clause there filters nothing, so
//! `/query` runs such a query unbounded. A capture refuses that clause
//! instead of guessing ([`BoundsError::ClauseNotTopLevel`]).

use std::ops::Range;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use trawl_core::ast::{
    PipeStage, Query, SearchStage, SearchToken, SortDirection, SortField, SortStage, Spanned,
    TimeClause,
};
use trawl_core::compare::{Instant, literal_timestamp};
use trawl_core::complexity::MAX_PIPELINE_STAGES;
use trawl_core::format::format_query;
use trawl_core::sanitize::{ERROR_EXCERPT_BYTES, excerpt};
use trawl_core::schema::{INGESTED, TIME, catalog_key};

use crate::report_window::{
    ReportWindow, WindowKind, format_window_bound, materialize_window, truncate_to_micros,
};

/// The stage an event-shaped capture ends with: newest first, arrival order
/// breaking ties between equal `_time`s.
///
/// It goes on its own line, so a `#` comment at the end of the caller's
/// text cannot swallow it.
const IMPOSED_ORDER: &str = "| sort -_time, -_ingested";

/// The most pipeline stages an event-shaped capture's query may carry: one
/// fewer than any query ([`MAX_PIPELINE_STAGES`]), because the executed text
/// adds [`IMPOSED_ORDER`] and every door that checks the stage cap, the
/// engine's included, counts the text it runs. The executed text stays a
/// query any door admits, as `/query` would run it from history.
pub const MAX_EVENT_STAGES: usize = MAX_PIPELINE_STAGES - 1;

/// The longest time value a window field or a query's `earliest=` or
/// `latest=` may carry, in bytes. The longest RFC 3339 instant is about 35;
/// a longer value is refused by its length before anything quotes it.
pub const MAX_TIME_VALUE_BYTES: usize = 128;

/// What [`BoundsError::InvalidWindow`] names for a window instant.
const RFC3339_GRAMMAR: &str = "an RFC 3339 instant such as 2026-10-23T09:00:00Z";

/// What [`BoundsError::InvalidWindow`] names for a window duration.
const DURATION_GRAMMAR: &str = "a duration: a number and one of s, m, h, d, w";

/// A structured capture window, as the request carries it.
///
/// A window with all three fields absent is no window at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowInput {
    /// Inclusive lower bound, an RFC 3339 instant.
    pub earliest: Option<String>,
    /// Exclusive upper bound, an RFC 3339 instant.
    pub latest: Option<String>,
    /// A span back from the resolution instant, in the DSL's `last=`
    /// grammar (`15m`, `2h`).
    pub last: Option<String>,
}

impl WindowInput {
    fn is_empty(&self) -> bool {
        self.earliest.is_none() && self.latest.is_none() && self.last.is_none()
    }
}

/// The window limits, from the `[capture]` configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The window a capture covers when neither the request nor the query
    /// names one, ending at the resolution instant.
    pub default_window: Duration,
    /// The longest window a capture may cover. A window exactly this long
    /// is accepted.
    pub max_span: Duration,
}

/// What a capture executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The text to execute and record. For a live source it is the
    /// caller's text with its own time clauses cut out, the window spliced
    /// in front, and for an event shape [`IMPOSED_ORDER`] appended. For a
    /// stored source it is the caller's text verbatim.
    pub executed: String,
    /// Where the rows come from, and the window for a live source.
    pub source: Source,
    /// How the result behaves over the cap.
    pub shape: Shape,
}

/// Where a capture's rows come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Ingested events in the half-open window `[earliest, latest)`. Both
    /// bounds are whole microseconds, as the executed text spells them.
    Live {
        /// Inclusive lower bound.
        earliest: DateTime<Utc>,
        /// Exclusive upper bound.
        latest: DateTime<Utc>,
    },
    /// Stored report rows (`from saved`), which take no window. The
    /// response says the capture has no bounds.
    Stored,
}

/// How a result behaves over the capture cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Every row is an event: the result is ordered newest first and cut
    /// to the most recent rows that fit.
    Events,
    /// The result is one answer (an aggregate, a projection, an ordering
    /// or a sample the caller chose), so a prefix of it would answer a
    /// different question. Over the cap it is refused.
    Whole,
}

/// Why a capture's bounds could not be resolved. Every one but
/// [`Self::ShapeNotImposable`] is a 400, and the message names the fix.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BoundsError {
    /// The query text does not parse.
    #[error("the query does not parse: {message}")]
    Unparseable {
        /// The parser's first message, as [`excerpt`] quotes at most
        /// [`ERROR_EXCERPT_BYTES`] of it: the parser may echo the query
        /// whole.
        message: String,
    },
    /// A `from saved` query came with a structured window.
    #[error(
        "a 'from saved' query reads stored report rows, which take no window; remove the window"
    )]
    WindowOnStored,
    /// A `from saved` query carries a time clause of its own.
    #[error(
        "a 'from saved' query reads stored report rows, which take no time clause; remove the {}= clause",
        .clause.keyword()
    )]
    ClauseOnStored {
        /// The clause the query carries.
        clause: TimeClause,
    },
    /// The request carries a window and the query a time clause.
    #[error(
        "the request carries a window and the query carries a {}= clause; give the window in one place",
        .clause.keyword()
    )]
    WindowAndClause {
        /// The clause the query carries.
        clause: TimeClause,
    },
    /// `last` beside `earliest` or `latest`, in the query or the window.
    #[error(
        "last cannot be combined with earliest or latest; give a span back from now or absolute bounds"
    )]
    MixedClauses,
    /// `latest` without a start, in the query or the window.
    #[error(
        "latest without earliest leaves the window without a start; add earliest, or give last instead"
    )]
    LatestAlone,
    /// A query `earliest=`/`latest=` value is longer than
    /// [`MAX_TIME_VALUE_BYTES`]; the refusal names its length, never the
    /// value.
    #[error(
        "the {}= value is {bytes} bytes; at most {MAX_TIME_VALUE_BYTES} are accepted",
        .clause.keyword()
    )]
    ClauseTooLong {
        /// The clause carrying the value.
        clause: TimeClause,
        /// The value's length in bytes.
        bytes: usize,
    },
    /// A query `earliest=`/`latest=` value names no finite instant.
    #[error(
        "{}={} does not name a finite instant; use a timestamp such as 2026-10-23T09:00:00Z",
        .clause.keyword(),
        excerpt(.value, ERROR_EXCERPT_BYTES)
    )]
    InvalidClause {
        /// The clause carrying the value.
        clause: TimeClause,
        /// The value as written.
        value: String,
    },
    /// A structured window field is longer than [`MAX_TIME_VALUE_BYTES`];
    /// the refusal names its length, never the value.
    #[error("window {field} is {bytes} bytes; at most {MAX_TIME_VALUE_BYTES} are accepted")]
    WindowTooLong {
        /// The window field: `earliest`, `latest` or `last`.
        field: &'static str,
        /// The value's length in bytes.
        bytes: usize,
    },
    /// A structured window field does not read as its grammar.
    #[error("window {field} {} is not {expected}", excerpt(.value, ERROR_EXCERPT_BYTES))]
    InvalidWindow {
        /// The window field: `earliest`, `latest` or `last`.
        field: &'static str,
        /// The value as sent.
        value: String,
        /// What the field accepts.
        expected: &'static str,
    },
    /// The window's start is not before its end.
    #[error("the window is empty: earliest {earliest} is not before latest {latest}")]
    EmptyWindow {
        /// The resolved start, as the splice would spell it.
        earliest: String,
        /// The resolved end, as the splice would spell it.
        latest: String,
    },
    /// The window is longer than `[capture] max_span_secs`.
    #[error("a capture window may span at most {limit}; narrow the window")]
    SpanTooLong {
        /// The configured limit, as a duration.
        limit: String,
    },
    /// A time clause sits where the parser does not lift it out, or where
    /// cutting it out leaves text that does not parse.
    #[error(
        "the {}= clause is nested where the search cannot take it as the window, such as inside NOT (...); \
         write it at the top level of the search",
        .clause.keyword()
    )]
    ClauseNotTopLevel {
        /// The nested clause.
        clause: TimeClause,
    },
    /// An event-shaped query with no stage to spare for [`IMPOSED_ORDER`]
    /// under the stage cap.
    #[error(
        "this query has {stages} pipeline stages, over the limit of {MAX_EVENT_STAGES} for a \
         capture of events, which ends with its own `{IMPOSED_ORDER}` stage; shorten the pipeline"
    )]
    TooManyStages {
        /// The stages the query carries.
        stages: usize,
    },
    /// The spliced text is not provably the caller's query with a new
    /// window. This is a limit of the server's splice, not of the query.
    #[error(
        "the capture window could not be spliced into this query without changing the rest of it; \
         write the time clause as a top-level last=, earliest= or latest="
    )]
    SpliceMismatch,
    /// The event shape was imposed on a pipeline that is not the
    /// `eventstats` form ([`resolve_as`]). Only the server composes a query
    /// that imposes a shape, so this is the server's fault, never the
    /// caller's: the capture service answers it as an internal error.
    #[error("the event shape was imposed on a pipeline outside the eventstats form")]
    ShapeNotImposable,
}

/// Resolve a capture's window and the text it executes, at the instant
/// `now` (ADR-0057).
///
/// `T` is `now` truncated to microseconds. The window is, from the request
/// window or else the query's own clauses:
///
/// - `last=d`: `[T − d, T)`;
/// - `earliest=X`: from `X`, to `latest=Y` when given, else to `T`;
/// - `latest=Y` alone: refused, since it names no start;
/// - neither: `[T − default_window, T)`.
///
/// A query's `earliest=`/`latest=` value is read the way `DuckDB` reads the
/// literal (`compare::literal_timestamp`, which drops a spelled offset), so
/// the bound is the one `/query` would apply. A window's values are RFC
/// 3339 instants, offsets applied. A window and a query clause together are
/// refused, as are an empty window and one longer than `max_span`. An
/// event-shaped query of more than [`MAX_EVENT_STAGES`] stages is refused,
/// since its executed text would pass the stage cap.
///
/// A `from saved` query passes through unchanged as a [`Source::Stored`],
/// whole-shaped capture. It is refused with a window or a clause, since
/// stored report rows take neither.
///
/// `shape`, when given, is imposed instead of the one [`classify`]
/// decides. A correlation read (ADR-0058) composes a query whose rows are
/// events although a stage of it makes [`classify`] call it whole:
/// sightings counts its groups with `eventstats`, which adds fields and
/// keeps every event. `Some(Shape::Events)` resolves such a query as
/// event-shaped, so it is ordered and cut like any search of events,
/// [`MAX_EVENT_STAGES`] included. A `from saved` query stays a
/// whole-shaped stored capture whatever `shape` says: no order is appended
/// to stored rows.
///
/// The event shape is imposed only on that form: every stage [`classify`]
/// calls whole is an `eventstats`. A pipeline that orders, limits, samples or aggregates
/// would have its rows changed by the imposed order, or be cut as events
/// when it is one answer, so it is refused with
/// [`BoundsError::ShapeNotImposable`].
pub fn resolve_as(
    dsl: &str,
    window: Option<&WindowInput>,
    now: DateTime<Utc>,
    limits: &Limits,
    shape: Option<Shape>,
) -> Result<Resolved, BoundsError> {
    let original = parse(dsl).map_err(|message| BoundsError::Unparseable {
        message: excerpt(&message, ERROR_EXCERPT_BYTES),
    })?;
    let window = window.filter(|w| !w.is_empty());
    let clause = any_clause(&original);

    if reads_stored(&original) {
        if window.is_some() {
            return Err(BoundsError::WindowOnStored);
        }
        if let Some(clause) = clause {
            return Err(BoundsError::ClauseOnStored { clause });
        }
        return Ok(Resolved {
            executed: dsl.to_owned(),
            source: Source::Stored,
            shape: Shape::Whole,
        });
    }
    if let (Some(_), Some(clause)) = (window, clause) {
        return Err(BoundsError::WindowAndClause { clause });
    }
    if let Some(clause) = unreachable_clause(&original.search.groups) {
        return Err(BoundsError::ClauseNotTopLevel { clause });
    }

    let requested = match window {
        Some(window) => requested_by_window(window)?,
        None => requested_by_query(&original.search)?,
    };
    let (earliest, latest) = requested.bounds(truncate_to_micros(now), limits)?;

    let stripped = strip_time_clauses(dsl, &original)?;
    // `truncated` and `kind` are a report run's bookkeeping; the splice
    // reads only the two bounds.
    let window = ReportWindow {
        start: earliest,
        end: latest,
        truncated: false,
        kind: WindowKind::Fixed,
    };
    let spliced =
        materialize_window(&stripped, &window).map_err(|_| BoundsError::SpliceMismatch)?;
    prove_unchanged(dsl, &spliced)?;

    if shape == Some(Shape::Events) && !events_by_eventstats(&original) {
        return Err(BoundsError::ShapeNotImposable);
    }
    let shape = shape.unwrap_or_else(|| classify(&original));
    let executed = match shape {
        Shape::Events => {
            let stages = original.pipeline.len();
            if stages > MAX_EVENT_STAGES {
                return Err(BoundsError::TooManyStages { stages });
            }
            let ordered = format!("{spliced}\n{IMPOSED_ORDER}");
            prove_order_appended(&spliced, &ordered)?;
            ordered
        }
        Shape::Whole => spliced,
    };

    Ok(Resolved {
        executed,
        source: Source::Live { earliest, latest },
        shape,
    })
}

/// The half-open `[start, end)` a structured window covers at the instant
/// `now`, measured as [`resolve_as`] measures a capture's request window:
/// the same grammar, the same default, and the same refusals of an empty
/// window and one longer than `max_span`.
///
/// For a read that combines windows itself (`compare`, ADR-0058) and then
/// captures their hull, which [`resolve_as`] measures again.
///
/// # Errors
/// The [`BoundsError`] a capture with this window would get.
pub fn window_bounds(
    window: &WindowInput,
    now: DateTime<Utc>,
    limits: &Limits,
) -> Result<(DateTime<Utc>, DateTime<Utc>), BoundsError> {
    requested_by_window(window)?.bounds(truncate_to_micros(now), limits)
}

/// Decide whether a query's result is event-shaped (ADR-0057).
///
/// Event-shaped means every row is an event with its `_time` and
/// `_ingested` intact: every stage filters or adds fields (`where`,
/// `extract`, `let`, `drop`, `rename`), and none of `let`, `drop` or
/// `rename` writes, removes or moves `_time` or `_ingested`. Every other
/// stage makes the result one answer: it aggregates, projects, orders,
/// limits, deduplicates, samples or reads stored rows.
/// `Query::has_aggregation` is not this question, since it does not see
/// projection, ordering or sampling.
///
/// The match has no wildcard arm, so a new stage does not compile until it
/// is classified here. `extract` cannot write either column: a capture
/// group or a kv key in the `_` namespace is refused or sealed before it
/// executes (ADR-0013), so `extract` needs no name check.
pub fn classify(query: &Query) -> Shape {
    if query.pipeline.iter().all(|stage| keeps_events(&stage.node)) {
        Shape::Events
    } else {
        Shape::Whole
    }
}

/// Whether one stage keeps every row an event with its order columns
/// intact ([`classify`]).
fn keeps_events(stage: &PipeStage) -> bool {
    match stage {
        PipeStage::Where(_) | PipeStage::Extract(_) => true,
        PipeStage::Let(stage) => !stage
            .assignments
            .iter()
            .any(|(target, _)| is_order_key(target)),
        PipeStage::Drop(stage) => !stage.fields.iter().any(|field| is_order_key(field)),
        PipeStage::Rename(stage) => !stage
            .renames
            .iter()
            .any(|(from, to)| is_order_key(from) || is_order_key(to)),
        PipeStage::Stats(_)
        | PipeStage::Sort(_)
        | PipeStage::Limit(_)
        | PipeStage::Table(_)
        | PipeStage::Top(_)
        | PipeStage::Rare(_)
        | PipeStage::Dedup(_)
        | PipeStage::Timechart(_)
        | PipeStage::Pivot(_)
        | PipeStage::Tail(_)
        | PipeStage::Sample(_)
        | PipeStage::EventStats(_)
        | PipeStage::FromSaved(_) => false,
    }
}

/// Whether a query's rows are events although [`classify`] may call it
/// whole: every stage keeps events or is an `eventstats`, which adds its
/// aggregates to every row and drops none ([`resolve_as`]). So it has no
/// ordering, limiting, sampling or aggregating stage for the imposed order
/// to change. An `eventstats` cannot write an order column: the parser
/// refuses an alias in the `_` namespace (ADR-0013).
fn events_by_eventstats(query: &Query) -> bool {
    query
        .pipeline
        .iter()
        .all(|stage| matches!(stage.node, PipeStage::EventStats(_)) || keeps_events(&stage.node))
}

/// Whether a field name is one of the two columns the imposed order sorts
/// by, under the fold `DuckDB` binds identifiers with.
fn is_order_key(name: &str) -> bool {
    let key = catalog_key(name);
    key == TIME || key == INGESTED
}

/// Parse DSL, reducing a failure to its first message.
fn parse(dsl: &str) -> Result<Query, String> {
    trawl_core::parser::parse(dsl).map_err(|errors| {
        errors
            .first()
            .map_or_else(|| "parse error".to_owned(), |e| e.message.clone())
    })
}

/// Whether any stage reads stored report rows. Any position counts, not
/// only the first: a later `from saved` is refused when the text is
/// validated, and no window may be spliced onto it meanwhile.
fn reads_stored(query: &Query) -> bool {
    query
        .pipeline
        .iter()
        .any(|stage| matches!(stage.node, PipeStage::FromSaved(_)))
}

/// The window a request asks for, before it is measured against `T`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Requested {
    /// A span back from `T`.
    Last(Duration),
    /// A start, and an end or `T`.
    Absolute {
        earliest: DateTime<Utc>,
        latest: Option<DateTime<Utc>>,
    },
    /// The configured default span back from `T`.
    Default,
}

impl Requested {
    /// The half-open `[start, end)` this request covers at `t`, refused
    /// when it is empty or longer than the limit.
    fn bounds(
        self,
        t: DateTime<Utc>,
        limits: &Limits,
    ) -> Result<(DateTime<Utc>, DateTime<Utc>), BoundsError> {
        let too_long = || BoundsError::SpanTooLong {
            limit: span_text(limits.max_span),
        };
        // A span no instant arithmetic can hold is longer than any limit.
        let back_from_t = |span: Duration| {
            TimeDelta::from_std(span)
                .ok()
                .and_then(|span| t.checked_sub_signed(span))
                .ok_or_else(too_long)
        };
        let (start, end) = match self {
            Self::Last(span) => (back_from_t(span)?, t),
            Self::Default => (back_from_t(limits.default_window)?, t),
            Self::Absolute { earliest, latest } => (earliest, latest.unwrap_or(t)),
        };
        if start >= end {
            return Err(BoundsError::EmptyWindow {
                earliest: format_window_bound(start),
                latest: format_window_bound(end),
            });
        }
        let max_span = TimeDelta::from_std(limits.max_span).unwrap_or(TimeDelta::MAX);
        if end.signed_duration_since(start) > max_span {
            return Err(too_long());
        }
        Ok((start, end))
    }
}

/// The window a structured request window asks for.
fn requested_by_window(window: &WindowInput) -> Result<Requested, BoundsError> {
    match (&window.last, &window.earliest, &window.latest) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err(BoundsError::MixedClauses),
        (Some(last), None, None) => Ok(Requested::Last(window_span(last)?)),
        (None, Some(earliest), latest) => Ok(Requested::Absolute {
            earliest: window_instant("earliest", earliest)?,
            latest: latest
                .as_deref()
                .map(|latest| window_instant("latest", latest))
                .transpose()?,
        }),
        (None, None, Some(_)) => Err(BoundsError::LatestAlone),
        (None, None, None) => Ok(Requested::Default),
    }
}

/// The window a query's own (lifted) clauses ask for.
fn requested_by_query(search: &SearchStage) -> Result<Requested, BoundsError> {
    let last = search.time_filter.as_ref().map(|tf| tf.node.duration);
    match (last, &search.earliest, &search.latest) {
        // The emitter refuses the same pair, so `/query` would too.
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err(BoundsError::MixedClauses),
        (Some(last), None, None) => Ok(Requested::Last(Duration::from_secs(last.to_seconds()))),
        (None, Some(earliest), latest) => Ok(Requested::Absolute {
            earliest: clause_instant(TimeClause::Earliest, &earliest.node)?,
            latest: latest
                .as_ref()
                .map(|latest| clause_instant(TimeClause::Latest, &latest.node))
                .transpose()?,
        }),
        (None, None, Some(_)) => Err(BoundsError::LatestAlone),
        (None, None, None) => Ok(Requested::Default),
    }
}

/// The instant a query's `earliest=`/`latest=` literal names, read as
/// `DuckDB` reads it against a TIMESTAMP column.
fn clause_instant(clause: TimeClause, value: &str) -> Result<DateTime<Utc>, BoundsError> {
    if value.len() > MAX_TIME_VALUE_BYTES {
        return Err(BoundsError::ClauseTooLong {
            clause,
            bytes: value.len(),
        });
    }
    match literal_timestamp(value) {
        Some(Instant::At(at)) => Ok(at.and_utc()),
        Some(Instant::Infinity | Instant::NegInfinity) | None => Err(BoundsError::InvalidClause {
            clause,
            value: value.to_owned(),
        }),
    }
}

/// A window instant: RFC 3339, its offset applied, truncated to the
/// microseconds every hop stores.
fn window_instant(field: &'static str, value: &str) -> Result<DateTime<Utc>, BoundsError> {
    window_length(field, value)?;
    DateTime::parse_from_rfc3339(value)
        .map(|at| truncate_to_micros(at.with_timezone(&Utc)))
        .map_err(|_| BoundsError::InvalidWindow {
            field,
            value: value.to_owned(),
            expected: RFC3339_GRAMMAR,
        })
}

/// A window span in the DSL's own `last=` grammar, read by the DSL parser
/// so the two cannot drift. The value must be the whole clause: `1h | …`
/// or `1h last=2h` is not a duration.
fn window_span(value: &str) -> Result<Duration, BoundsError> {
    window_length("last", value)?;
    let clause = format!("last={value}");
    let invalid = || BoundsError::InvalidWindow {
        field: "last",
        value: value.to_owned(),
        expected: DURATION_GRAMMAR,
    };
    let query = trawl_core::parser::parse(&clause).map_err(|_| invalid())?;
    let search = &query.search;
    match &search.time_filter {
        Some(tf)
            if tf.span == (0..clause.len())
                && search.groups.is_empty()
                && search.earliest.is_none()
                && search.latest.is_none()
                && query.pipeline.is_empty() =>
        {
            Ok(Duration::from_secs(tf.node.duration.to_seconds()))
        }
        _ => Err(invalid()),
    }
}

/// A window field's value within [`MAX_TIME_VALUE_BYTES`], refused by its
/// length before it is parsed or quoted.
fn window_length(field: &'static str, value: &str) -> Result<(), BoundsError> {
    if value.len() > MAX_TIME_VALUE_BYTES {
        return Err(BoundsError::WindowTooLong {
            field,
            bytes: value.len(),
        });
    }
    Ok(())
}

/// A configured span as the DSL would write it, for a refusal message.
fn span_text(span: Duration) -> String {
    let secs = span.as_secs();
    if secs == 0 || span.subsec_nanos() != 0 {
        return format!("{span:?}");
    }
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        if secs.is_multiple_of(size) {
            return format!("{}{unit}", secs / size);
        }
    }
    format!("{secs}s")
}

// ---------------------------------------------------------------------------
// Finding, cutting and proving time clauses
// ---------------------------------------------------------------------------

/// The clause a search token is, when it is one.
fn direct_clause(token: &SearchToken) -> Option<TimeClause> {
    match token {
        SearchToken::TimeFilter(_) => Some(TimeClause::Last),
        SearchToken::EarliestFilter(_) => Some(TimeClause::Earliest),
        SearchToken::LatestFilter(_) => Some(TimeClause::Latest),
        SearchToken::FieldFilter(_)
        | SearchToken::TextSearch(_)
        | SearchToken::QuotedSearch(_)
        | SearchToken::Not(_)
        | SearchToken::Group(_) => None,
    }
}

/// A time clause anywhere under `token`, lifted or not.
fn clause_in(token: &SearchToken) -> Option<TimeClause> {
    match token {
        SearchToken::TimeFilter(_)
        | SearchToken::EarliestFilter(_)
        | SearchToken::LatestFilter(_) => direct_clause(token),
        SearchToken::Not(inner) => clause_in(&inner.node),
        SearchToken::Group(groups) => clause_in_groups(groups),
        SearchToken::FieldFilter(_) | SearchToken::TextSearch(_) | SearchToken::QuotedSearch(_) => {
            None
        }
    }
}

/// A time clause anywhere in a token tree.
fn clause_in_groups(groups: &[Vec<Spanned<SearchToken>>]) -> Option<TimeClause> {
    groups
        .iter()
        .flatten()
        .find_map(|token| clause_in(&token.node))
}

/// The time clause a query carries anywhere: lifted into the search stage
/// or left in the tree where the parser does not reach.
fn any_clause(query: &Query) -> Option<TimeClause> {
    query
        .time_clause()
        .or_else(|| clause_in_groups(&query.search.groups))
}

/// A clause the parser does not lift: one under a `NOT` whose operand is
/// not itself the clause, such as `NOT (last=1h)` or `NOT NOT last=1h`.
fn unreachable_clause(groups: &[Vec<Spanned<SearchToken>>]) -> Option<TimeClause> {
    groups.iter().flatten().find_map(|token| match &token.node {
        SearchToken::Group(inner) => unreachable_clause(inner),
        // `NOT last=1h`: lifted, and the `NOT` stays behind empty.
        SearchToken::Not(inner) if direct_clause(&inner.node).is_some() => None,
        SearchToken::Not(inner) => clause_in(&inner.node),
        // A clause at a level the parser walks was lifted out of it.
        SearchToken::TimeFilter(_)
        | SearchToken::EarliestFilter(_)
        | SearchToken::LatestFilter(_)
        | SearchToken::FieldFilter(_)
        | SearchToken::TextSearch(_)
        | SearchToken::QuotedSearch(_) => None,
    })
}

/// Whether a token holds nothing once its time clauses are gone: a clause,
/// the empty `NOT` a lifted clause leaves, or parentheses around only
/// those. Such a token filters nothing in the emitter.
fn is_time_only(token: &SearchToken) -> bool {
    match token {
        SearchToken::TimeFilter(_)
        | SearchToken::EarliestFilter(_)
        | SearchToken::LatestFilter(_) => true,
        SearchToken::Not(inner) => direct_clause(&inner.node).is_some(),
        SearchToken::Group(groups) => groups.iter().flatten().all(|t| is_time_only(&t.node)),
        SearchToken::FieldFilter(_) | SearchToken::TextSearch(_) | SearchToken::QuotedSearch(_) => {
            false
        }
    }
}

/// Append to `cuts`, in text order, the bytes to cut so every clause in
/// `clauses` leaves the text: the clause itself, or the outermost token
/// around it that holds nothing else. Returns whether every token in
/// `groups` holds nothing once its clauses are gone.
///
/// Cutting only `last=1h` out of `NOT last=1h` would leave `NOT`, which
/// parses as a search for the word, and cutting it out of `(last=1h)`
/// would leave `()`, which does not parse.
///
/// `clauses` are the sorted spans the parser hoisted from `groups`. Each
/// sits between two tokens, where the parser lifted it out of this level,
/// or inside exactly one token. A token gets its own clauses as a slice and
/// is walked once, so the walk is linear in the tree.
fn collect_cuts(
    groups: &[Vec<Spanned<SearchToken>>],
    mut clauses: &[Range<usize>],
    cuts: &mut Vec<Range<usize>>,
) -> bool {
    let mut time_only = true;
    for token in groups.iter().flatten() {
        let before = clauses.partition_point(|c| c.start < token.span.start);
        cuts.extend_from_slice(&clauses[..before]);
        let inside = clauses[before..].partition_point(|c| c.end <= token.span.end);
        let held = &clauses[before..before + inside];
        clauses = &clauses[before + inside..];

        let mark = cuts.len();
        let holds_only_time = match &token.node {
            SearchToken::Group(inner) => collect_cuts(inner, held, cuts),
            // `NOT last=1h`: lifted, and the `NOT` stays behind empty.
            SearchToken::Not(inner) => direct_clause(&inner.node).is_some(),
            SearchToken::TimeFilter(_)
            | SearchToken::EarliestFilter(_)
            | SearchToken::LatestFilter(_) => true,
            SearchToken::FieldFilter(_)
            | SearchToken::TextSearch(_)
            | SearchToken::QuotedSearch(_) => false,
        };
        if !held.is_empty() {
            if holds_only_time {
                cuts.truncate(mark);
                cuts.push(token.span.clone());
            } else if !matches!(token.node, SearchToken::Group(_)) {
                // A lifted span never sits inside any other token; if one
                // ever did, the proof refuses whatever this cut leaves.
                cuts.extend_from_slice(held);
            }
        }
        time_only &= holds_only_time;
    }
    cuts.extend_from_slice(clauses);
    time_only
}

/// The caller's text, parsed as `query`, with every `last=`, `earliest=`
/// and `latest=` cut out.
///
/// The parse already found every clause: the search stage records each one
/// it hoisted ([`SearchStage::hoisted`]), the ones the last of each kind
/// displaced included. They are cut out together and the rest is parsed
/// once more, to prove no clause is left. The work grows with the text and
/// not with its clauses, which matters because nothing bounds it: a
/// capture resolves before admission.
fn strip_time_clauses(dsl: &str, query: &Query) -> Result<String, BoundsError> {
    let search = &query.search;
    let mut hoisted: Vec<&Spanned<TimeClause>> = search.hoisted.iter().collect();
    hoisted.sort_unstable_by_key(|clause| clause.span.start);
    let Some(first) = hoisted.first().map(|clause| clause.node) else {
        return match clause_in_groups(&search.groups) {
            Some(clause) => Err(BoundsError::ClauseNotTopLevel { clause }),
            None => Ok(dsl.to_owned()),
        };
    };
    let clauses: Vec<Range<usize>> = hoisted.iter().map(|clause| clause.span.clone()).collect();
    let mut cuts = Vec::new();
    collect_cuts(&search.groups, &clauses, &mut cuts);

    // The cuts are in text order; overlapping or adjacent ones merge.
    let mut text = String::with_capacity(dsl.len());
    let mut kept = 0;
    for cut in &cuts {
        if cut.start > kept {
            text.push_str(&dsl[kept..cut.start]);
        }
        kept = kept.max(cut.end);
    }
    text.push_str(&dsl[kept..]);

    let stripped = parse(&text).map_err(|_| BoundsError::ClauseNotTopLevel { clause: first })?;
    match any_clause(&stripped) {
        Some(clause) => Err(BoundsError::ClauseNotTopLevel { clause }),
        None => Ok(text),
    }
}

/// A query with every time clause gone: the lifted fields cleared and the
/// tokens that held only clauses removed, as the parser removes groups
/// that hoisting emptied. Everything removed filtered nothing.
fn without_time(mut query: Query) -> Query {
    query.search.time_filter = None;
    query.search.earliest = None;
    query.search.latest = None;
    prune_time_only(&mut query.search.groups);
    query
}

fn prune_time_only(groups: &mut Vec<Vec<Spanned<SearchToken>>>) {
    for group in groups.iter_mut() {
        group.retain_mut(|token| match &mut token.node {
            // Pruned first, a group that held only time clauses is empty.
            // Reading that, rather than walking the group again, keeps the
            // prune linear in the nesting depth.
            SearchToken::Group(inner) => {
                prune_time_only(inner);
                !inner.is_empty()
            }
            node => !is_time_only(node),
        });
    }
    groups.retain(|group| !group.is_empty());
}

/// Prove `executed` is `original` with only its time clauses changed.
///
/// Both sides are parsed, their time clauses removed, and compared through
/// [`format_query`], which drops source spans: the splice moves every byte,
/// so comparing the trees directly would report a difference for every
/// query. This check does not trust the cutting code; it is what makes the
/// cut safe.
fn prove_unchanged(original: &str, executed: &str) -> Result<(), BoundsError> {
    let (Ok(before), Ok(after)) = (parse(original), parse(executed)) else {
        return Err(BoundsError::SpliceMismatch);
    };
    if format_query(&without_time(before)) == format_query(&without_time(after)) {
        Ok(())
    } else {
        Err(BoundsError::SpliceMismatch)
    }
}

/// Prove `ordered` is `spliced` with exactly [`IMPOSED_ORDER`] appended.
fn prove_order_appended(spliced: &str, ordered: &str) -> Result<(), BoundsError> {
    let (Ok(before), Ok(mut after)) = (parse(spliced), parse(ordered)) else {
        return Err(BoundsError::SpliceMismatch);
    };
    let imposed = after.pipeline.pop().map(|stage| stage.node);
    let expected = PipeStage::Sort(SortStage {
        fields: [TIME, INGESTED]
            .into_iter()
            .map(|field| SortField {
                field: field.to_owned(),
                direction: SortDirection::Desc,
            })
            .collect(),
    });
    if imposed == Some(expected) && format_query(&after) == format_query(&before) {
        Ok(())
    } else {
        Err(BoundsError::SpliceMismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The resolution instant, with nanoseconds the resolver must drop.
    fn now() -> DateTime<Utc> {
        at("2026-10-23T10:20:30.123456789Z")
    }

    /// `now()` as the splice spells `T`.
    const T: &str = "2026-10-23T10:20:30.123456Z";

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("test instant")
            .with_timezone(&Utc)
    }

    fn limits() -> Limits {
        Limits {
            default_window: Duration::from_mins(15),
            max_span: Duration::from_hours(7 * 24),
        }
    }

    fn resolve_dsl(dsl: &str) -> Result<Resolved, BoundsError> {
        resolve_as(dsl, None, now(), &limits(), None)
    }

    fn resolve_window(dsl: &str, window: &WindowInput) -> Result<Resolved, BoundsError> {
        resolve_as(dsl, Some(window), now(), &limits(), None)
    }

    fn last(value: &str) -> WindowInput {
        WindowInput {
            last: Some(value.to_owned()),
            ..WindowInput::default()
        }
    }

    fn absolute(earliest: Option<&str>, latest: Option<&str>) -> WindowInput {
        WindowInput {
            earliest: earliest.map(str::to_owned),
            latest: latest.map(str::to_owned),
            last: None,
        }
    }

    /// The live window a resolution covers, as the splice spells it.
    fn window_of(resolved: &Resolved) -> (String, String) {
        match resolved.source {
            Source::Live { earliest, latest } => {
                (format_window_bound(earliest), format_window_bound(latest))
            }
            Source::Stored => panic!("expected a live source, got {resolved:?}"),
        }
    }

    fn prefix(earliest: &str, latest: &str) -> String {
        format!("earliest=\"{earliest}\" latest=\"{latest}\" ")
    }

    // -- Resolution ---------------------------------------------------------

    #[test]
    fn last_is_recorded_as_absolute_micros_with_the_rest_unchanged() {
        let resolved = resolve_dsl("service=nginx last=1h | stats count() by host").unwrap();

        let start = "2026-10-23T09:20:30.123456Z";
        assert_eq!(window_of(&resolved), (start.to_owned(), T.to_owned()));
        assert_eq!(
            resolved.executed,
            format!("{}service=nginx  | stats count() by host", prefix(start, T)),
            "the clause is cut, the window spliced in front, every other byte kept"
        );
        assert_eq!(resolved.shape, Shape::Whole);
    }

    #[test]
    fn the_default_window_is_the_configured_span_back_from_t() {
        let resolved = resolve_dsl("service=nginx").unwrap();

        let start = "2026-10-23T10:05:30.123456Z";
        assert_eq!(window_of(&resolved), (start.to_owned(), T.to_owned()));
        assert_eq!(
            resolved.executed,
            format!("{}service=nginx\n{IMPOSED_ORDER}", prefix(start, T))
        );
        assert_eq!(resolved.shape, Shape::Events);
    }

    #[test]
    fn an_event_query_leaves_one_stage_of_the_cap_for_the_order() {
        let staged = |stages: usize, tail: &str| {
            format!(
                "service=nginx{}{tail}",
                " | where status >= 0".repeat(stages)
            )
        };
        let fits = resolve_dsl(&staged(MAX_EVENT_STAGES, "")).unwrap();
        assert_eq!(fits.shape, Shape::Events);
        let executed = trawl_core::parser::parse(&fits.executed).unwrap();
        assert_eq!(executed.pipeline.len(), MAX_PIPELINE_STAGES);

        let refused = resolve_dsl(&staged(MAX_PIPELINE_STAGES, "")).unwrap_err();
        assert_eq!(
            refused,
            BoundsError::TooManyStages {
                stages: MAX_PIPELINE_STAGES
            }
        );
        assert_eq!(
            refused.to_string(),
            "this query has 128 pipeline stages, over the limit of 127 for a capture of events, \
             which ends with its own `| sort -_time, -_ingested` stage; shorten the pipeline"
        );

        // A whole-shaped query appends nothing and keeps the whole cap.
        let whole = resolve_dsl(&staged(MAX_EVENT_STAGES, " | stats count()")).unwrap();
        assert_eq!(whole.shape, Shape::Whole);
    }

    #[test]
    fn an_imposed_event_shape_orders_a_query_classify_calls_whole() {
        let dsl = "error | eventstats count() by service, env";
        let classified = resolve_dsl(dsl).unwrap();
        assert_eq!(classified.shape, Shape::Whole);

        let imposed = resolve_as(dsl, None, now(), &limits(), Some(Shape::Events)).unwrap();
        assert_eq!(imposed.shape, Shape::Events);
        assert_eq!(imposed.source, classified.source);
        assert_eq!(
            imposed.executed,
            format!("{}\n{IMPOSED_ORDER}", classified.executed),
            "the same splice, with the order appended"
        );
        let executed = trawl_core::parser::parse(&imposed.executed).unwrap();
        assert!(matches!(
            executed.pipeline.last().map(|stage| &stage.node),
            Some(PipeStage::Sort(_))
        ));
    }

    #[test]
    fn an_imposed_event_shape_leaves_one_stage_of_the_cap_for_the_order() {
        let staged = |stages: usize| {
            format!(
                "error{} | eventstats count() by service",
                " | where status >= 0".repeat(stages - 1)
            )
        };
        let events = |dsl: &str| resolve_as(dsl, None, now(), &limits(), Some(Shape::Events));
        assert_eq!(
            events(&staged(MAX_EVENT_STAGES)).unwrap().shape,
            Shape::Events
        );
        assert_eq!(
            events(&staged(MAX_PIPELINE_STAGES)).unwrap_err(),
            BoundsError::TooManyStages {
                stages: MAX_PIPELINE_STAGES
            }
        );
        // Classified whole, the same query keeps the whole cap.
        assert_eq!(
            resolve_dsl(&staged(MAX_PIPELINE_STAGES)).unwrap().shape,
            Shape::Whole
        );
    }

    /// The sightings builder's own text takes the imposed shape, and so
    /// does a plain search, whose rows are events already. A pipeline that
    /// orders, limits, samples or aggregates is refused as the server's
    /// fault: only server-composed queries impose a shape. No `eventstats`
    /// can write an order column, which the parser refuses.
    #[test]
    fn the_event_shape_is_imposed_only_on_the_eventstats_form() {
        let events = |dsl: &str| resolve_as(dsl, None, now(), &limits(), Some(Shape::Events));
        let sightings = trawl_core::correlate::sightings_query("10.0.4.17").unwrap();
        for dsl in [
            sightings.text(),
            "service=nginx error",
            "* | where status >= 500 | eventstats count() by service",
        ] {
            assert_eq!(events(dsl).unwrap().shape, Shape::Events, "{dsl}");
        }
        for dsl in [
            "* | sort status | head 10000",
            "* | head 10",
            "* | tail 10",
            "* | sample 10%",
            "* | eventstats count() by service | sort -count",
            "* | eventstats count() by service | head 5",
            "* | stats count() by service",
            "* | dedup host",
            "* | drop _ingested",
        ] {
            assert_eq!(
                events(dsl).unwrap_err(),
                BoundsError::ShapeNotImposable,
                "{dsl}"
            );
        }
        assert!(matches!(
            events("* | eventstats max(_time) as _time by service"),
            Err(BoundsError::Unparseable { .. })
        ));
        // Without the override nothing changes: the same text resolves as
        // classified.
        assert_eq!(
            resolve_dsl("* | sort status | head 10000").unwrap().shape,
            Shape::Whole
        );
    }

    #[test]
    fn window_bounds_measure_a_window_as_a_capture_does() {
        let resolved = resolve_window("service=nginx", &last("2h")).unwrap();
        let Source::Live { earliest, latest } = resolved.source else {
            panic!("a live source");
        };
        assert_eq!(
            window_bounds(&last("2h"), now(), &limits()),
            Ok((earliest, latest))
        );
        assert_eq!(
            window_bounds(&WindowInput::default(), now(), &limits())
                .map(|(start, end)| end - start),
            Ok(TimeDelta::minutes(15))
        );
        assert!(matches!(
            window_bounds(&last("8d"), now(), &limits()),
            Err(BoundsError::SpanTooLong { .. })
        ));
        let latest_alone = WindowInput {
            latest: Some("2026-10-23T09:00:00Z".into()),
            ..WindowInput::default()
        };
        assert_eq!(
            window_bounds(&latest_alone, now(), &limits()),
            Err(BoundsError::LatestAlone)
        );
    }

    #[test]
    fn a_stored_capture_stays_whole_under_an_imposed_shape() {
        let resolved = resolve_as(
            "| from saved daily_errors run=latest",
            None,
            now(),
            &limits(),
            Some(Shape::Events),
        )
        .unwrap();
        assert_eq!(resolved.source, Source::Stored);
        assert_eq!(resolved.shape, Shape::Whole);
        assert_eq!(resolved.executed, "| from saved daily_errors run=latest");
    }

    #[test]
    fn an_open_end_closes_at_t() {
        let resolved = resolve_dsl(r#"earliest="2026-10-23T10:00:00Z" error"#).unwrap();
        assert_eq!(
            window_of(&resolved),
            ("2026-10-23T10:00:00.000000Z".to_owned(), T.to_owned())
        );
    }

    #[test]
    fn earliest_and_latest_bound_both_ends() {
        let resolved = resolve_dsl(
            r#"earliest="2026-10-22T08:00:00Z" latest="2026-10-22T09:30:00Z" | stats count()"#,
        )
        .unwrap();
        assert_eq!(
            window_of(&resolved),
            (
                "2026-10-22T08:00:00.000000Z".to_owned(),
                "2026-10-22T09:30:00.000000Z".to_owned()
            )
        );
        assert_eq!(
            resolved.executed,
            format!(
                "{}  | stats count()",
                prefix("2026-10-22T08:00:00.000000Z", "2026-10-22T09:30:00.000000Z")
            )
        );
    }

    #[test]
    fn a_query_literal_reads_as_duckdb_reads_it() {
        // `DuckDB` casts the literal at wall clock, dropping a spelled
        // offset, and `/query` applies exactly that: the capture's bound
        // must be the same instant.
        let resolved = resolve_dsl(r#"earliest="2026-10-23T09:00:00+05:30""#).unwrap();
        assert_eq!(window_of(&resolved).0, "2026-10-23T09:00:00.000000Z");

        let resolved = resolve_dsl(r#"earliest="2026-10-23 09:00""#).unwrap();
        assert_eq!(window_of(&resolved).0, "2026-10-23T09:00:00.000000Z");
    }

    #[test]
    fn microsecond_precision_survives_and_finer_digits_truncate() {
        let resolved = resolve_dsl(
            r#"earliest="2026-10-23T10:00:00.000001Z" latest="2026-10-23T10:00:00.9999999Z""#,
        )
        .unwrap();
        let (start, end) = window_of(&resolved);
        assert_eq!(start, "2026-10-23T10:00:00.000001Z");
        assert_eq!(end, "2026-10-23T10:00:00.999999Z");
        assert!(resolved.executed.starts_with(&prefix(&start, &end)));

        let window = absolute(
            Some("2026-10-23T10:00:00.0000019Z"),
            Some("2026-10-23T10:00:01Z"),
        );
        let resolved = resolve_window("*", &window).unwrap();
        assert_eq!(window_of(&resolved).0, "2026-10-23T10:00:00.000001Z");
    }

    #[test]
    fn exactly_the_max_span_is_accepted_and_one_microsecond_more_is_refused() {
        let exact = resolve_dsl(r#"earliest="2026-10-16T10:00:00Z" latest="2026-10-23T10:00:00Z""#)
            .unwrap();
        assert_eq!(window_of(&exact).0, "2026-10-16T10:00:00.000000Z");

        let over =
            resolve_dsl(r#"earliest="2026-10-16T10:00:00Z" latest="2026-10-23T10:00:00.000001Z""#);
        assert_eq!(
            over,
            Err(BoundsError::SpanTooLong {
                limit: "7d".to_owned()
            })
        );

        for ok in ["last=7d", "last=1w", "last=168h"] {
            resolve_dsl(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        assert!(matches!(
            resolve_dsl("last=169h"),
            Err(BoundsError::SpanTooLong { .. })
        ));
        resolve_window("*", &last("7d")).unwrap();
        assert!(matches!(
            resolve_window("*", &last("8d")),
            Err(BoundsError::SpanTooLong { .. })
        ));
        let window = absolute(
            Some("2026-10-16T10:00:00Z"),
            Some("2026-10-23T10:00:00.000001Z"),
        );
        assert!(matches!(
            resolve_window("*", &window),
            Err(BoundsError::SpanTooLong { .. })
        ));
    }

    #[test]
    fn a_span_past_instant_arithmetic_is_too_long() {
        let huge = format!("last={}s", u64::MAX / 2);
        assert!(matches!(
            resolve_dsl(&huge),
            Err(BoundsError::SpanTooLong { .. })
        ));
    }

    #[test]
    fn latest_alone_is_refused() {
        assert_eq!(
            resolve_dsl(r#"latest="2026-10-23T10:00:00Z" error"#),
            Err(BoundsError::LatestAlone)
        );
        assert_eq!(
            resolve_window("*", &absolute(None, Some("2026-10-23T10:00:00Z"))),
            Err(BoundsError::LatestAlone)
        );
    }

    #[test]
    fn last_beside_an_absolute_bound_is_refused() {
        assert_eq!(
            resolve_dsl(r#"last=1h earliest="2026-10-23T10:00:00Z""#),
            Err(BoundsError::MixedClauses)
        );
        assert_eq!(
            resolve_dsl(r#"latest="2026-10-23T10:00:00Z" last=1h"#),
            Err(BoundsError::MixedClauses)
        );
        let window = WindowInput {
            earliest: Some("2026-10-23T10:00:00Z".to_owned()),
            latest: None,
            last: Some("1h".to_owned()),
        };
        assert_eq!(resolve_window("*", &window), Err(BoundsError::MixedClauses));
    }

    #[test]
    fn an_empty_window_is_refused() {
        let same = resolve_dsl(r#"earliest="2026-10-23T10:00:00Z" latest="2026-10-23T10:00:00Z""#);
        assert_eq!(
            same,
            Err(BoundsError::EmptyWindow {
                earliest: "2026-10-23T10:00:00.000000Z".to_owned(),
                latest: "2026-10-23T10:00:00.000000Z".to_owned(),
            })
        );
        assert!(matches!(
            resolve_dsl(r#"earliest="2026-10-23T10:00:01Z" latest="2026-10-23T10:00:00Z""#),
            Err(BoundsError::EmptyWindow { .. })
        ));
        // A start after T with an open end closes before it opens.
        assert!(matches!(
            resolve_dsl(r#"earliest="2026-10-23T11:00:00Z""#),
            Err(BoundsError::EmptyWindow { .. })
        ));
    }

    #[test]
    fn an_unreadable_query_instant_is_refused() {
        for (dsl, clause, value) in [
            (r#"earliest="yesterday""#, TimeClause::Earliest, "yesterday"),
            (
                r#"earliest="-infinity" latest="2026-10-23T10:00:00Z""#,
                TimeClause::Earliest,
                "-infinity",
            ),
            (
                r#"earliest="2026-10-23T09:00:00Z" latest="infinity""#,
                TimeClause::Latest,
                "infinity",
            ),
        ] {
            assert_eq!(
                resolve_dsl(dsl),
                Err(BoundsError::InvalidClause {
                    clause,
                    value: value.to_owned()
                }),
                "{dsl}"
            );
        }
    }

    #[test]
    fn an_unreadable_window_field_is_refused() {
        for (window, field) in [
            (absolute(Some("2026-10-23"), None), "earliest"),
            (absolute(Some("2026-10-23 10:00"), None), "earliest"),
            (
                absolute(Some("2026-10-23T09:00:00Z"), Some("soon")),
                "latest",
            ),
            (last("0h"), "last"),
            (last("1y"), "last"),
            (last(" 1h"), "last"),
            (last("1h | stats count()"), "last"),
            (last("1h last=2h"), "last"),
            (last("1h # 2h"), "last"),
            (last("1h service=x"), "last"),
        ] {
            let err = resolve_window("*", &window).unwrap_err();
            assert!(
                matches!(&err, BoundsError::InvalidWindow { field: f, .. } if *f == field),
                "{window:?}: {err:?}"
            );
        }
    }

    /// A time value over [`MAX_TIME_VALUE_BYTES`] is refused by its
    /// length, whatever it holds; one at the limit reads as its grammar.
    #[test]
    fn a_time_value_over_the_limit_is_refused_by_its_length() {
        let long = "1".repeat(MAX_TIME_VALUE_BYTES + 1);
        for (window, field) in [
            (last(&long), "last"),
            (absolute(Some(&long), None), "earliest"),
            (
                absolute(Some("2026-10-23T09:00:00Z"), Some(&long)),
                "latest",
            ),
        ] {
            assert_eq!(
                resolve_window("*", &window),
                Err(BoundsError::WindowTooLong { field, bytes: 129 }),
                "{field}"
            );
        }
        assert!(matches!(
            resolve_window("*", &last(&"1".repeat(MAX_TIME_VALUE_BYTES))),
            Err(BoundsError::InvalidWindow { field: "last", .. })
        ));
        assert_eq!(
            resolve_dsl(&format!("earliest=\"{long}\"")),
            Err(BoundsError::ClauseTooLong {
                clause: TimeClause::Earliest,
                bytes: 129
            })
        );
    }

    #[test]
    fn a_window_alone_resolves_and_applies_its_offset() {
        let resolved = resolve_window("service=x", &last("1h")).unwrap();
        let start = "2026-10-23T09:20:30.123456Z";
        assert_eq!(window_of(&resolved), (start.to_owned(), T.to_owned()));
        assert_eq!(
            resolved.executed,
            format!("{}service=x\n{IMPOSED_ORDER}", prefix(start, T))
        );

        // An API instant applies its offset, unlike a DSL literal.
        let window = absolute(
            Some("2026-10-23T11:00:00+02:00"),
            Some("2026-10-23T12:00:00+02:00"),
        );
        let resolved = resolve_window("*", &window).unwrap();
        assert_eq!(
            window_of(&resolved),
            (
                "2026-10-23T09:00:00.000000Z".to_owned(),
                "2026-10-23T10:00:00.000000Z".to_owned()
            )
        );

        let window = absolute(Some("2026-10-23T10:00:00Z"), None);
        assert_eq!(window_of(&resolve_window("*", &window).unwrap()).1, T);
    }

    #[test]
    fn a_window_with_no_field_set_is_no_window() {
        let resolved = resolve_window("last=1h", &WindowInput::default()).unwrap();
        assert_eq!(window_of(&resolved).0, "2026-10-23T09:20:30.123456Z");
    }

    #[test]
    fn a_window_beside_a_query_clause_is_refused() {
        for (dsl, clause) in [
            ("last=2h", TimeClause::Last),
            (
                r#"error earliest="2026-10-23T10:00:00Z""#,
                TimeClause::Earliest,
            ),
            (r#"latest="2026-10-23T10:00:00Z""#, TimeClause::Latest),
            ("NOT (last=2h)", TimeClause::Last),
        ] {
            assert_eq!(
                resolve_window(dsl, &last("1h")),
                Err(BoundsError::WindowAndClause { clause }),
                "{dsl}"
            );
        }
    }

    #[test]
    fn from_saved_passes_through_with_no_bounds() {
        let dsl = "| from saved daily_errors run=latest | stats count()";
        let resolved = resolve_dsl(dsl).unwrap();
        assert_eq!(
            resolved,
            Resolved {
                executed: dsl.to_owned(),
                source: Source::Stored,
                shape: Shape::Whole,
            }
        );

        // `| from saved x` alone has no other stage, and is still whole:
        // stored results are one answer.
        assert_eq!(
            resolve_dsl("| from saved daily_errors").unwrap().shape,
            Shape::Whole
        );

        assert_eq!(
            resolve_window(dsl, &last("1h")),
            Err(BoundsError::WindowOnStored)
        );
        assert_eq!(
            resolve_dsl("last=1h | from saved daily_errors"),
            Err(BoundsError::ClauseOnStored {
                clause: TimeClause::Last
            })
        );
        // A later `from saved` is refused when the text is validated; no
        // window is spliced onto it here.
        let later = "* | stats count() | from saved daily_errors";
        assert_eq!(resolve_dsl(later).unwrap().source, Source::Stored);
    }

    #[test]
    fn unparseable_text_is_refused() {
        assert!(matches!(
            resolve_dsl("| | broken {{{"),
            Err(BoundsError::Unparseable { .. })
        ));
    }

    /// The parser's message can echo the query whole, so the refusal
    /// quotes an excerpt of it and its length; a short one is still named.
    #[test]
    fn an_unparseable_refusal_excerpts_the_parser_message() {
        let longest = format!("| {}", "z".repeat(trawl_core::parser::MAX_QUERY_LEN - 2));
        let parser = parse(&longest).unwrap_err().len();
        let refused = resolve_dsl(&longest).unwrap_err();
        assert!(
            matches!(refused, BoundsError::Unparseable { .. }),
            "{refused:?}"
        );
        let message = refused.to_string();
        assert!(message.len() < 256, "{} bytes", message.len());
        assert!(
            message.ends_with(&format!("… ({parser} bytes)")),
            "{message}"
        );

        let short = resolve_dsl("| zz").unwrap_err();
        assert!(
            matches!(short, BoundsError::Unparseable { .. }),
            "{short:?}"
        );
        assert!(short.to_string().contains("zz"), "{short}");
    }

    // -- Where a clause sits ------------------------------------------------

    /// Every position the parser lifts a clause from: the clause is cut,
    /// the window it named is the one spliced, and nothing else changes. A
    /// parenthesized token's span runs over the padding after it, so a cut
    /// group takes its trailing space along.
    #[test]
    fn a_clause_is_found_and_cut_in_every_lifted_position() {
        let hour_ago = "2026-10-23T09:20:30.123456Z";
        let two_hours_ago = "2026-10-23T08:20:30.123456Z";
        for (dsl, start, body) in [
            ("last=1h service=x", hour_ago, " service=x"),
            ("service=x last=1h", hour_ago, "service=x "),
            (
                "service=x last=1h | stats count()",
                hour_ago,
                "service=x  | stats count()",
            ),
            ("service=x OR last=1h", hour_ago, "service=x OR "),
            ("last=1h OR service=x", hour_ago, " OR service=x"),
            ("(a OR last=1h) b", hour_ago, "(a OR ) b"),
            ("(last=1h) b", hour_ago, "b"),
            ("((last=1h)) b", hour_ago, "b"),
            ("NOT last=1h b", hour_ago, " b"),
            ("(NOT last=1h) b", hour_ago, "b"),
            ("(a NOT last=1h) b", hour_ago, "(a ) b"),
            ("b NOT last=1h", hour_ago, "b "),
            // Repeats: the last of each kind wins, as on `/query`, and a
            // top-level clause beats one lifted out of NOT.
            ("last=1h b last=2h", two_hours_ago, " b "),
            ("last=2h b last=1h", hour_ago, " b "),
            ("NOT last=2h last=1h", hour_ago, " "),
            ("last=1h NOT last=2h", hour_ago, " "),
            ("last=1h (last=2h OR a)", hour_ago, " ( OR a)"),
        ] {
            let resolved = resolve_dsl(dsl).unwrap_or_else(|e| panic!("{dsl}: {e}"));
            assert_eq!(
                window_of(&resolved),
                (start.to_owned(), T.to_owned()),
                "{dsl}"
            );
            let spliced = resolved
                .executed
                .strip_suffix(&format!("\n{IMPOSED_ORDER}"))
                .unwrap_or(&resolved.executed);
            assert_eq!(spliced, format!("{}{body}", prefix(start, T)), "{dsl}");
        }
    }

    #[test]
    fn absolute_clauses_are_found_and_cut_in_every_lifted_position() {
        let e1 = r#"earliest="2026-10-23T09:00:00Z""#;
        let e2 = r#"earliest="2026-10-23T10:00:00Z""#;
        let l1 = r#"latest="2026-10-23T10:10:00Z""#;
        for (dsl, start, end) in [
            (format!("{e1} {l1} a"), "09:00", "10:10"),
            (format!("a OR {e1} OR {l1}"), "09:00", "10:10"),
            (format!("NOT {e1} (b OR {l1})"), "09:00", "10:10"),
            (format!("{e1} a {e2}"), "10:00", T),
            (format!("{e2} a NOT {e1}"), "10:00", T),
            (format!("a {e1} | where b == 1"), "09:00", T),
        ] {
            let resolved = resolve_dsl(&dsl).unwrap_or_else(|e| panic!("{dsl}: {e}"));
            let full = |hm: &str| {
                if hm == T {
                    T.to_owned()
                } else {
                    format!("2026-10-23T{hm}:00.000000Z")
                }
            };
            assert_eq!(window_of(&resolved), (full(start), full(end)), "{dsl}");
            let after = parse(&resolved.executed).unwrap();
            assert!(
                clause_in_groups(&after.search.groups).is_none(),
                "{dsl}: no clause may survive in the tree: {}",
                resolved.executed
            );
        }
    }

    /// A query of thousands of clauses resolves in one pass over its text,
    /// in every position the parser lifts a clause from, and the last
    /// clause still wins. The resolution runs before admission, where no
    /// lane limit or deadline bounds it, so its work must grow with the
    /// text alone: cutting one clause per parse took minutes here.
    #[test]
    fn thousands_of_clauses_resolve_in_one_pass() {
        /// The parser's input limit (`trawl_core::parser::MAX_QUERY_LEN`).
        const MAX_QUERY_LEN: usize = 65_536;
        /// Far above one pass over 64 KiB in an unoptimized build under
        /// load, far below a pass per clause in an optimized one.
        const BUDGET: Duration = Duration::from_secs(5);

        let hour_ago = "2026-10-23T09:20:30.123456Z";
        let single = resolve_dsl("last=1h").unwrap();
        assert_eq!(window_of(&single), (hour_ago.to_owned(), T.to_owned()));
        for (earlier, last) in [
            ("last=2h ", "last=1h "),
            ("NOT last=2h ", "last=1h "),
            ("(last=2h) ", "last=1h "),
            ("a OR last=2h ", "last=1h "),
            ("(b NOT last=2h) ", "last=1h "),
        ] {
            let count = (MAX_QUERY_LEN - last.len()) / earlier.len();
            let dsl = format!("{}{last}", earlier.repeat(count));
            assert!(dsl.len() <= MAX_QUERY_LEN, "{earlier}: {}", dsl.len());

            let started = std::time::Instant::now();
            let resolved = resolve_dsl(&dsl).unwrap_or_else(|e| panic!("{earlier}: {e}"));
            let elapsed = started.elapsed();

            assert_eq!(
                (window_of(&resolved), resolved.source, resolved.shape),
                (window_of(&single), single.source, single.shape),
                "{earlier}: the last clause wins, as alone"
            );
            let after = parse(&resolved.executed).unwrap();
            assert!(
                after.time_clause() == Some(TimeClause::Earliest)
                    && clause_in_groups(&after.search.groups).is_none(),
                "{earlier}: only the spliced window is left"
            );
            assert!(
                elapsed < BUDGET,
                "{earlier}: {count} clauses resolved in {elapsed:?}"
            );
        }

        // Every clause is cut, and every byte between them kept.
        let dsl = format!("{}last=1h ", "last=2h ".repeat(8190));
        assert_eq!(
            resolve_dsl(&dsl).unwrap().executed,
            format!(
                "{}{}\n{IMPOSED_ORDER}",
                prefix(hour_ago, T),
                " ".repeat(8191)
            )
        );
    }

    /// A clause under thousands of nested parentheses resolves in time
    /// that grows with the text, not with the depth times the text. The
    /// test runs on a stack sized for the depth: an unoptimized parser
    /// spends about a kilobyte of it per level.
    #[test]
    fn deeply_nested_groups_resolve_in_linear_time() {
        const DEPTH: usize = 16_384;
        /// The walk per level this guards against took 29 s on a fast
        /// desktop in an unoptimized build; the linear pass takes 2 s there
        /// and has taken 5.03 s on a hosted CI runner sharing two cores with
        /// the rest of its test slice. 15 s sits between, a threefold margin
        /// over the slowest linear run and half the fastest quadratic one.
        const BUDGET: Duration = Duration::from_secs(15);

        let dsl = format!("{}a last=1h{}", "(".repeat(DEPTH), ")".repeat(DEPTH));
        let (resolved, elapsed) = std::thread::Builder::new()
            .stack_size(512 << 20)
            .spawn(move || {
                let started = std::time::Instant::now();
                let resolved = resolve_dsl(&dsl);
                (resolved, started.elapsed())
            })
            .expect("spawn the resolving thread")
            .join()
            .expect("the resolution finishes");

        let resolved = resolved.unwrap();
        assert_eq!(window_of(&resolved).0, "2026-10-23T09:20:30.123456Z");
        assert!(elapsed < BUDGET, "{DEPTH} levels resolved in {elapsed:?}");
    }

    #[test]
    fn a_clause_the_parser_does_not_lift_is_refused() {
        for (dsl, clause) in [
            ("NOT (last=1h)", TimeClause::Last),
            ("a NOT (b OR last=1h)", TimeClause::Last),
            ("NOT NOT last=1h", TimeClause::Last),
            (
                r#"NOT (earliest="2026-10-23T09:00:00Z")"#,
                TimeClause::Earliest,
            ),
            (
                r#"x NOT (a latest="2026-10-23T09:00:00Z")"#,
                TimeClause::Latest,
            ),
            // A lifted clause beside an unreachable one still refuses.
            ("last=1h NOT (last=2h)", TimeClause::Last),
        ] {
            assert_eq!(
                resolve_dsl(dsl),
                Err(BoundsError::ClauseNotTopLevel { clause }),
                "{dsl}"
            );
        }
    }

    #[test]
    fn comments_and_backticked_names_are_not_clauses() {
        // A backticked keyword is a field, a comment is not text, and the
        // imposed order survives a trailing comment on its own line.
        let dsl = "`last`=5 `earliest`=x error # last=2h";
        let resolved = resolve_dsl(dsl).unwrap();
        assert_eq!(
            window_of(&resolved).0,
            "2026-10-23T10:05:30.123456Z",
            "no clause, so the default window"
        );
        assert_eq!(
            resolved.executed,
            format!(
                "{}{dsl}\n{IMPOSED_ORDER}",
                prefix("2026-10-23T10:05:30.123456Z", T)
            )
        );
        let reparsed = parse(&resolved.executed).unwrap();
        assert!(matches!(
            reparsed.pipeline.last().map(|s| &s.node),
            Some(PipeStage::Sort(_))
        ));

        // A real clause beside them is cut; they stay.
        let resolved = resolve_dsl("`last`=5 last=1h # note\n| where `latest` == 1").unwrap();
        assert_eq!(
            resolved.executed,
            format!(
                "{}`last`=5  # note\n| where `latest` == 1\n{IMPOSED_ORDER}",
                prefix("2026-10-23T09:20:30.123456Z", T)
            )
        );

        // A clause commented out is no clause at all.
        let resolved = resolve_dsl("error\n# last=1h\n").unwrap();
        assert_eq!(window_of(&resolved).0, "2026-10-23T10:05:30.123456Z");
    }

    // -- Shape --------------------------------------------------------------

    /// The variant's name, by an exhaustive match: a new stage fails to
    /// compile here too, until the table below covers it.
    fn stage_name(stage: &PipeStage) -> &'static str {
        match stage {
            PipeStage::Stats(_) => "stats",
            PipeStage::Where(_) => "where",
            PipeStage::Sort(_) => "sort",
            PipeStage::Limit(_) => "limit",
            PipeStage::Table(_) => "table",
            PipeStage::Top(_) => "top",
            PipeStage::Rare(_) => "rare",
            PipeStage::Drop(_) => "drop",
            PipeStage::Let(_) => "let",
            PipeStage::Extract(_) => "extract",
            PipeStage::Dedup(_) => "dedup",
            PipeStage::Timechart(_) => "timechart",
            PipeStage::Pivot(_) => "pivot",
            PipeStage::Tail(_) => "tail",
            PipeStage::Rename(_) => "rename",
            PipeStage::Sample(_) => "sample",
            PipeStage::EventStats(_) => "eventstats",
            PipeStage::FromSaved(_) => "from_saved",
        }
    }

    /// Variants `stage_name` knows. Kept beside it so the two change
    /// together.
    const STAGE_VARIANTS: usize = 18;

    #[test]
    fn every_stage_is_classified() {
        let table = [
            ("* | where status == 500", Shape::Events),
            (r#"* | extract "(?P<ip>\d+)" from message"#, Shape::Events),
            (r#"* | rex "(?P<ip>\d+)""#, Shape::Events),
            ("* | extract kv", Shape::Events),
            ("* | let ms = duration * 1000", Shape::Events),
            ("* | eval ms = duration * 1000", Shape::Events),
            ("* | let t = _time", Shape::Events),
            ("* | drop message", Shape::Events),
            ("* | rename host as origin", Shape::Events),
            ("* | stats count() by host", Shape::Whole),
            ("* | sort -status", Shape::Whole),
            ("* | limit 5", Shape::Whole),
            ("* | head 5", Shape::Whole),
            ("* | table _time, message", Shape::Whole),
            ("* | fields _time, message", Shape::Whole),
            ("* | top 3 host", Shape::Whole),
            ("* | rare 3 host", Shape::Whole),
            ("* | dedup host", Shape::Whole),
            ("* | timechart span=5m count()", Shape::Whole),
            ("* | pivot count() on status by host", Shape::Whole),
            ("* | tail 5", Shape::Whole),
            ("* | sample 10%", Shape::Whole),
            ("* | sample 100", Shape::Whole),
            ("* | eventstats avg(duration) by host", Shape::Whole),
            ("| from saved daily_errors", Shape::Whole),
        ];

        let mut covered = std::collections::BTreeSet::new();
        for (dsl, shape) in table {
            let query = trawl_core::parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl}: {e:?}"));
            assert_eq!(classify(&query), shape, "{dsl}");
            covered.extend(query.pipeline.iter().map(|s| stage_name(&s.node)));
        }
        assert_eq!(
            covered.len(),
            STAGE_VARIANTS,
            "every stage variant needs a row: {covered:?}"
        );

        // No pipeline at all is a plain search: events.
        let plain = trawl_core::parser::parse("service=nginx error").unwrap();
        assert_eq!(classify(&plain), Shape::Events);
        // One whole stage anywhere makes the result whole.
        let mixed = trawl_core::parser::parse("* | where a == 1 | sort a | let b = 1").unwrap();
        assert_eq!(classify(&mixed), Shape::Whole);
    }

    #[test]
    fn touching_time_or_ingested_makes_the_result_whole() {
        for dsl in [
            "* | drop _time",
            "* | drop _TIME",
            "* | drop message, _ingested",
            "* | rename _time as t",
            "* | rename _Ingested as arrived",
        ] {
            let query = trawl_core::parser::parse(dsl).unwrap();
            assert_eq!(classify(&query), Shape::Whole, "{dsl}");
        }

        // The parser refuses a `_` target, so a shadowing `let` or a
        // rename onto the column only exists as a built tree. The rule
        // still holds for it.
        for target in ["_time", "_ingested", "_TIME"] {
            let mut query = trawl_core::parser::parse("* | let x = 1 | rename a as b").unwrap();
            if let PipeStage::Let(stage) = &mut query.pipeline[0].node {
                stage.assignments[0].0 = target.to_owned();
            }
            assert_eq!(classify(&query), Shape::Whole, "let {target}");

            let mut query = trawl_core::parser::parse("* | rename a as b").unwrap();
            if let PipeStage::Rename(stage) = &mut query.pipeline[0].node {
                stage.renames[0].1 = target.to_owned();
            }
            assert_eq!(classify(&query), Shape::Whole, "rename onto {target}");
        }
    }

    #[test]
    fn an_events_text_ends_with_the_imposed_order_and_reparses() {
        for dsl in [
            "service=nginx",
            "last=1h error | where status == 500 | let s = status",
            "error # trailing comment",
            "* | extract kv",
        ] {
            let resolved = resolve_dsl(dsl).unwrap();
            assert_eq!(resolved.shape, Shape::Events, "{dsl}");
            assert!(
                resolved.executed.ends_with(&format!("\n{IMPOSED_ORDER}")),
                "{dsl}: {}",
                resolved.executed
            );
            let query = trawl_core::parser::parse(&resolved.executed).unwrap();
            let Some(PipeStage::Sort(sort)) = query.pipeline.last().map(|s| &s.node) else {
                panic!("{dsl}: the last stage is not a sort: {}", resolved.executed);
            };
            let keys: Vec<(&str, SortDirection)> = sort
                .fields
                .iter()
                .map(|f| (f.field.as_str(), f.direction))
                .collect();
            assert_eq!(
                keys,
                [(TIME, SortDirection::Desc), (INGESTED, SortDirection::Desc)]
            );
        }

        // A whole result is never reordered.
        let whole = resolve_dsl("* | stats count() by host").unwrap();
        assert!(!whole.executed.contains("| sort"), "{}", whole.executed);
    }

    #[test]
    fn every_executed_text_is_admitted_and_emits() {
        for dsl in [
            "service=nginx last=1h",
            "(NOT last=1h) error OR warn",
            r#"earliest="2026-10-23T09:00:00Z" | stats count() by host"#,
            "* | extract kv | where a == 1",
            "| from saved daily_errors",
        ] {
            let resolved = resolve_dsl(dsl).unwrap();
            crate::admission::check_dsl(&resolved.executed)
                .unwrap_or_else(|e| panic!("{dsl}: {e:?}"));
            if resolved.source == Source::Stored {
                continue;
            }
            let query = trawl_core::parser::parse(&resolved.executed).unwrap();
            let emitted = trawl_core::emitter::emit(
                &query,
                "/data/prod/*.parquet",
                trawl_core::context::EvalContext::capture(),
            )
            .unwrap_or_else(|e| panic!("{dsl}: {e:?}"));
            let (start, end) = window_of(&resolved);
            let params = format!("{:?}", emitted.params);
            assert!(
                params.contains(&start) && params.contains(&end),
                "{dsl}: the bounds bind as parameters: {params}"
            );
        }
    }

    // -- The proofs ---------------------------------------------------------

    #[test]
    fn the_proof_accepts_an_honest_splice() {
        prove_unchanged(
            "service=x NOT last=1h | stats count()",
            &format!("{}service=x  | stats count()", prefix(T, T)),
        )
        .unwrap();
        prove_unchanged("(last=1h) a OR b", r#"earliest="x" latest="y"  a OR b"#).unwrap();
    }

    #[test]
    fn the_proof_catches_a_corrupted_splice() {
        let original = "service=x error last=1h | stats count() by host";
        for corrupted in [
            // A value changed.
            r#"earliest="a" latest="b" service=y error  | stats count() by host"#,
            // A token lost.
            r#"earliest="a" latest="b" service=x  | stats count() by host"#,
            // A negation gained.
            r#"earliest="a" latest="b" service=x NOT error  | stats count() by host"#,
            // The OR structure changed.
            r#"earliest="a" latest="b" service=x OR error  | stats count() by host"#,
            // A stage changed.
            r#"earliest="a" latest="b" service=x error  | stats count() by service"#,
            // A stage gained.
            r#"earliest="a" latest="b" service=x error  | stats count() by host | head 1"#,
            // Text that no longer parses.
            r#"earliest="a" latest="b" service=x error  | stats count( by host"#,
        ] {
            assert_eq!(
                prove_unchanged(original, corrupted),
                Err(BoundsError::SpliceMismatch),
                "{corrupted}"
            );
        }
    }

    #[test]
    fn the_order_proof_accepts_only_the_imposed_stage() {
        let spliced = r#"earliest="a" latest="b" error"#;
        prove_order_appended(spliced, &format!("{spliced}\n{IMPOSED_ORDER}")).unwrap();
        for wrong in [
            format!("{spliced}\n| sort _time, _ingested"),
            format!("{spliced}\n| sort -_time"),
            format!("{spliced}\n| sort -_ingested, -_time"),
            format!("{spliced} warn\n{IMPOSED_ORDER}"),
            format!("{spliced}\n| head 5\n{IMPOSED_ORDER}"),
            spliced.to_owned(),
        ] {
            assert_eq!(
                prove_order_appended(spliced, &wrong),
                Err(BoundsError::SpliceMismatch),
                "{wrong}"
            );
        }
    }

    #[test]
    fn the_imposed_order_names_the_schema_columns() {
        assert_eq!(IMPOSED_ORDER, format!("| sort -{TIME}, -{INGESTED}"));
    }

    // -- Messages -----------------------------------------------------------

    #[test]
    fn every_refusal_names_its_fix() {
        let cases = [
            (
                BoundsError::Unparseable {
                    message: "found '|'".to_owned(),
                },
                "found '|'",
            ),
            (BoundsError::WindowOnStored, "remove the window"),
            (
                BoundsError::ClauseOnStored {
                    clause: TimeClause::Last,
                },
                "remove the last= clause",
            ),
            (
                BoundsError::WindowAndClause {
                    clause: TimeClause::Earliest,
                },
                "earliest= clause; give the window in one place",
            ),
            (BoundsError::MixedClauses, "last cannot be combined"),
            (BoundsError::LatestAlone, "add earliest, or give last"),
            (
                BoundsError::InvalidClause {
                    clause: TimeClause::Latest,
                    value: "soon".to_owned(),
                },
                "latest=\"soon\" does not name a finite instant",
            ),
            (
                BoundsError::InvalidWindow {
                    field: "last",
                    value: "1y".to_owned(),
                    expected: DURATION_GRAMMAR,
                },
                "window last \"1y\" is not a duration",
            ),
            (
                BoundsError::ClauseTooLong {
                    clause: TimeClause::Earliest,
                    bytes: 129,
                },
                "the earliest= value is 129 bytes; at most 128 are accepted",
            ),
            (
                BoundsError::WindowTooLong {
                    field: "last",
                    bytes: 102_400,
                },
                "window last is 102400 bytes; at most 128 are accepted",
            ),
            (
                BoundsError::InvalidWindow {
                    field: "latest",
                    value: "s".repeat(100),
                    expected: RFC3339_GRAMMAR,
                },
                "… (100 bytes) is not an RFC 3339",
            ),
            (
                BoundsError::EmptyWindow {
                    earliest: "A".to_owned(),
                    latest: "B".to_owned(),
                },
                "earliest A is not before latest B",
            ),
            (
                BoundsError::SpanTooLong {
                    limit: "7d".to_owned(),
                },
                "at most 7d",
            ),
            (
                BoundsError::ClauseNotTopLevel {
                    clause: TimeClause::Last,
                },
                "write it at the top level",
            ),
            (
                BoundsError::SpliceMismatch,
                "top-level last=, earliest= or latest=",
            ),
            (BoundsError::ShapeNotImposable, "eventstats form"),
        ];
        for (error, needle) in cases {
            let message = error.to_string();
            assert!(message.contains(needle), "{error:?}: {message}");
        }
    }

    #[test]
    fn spans_print_in_the_largest_whole_unit() {
        assert_eq!(span_text(Duration::from_hours(7 * 24)), "7d");
        assert_eq!(span_text(Duration::from_secs(7_200)), "2h");
        assert_eq!(span_text(Duration::from_mins(90)), "90m");
        assert_eq!(span_text(Duration::from_secs(61)), "61s");
        assert_eq!(span_text(Duration::from_millis(1_500)), "1.5s");
    }
}
