// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! In-memory event filter compiled from the search stage of a DSL query.
//!
//! `filter.matches_at(event, ctx)` must agree with running the emitted SQL
//! against `DuckDB` for every `(event, search_stage)` pair, under the same
//! `now()` anchor (ADR-0017 §3).
//!
//! Collapsing UNKNOWN to `false` at a field-comparison leaf would invert
//! under `NOT`. Text containment is the deliberate two-valued exception
//! (ADR-0015).

use std::cell::OnceCell;

use serde_json::Value;

use crate::ast::{FieldFilter, FilterOp, FilterValue, Scalar, SearchStage, SearchToken};
use crate::cancel::Cancelled;
use crate::compare::{self, PatternForm};
use crate::context::EvalContext;
use crate::emitter::EmitError;
use crate::pattern::{Pattern, PatternError, PatternKind, Preparer, RegexRefusal};
use crate::pin_match::{
    CoercedValue, CompareOp, NullReadPolicy, Truth, and_all, coerce_form, compare_values, or_any,
    pattern_text,
};
use crate::schema::FieldTypes;
use crate::text::fold_ci;

pub struct CompiledFilter {
    groups: Vec<Vec<TokenMatcher>>,
    time_filter: Option<TimeMatcher>,
    earliest: Option<compare::Instant>,
    latest: Option<compare::Instant>,
}

impl std::fmt::Debug for CompiledFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledFilter")
            .field("groups", &self.groups.len())
            .field("time_filter", &self.time_filter.is_some())
            .field("earliest", &self.earliest)
            .field("latest", &self.latest)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FilterError {
    Emit(EmitError),
    /// The query's regex memory does not fit, in its budget or in the
    /// server's reservation (ADR-0063). A capacity refusal, not a fault in
    /// the query text, so its sentence quotes no pattern.
    RegexResources(RegexRefusal),
    /// The ledger's stop was requested before the filter's patterns built
    /// ([`Preparer::with_stop`]). The work that owned the compile ended,
    /// so this is no verdict on the query.
    Cancelled,
}

impl std::fmt::Display for FilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Emit(e) => e.fmt(f),
            Self::RegexResources(refusal) => PatternError::Resources(*refusal).fmt(f),
            Self::Cancelled => Cancelled.fmt(f),
        }
    }
}

impl std::error::Error for FilterError {}

impl From<compare::CompareError> for FilterError {
    fn from(e: compare::CompareError) -> Self {
        Self::Emit(e.into())
    }
}

struct TimeMatcher {
    duration_secs: u64,
}

enum TokenMatcher {
    Field(FieldMatcher),
    Text(TextMatcher),
    Not(Box<TokenMatcher>),
    OrGroup(Vec<Vec<TokenMatcher>>),
}

struct FieldMatcher {
    field: String,
    predicate: FieldPredicate,
}

enum FieldPredicate {
    Compare {
        op: CompareOp,
        value: CoercedValue,
    },
    Pattern {
        regex: Pattern,
        form: PatternForm,
        negated: bool,
    },
    List {
        elements: Vec<ListElement>,
        form: PatternForm,
        negated: bool,
    },
}

enum ListElement {
    Exact(CoercedValue),
    Pattern(Pattern),
}

struct TextMatcher {
    folded_term: String,
    negated: bool,
}

impl CompiledFilter {
    /// `pins` is the field catalog's full pin snapshot (ADR-0011). Pass an
    /// empty set where no catalog exists (embedded mode, plain unit tests);
    /// every comparison is then literal-driven.
    ///
    /// # Errors
    ///
    /// Returns the same error the SQL emitter raises when a `SEVERITY`-pinned
    /// field is compared against a literal outside the ladder vocabulary.
    /// Also errors when a regex does not compile here. Every glob compiles: a
    /// malformed one (`a[z-a]*`) matches nothing, as `DuckDB`'s GLOB does.
    pub fn compile(search: &SearchStage, pins: &FieldTypes) -> Result<Self, FilterError> {
        Self::compile_with(search, pins, &mut Preparer::new())
    }

    /// [`compile`](Self::compile), building the filter's patterns through
    /// `preparer`. A pattern the ledger already holds, from the filter or
    /// from a plan it prepared, is not built or charged again.
    pub fn compile_with(
        search: &SearchStage,
        pins: &FieldTypes,
        preparer: &mut Preparer,
    ) -> Result<Self, FilterError> {
        let time_filter = search.time_filter.as_ref().map(|tf| TimeMatcher {
            duration_secs: tf.node.duration.to_seconds(),
        });

        let earliest = search.earliest.as_ref().map(|b| b.node.instant());
        let latest = search.latest.as_ref().map(|b| b.node.instant());

        let groups = compile_groups(&search.groups, pins, preparer)?;

        Ok(Self {
            groups,
            time_filter,
            earliest,
            latest,
        })
    }

    /// # Errors
    ///
    /// [`PatternError::Cancelled`] once the stop of the ledger that built
    /// the filter's patterns is requested, at the next pattern it reads
    /// ([`Pattern::is_match`]). That is no verdict on the event.
    pub fn matches_at(
        &self,
        event: &serde_json::Map<String, Value>,
        ctx: &EvalContext,
    ) -> Result<bool, PatternError> {
        if let Some(tf) = &self.time_filter
            && !matches_time_filter_at(event, tf, ctx.now_utc())
        {
            return Ok(false);
        }

        // `Instant`'s order puts both infinities where the SQL comparison does.
        if self.earliest.is_some() || self.latest.is_some() {
            if let Some(ts) = extract_event_timestamp(event) {
                let ts = compare::Instant::At(ts.naive_utc());
                if let Some(earliest) = self.earliest
                    && ts < earliest
                {
                    return Ok(false);
                }
                if let Some(latest) = self.latest
                    && ts >= latest
                {
                    return Ok(false);
                }
            } else {
                return Ok(false);
            }
        }

        if self.groups.is_empty() {
            return Ok(true);
        }

        Ok(eval_groups(&self.groups, &EventView::new(event))? == Some(true))
    }
}

fn compile_groups(
    groups: &[Vec<crate::ast::Spanned<SearchToken>>],
    pins: &FieldTypes,
    preparer: &mut Preparer,
) -> Result<Vec<Vec<TokenMatcher>>, FilterError> {
    groups
        .iter()
        .map(|group| {
            group
                .iter()
                .filter_map(|token| compile_token(&token.node, pins, preparer).transpose())
                .collect()
        })
        .collect()
}

fn compile_token(
    token: &SearchToken,
    pins: &FieldTypes,
    preparer: &mut Preparer,
) -> Result<Option<TokenMatcher>, FilterError> {
    Ok(match token {
        SearchToken::FieldFilter(ff) => compile_field_filter(ff, pins, preparer)?,
        SearchToken::TextSearch(ts) => {
            if ts.term == "*" {
                return Ok(None);
            }
            Some(TokenMatcher::Text(TextMatcher {
                folded_term: fold_ci(&ts.term),
                negated: ts.negated,
            }))
        }
        SearchToken::TimeFilter(_)
        | SearchToken::EarliestFilter(_)
        | SearchToken::LatestFilter(_) => None,
        SearchToken::QuotedSearch(qs) => Some(TokenMatcher::Text(TextMatcher {
            folded_term: fold_ci(&qs.phrase),
            negated: false,
        })),
        SearchToken::Not(inner) => {
            let Some(inner_matcher) = compile_token(&inner.node, pins, preparer)? else {
                return Ok(None);
            };
            Some(TokenMatcher::Not(Box::new(inner_matcher)))
        }
        SearchToken::Group(groups) => {
            let compiled_groups = compile_groups(groups, pins, preparer)?;
            Some(TokenMatcher::OrGroup(compiled_groups))
        }
    })
}

/// `compile_token` recurses once per search bracket and per `NOT`. Inlined,
/// this arm's locals would sit in every level's frame; here they cost one
/// frame, at the leaf.
#[inline(never)]
fn compile_field_filter(
    ff: &FieldFilter,
    pins: &FieldTypes,
    preparer: &mut Preparer,
) -> Result<Option<TokenMatcher>, FilterError> {
    let pin = pins.pin_for(&ff.field);
    let form = compare::pattern_form(pin);
    let predicate = match &ff.value {
        FilterValue::Single(Scalar::Exact(v)) => FieldPredicate::Compare {
            op: compile_op(ff.op),
            value: coerce_form(compare::compare_form(pin, ff.op, v)?),
        },
        FilterValue::Single(pattern) => FieldPredicate::Pattern {
            regex: compile_pattern(pattern, preparer)?,
            form,
            negated: match ff.op {
                FilterOp::Eq => false,
                FilterOp::Ne => true,
                FilterOp::Gt | FilterOp::Gte | FilterOp::Lt | FilterOp::Lte => {
                    unreachable!("the parser refuses an ordered operator with a pattern (ADR-0060)")
                }
            },
        },
        FilterValue::List(values) => {
            FieldPredicate::List {
                elements: values
                    .iter()
                    .map(|v| match v {
                        Scalar::Exact(v) => Ok(ListElement::Exact(coerce_form(
                            compare::compare_form(pin, FilterOp::Eq, v)?,
                        ))),
                        pattern => compile_pattern(pattern, preparer).map(ListElement::Pattern),
                    })
                    .collect::<Result<_, FilterError>>()?,
                form,
                negated: match ff.op {
                    FilterOp::Eq => false,
                    FilterOp::Ne => true,
                    FilterOp::Gt | FilterOp::Gte | FilterOp::Lt | FilterOp::Lte => {
                        unreachable!("ordered operators cannot reach a comma-separated list")
                    }
                },
            }
        }
    };
    Ok(Some(TokenMatcher::Field(FieldMatcher {
        // The event key is the same `catalog_key` the pin lookup used. Without
        // the fold a mixed-case reference would read every event as a NULL
        // column, which `!=` reports as a match.
        field: crate::schema::catalog_key(&ff.field),
        predicate,
    })))
}

/// A syntax error gives no cause: regex-syntax's text spans several lines.
fn compile_pattern(pattern: &Scalar, preparer: &mut Preparer) -> Result<Pattern, FilterError> {
    let (label, kind, text) = match pattern {
        Scalar::Glob(glob) => ("glob", PatternKind::Glob, glob),
        Scalar::Regex(body) => ("regex", PatternKind::Regex, body),
        Scalar::Exact(_) => unreachable!("an exact value binds through compare_form"),
    };
    preparer.build(kind, text).map_err(|error| match error {
        PatternError::Resources(refusal) => FilterError::RegexResources(refusal),
        PatternError::Cancelled => FilterError::Cancelled,
        PatternError::Syntax(_) => FilterError::Emit(EmitError::UnsupportedOperation {
            message: format!("invalid {label} pattern '{text}'"),
        }),
        cause => FilterError::Emit(EmitError::UnsupportedOperation {
            message: format!("invalid {label} pattern '{text}': {cause}"),
        }),
    })
}

fn compile_op(op: FilterOp) -> CompareOp {
    match op {
        FilterOp::Eq => CompareOp::Eq,
        FilterOp::Ne => CompareOp::Ne,
        FilterOp::Gt => CompareOp::Gt,
        FilterOp::Gte => CompareOp::Gte,
        FilterOp::Lt => CompareOp::Lt,
        FilterOp::Lte => CompareOp::Lte,
    }
}

struct EventView<'a> {
    event: &'a serde_json::Map<String, Value>,
    message: OnceCell<Option<String>>,
    raw: OnceCell<Option<String>>,
}

impl<'a> EventView<'a> {
    fn new(event: &'a serde_json::Map<String, Value>) -> Self {
        Self {
            event,
            message: OnceCell::new(),
            raw: OnceCell::new(),
        }
    }

    fn folded_message(&self) -> Option<&str> {
        self.folded(&self.message, "message")
    }

    fn folded_raw(&self) -> Option<&str> {
        self.folded(&self.raw, "_raw")
    }

    fn folded<'s>(&'s self, cell: &'s OnceCell<Option<String>>, key: &str) -> Option<&'s str> {
        cell.get_or_init(|| match self.event.get(key) {
            Some(Value::String(s)) => Some(fold_ci(s)),
            _ => None,
        })
        .as_deref()
    }
}

fn eval_groups(groups: &[Vec<TokenMatcher>], event: &EventView<'_>) -> Result<Truth, PatternError> {
    fallible(
        groups
            .iter()
            .map(|group| fallible(group.iter().map(|m| m.eval(event)), |terms| and_all(terms))),
        |groups| or_any(groups),
    )
}

/// `fold` (`and_all` or `or_any`) over terms that may fail: it reads them
/// in order, short-circuiting as it always does, and the first failure it
/// reads ends the fold with that failure.
fn fallible(
    terms: impl IntoIterator<Item = Result<Truth, PatternError>>,
    fold: impl FnOnce(&mut dyn Iterator<Item = Truth>) -> Truth,
) -> Result<Truth, PatternError> {
    let mut failed = None;
    let mut read = terms
        .into_iter()
        .map_while(|term| term.map_err(|e| failed = Some(e)).ok());
    let truth = fold(&mut read);
    drop(read);
    failed.map_or(Ok(truth), Err)
}

impl TokenMatcher {
    fn eval(&self, event: &EventView<'_>) -> Result<Truth, PatternError> {
        Ok(match self {
            Self::Field(fm) => fm.eval(event.event)?,
            Self::Text(tm) => Some(tm.eval(event)),
            Self::Not(inner) => inner.eval(event)?.map(|b| !b),
            Self::OrGroup(groups) => eval_groups(groups, event)?,
        })
    }
}

impl FieldMatcher {
    fn eval(&self, event: &serde_json::Map<String, Value>) -> Result<Truth, PatternError> {
        let event_val = event.get(&self.field).filter(|v| !v.is_null());
        let Some(event_val) = event_val else {
            // SQL three-valued logic: a missing field is a NULL column and
            // a JSON null is a NULL value, so every comparison is UNKNOWN
            // — except `!=`, whose emitted form carries `OR col IS NULL`
            // and is therefore TRUE. Decided here, before coercion, so
            // every coercion class agrees with the SQL on nulls
            // (stringifying null to "" diverges on ordered comparisons).
            // A negated pattern or list is TRUE for the same reason: its
            // SQL is `NOT COALESCE(…, FALSE)` and the `!=` widening.
            return Ok(match &self.predicate {
                FieldPredicate::Compare {
                    op: CompareOp::Ne, ..
                }
                | FieldPredicate::Pattern { negated: true, .. }
                | FieldPredicate::List { negated: true, .. } => Some(true),
                _ => None,
            });
        };

        match &self.predicate {
            FieldPredicate::Compare { op, value } => Ok(compare_values(
                event_val,
                *op,
                value,
                NullReadPolicy::NeMatches,
            )),
            FieldPredicate::Pattern {
                regex,
                form,
                negated,
            } => pattern_truth(regex, pattern_text(event_val, *form).as_deref(), *negated),
            FieldPredicate::List {
                elements,
                form,
                negated,
            } => {
                let text = OnceCell::new();
                let op = if *negated {
                    CompareOp::Ne
                } else {
                    CompareOp::Eq
                };
                let terms = elements.iter().map(|element| match element {
                    ListElement::Exact(v) => {
                        Ok(compare_values(event_val, op, v, NullReadPolicy::NeMatches))
                    }
                    ListElement::Pattern(regex) => pattern_truth(
                        regex,
                        text.get_or_init(|| pattern_text(event_val, *form))
                            .as_deref(),
                        *negated,
                    ),
                });
                if *negated {
                    fallible(terms, |terms| and_all(terms))
                } else {
                    fallible(terms, |terms| or_any(terms))
                }
            }
        }
    }
}

/// No canonical text (a TIMESTAMP pin over a value with no timestamp
/// reading) is a NULL column in batch, and `strftime(NULL, …) GLOB p` is
/// NULL: UNKNOWN under `=`, not FALSE. Under `!=` the SQL is
/// `NOT COALESCE(…, FALSE)`, so the same value is TRUE (ADR-0060).
fn pattern_truth(
    regex: &Pattern,
    text: Option<&str>,
    negated: bool,
) -> Result<Truth, PatternError> {
    let matched = text.map(|s| regex.is_match(s)).transpose()?;
    Ok(if negated {
        Some(matched != Some(true))
    } else {
        matched
    })
}

impl TextMatcher {
    /// Containment folds case on both sides with [`fold_ci`], the
    /// `DuckDB` lowercase table `ILIKE` applies, so `é` finds `É` and `i`
    /// finds `İ` in both lanes.
    fn eval(&self, event: &EventView<'_>) -> bool {
        let hit = |text: Option<&str>| text.is_some_and(|t| t.contains(&self.folded_term));
        let msg = hit(event.folded_message());
        // `||` keeps `_raw` unfolded when `message` already matched.
        self.negated != (msg || hit(event.folded_raw()))
    }
}

fn extract_event_timestamp(
    event: &serde_json::Map<String, Value>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let ts_val = event.get("_time")?;
    match ts_val {
        Value::String(s) => parse_timestamp(s),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                parse_epoch_i64(i)
            } else if let Some(f) = n.as_f64() {
                parse_epoch_f64(f)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn matches_time_filter_at(
    event: &serde_json::Map<String, Value>,
    tf: &TimeMatcher,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let Some(ts_val) = event.get("_time") else {
        return false;
    };

    let event_time = match ts_val {
        Value::String(s) => parse_timestamp(s),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                parse_epoch_i64(i)
            } else if let Some(f) = n.as_f64() {
                parse_epoch_f64(f)
            } else {
                None
            }
        }
        _ => None,
    };

    let Some(event_time) = event_time else {
        return false;
    };

    #[allow(clippy::cast_possible_wrap)]
    let cutoff = now - chrono::Duration::seconds(tf.duration_secs as i64);
    event_time >= cutoff
}

fn parse_timestamp(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    if let Ok(dt) = chrono::DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%:z") {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(naive.and_utc());
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(naive.and_utc());
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Some(naive.and_utc());
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f") {
        return Some(naive.and_utc());
    }
    if let Ok(n) = s.parse::<i64>() {
        return parse_epoch_i64(n);
    }
    if let Ok(f) = s.parse::<f64>() {
        return parse_epoch_f64(f);
    }
    None
}

fn parse_epoch_i64(val: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    if val.abs() < 1_000_000_000_000 {
        chrono::DateTime::from_timestamp(val, 0)
    } else {
        let secs = val / 1000;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ns = ((val % 1000).unsigned_abs() as u32) * 1_000_000;
        chrono::DateTime::from_timestamp(secs, ns)
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn parse_epoch_f64(val: f64) -> Option<chrono::DateTime<chrono::Utc>> {
    let secs = val as i64;
    let nanos = ((val - secs as f64).abs() * 1e9) as u32;
    chrono::DateTime::from_timestamp(secs, nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    fn fixed_now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-08-24T12:00:00Z")
            .expect("literal is RFC 3339")
            .with_timezone(&chrono::Utc)
    }

    fn matches_event(dsl: &str, event_json: &str) -> bool {
        let query = parser::parse(dsl).expect("parse should succeed");
        let filter = CompiledFilter::compile(&query.search, &crate::schema::FieldTypes::new())
            .expect("filter compiles");
        let event: serde_json::Map<String, Value> =
            serde_json::from_str(event_json).expect("valid JSON object");
        filter
            .matches_at(&event, &EvalContext::at(fixed_now()))
            .expect("a never stop never cancels")
    }

    fn matches_event_pinned(
        dsl: &str,
        event_json: &str,
        pins: &[(&str, crate::schema::CanonicalType)],
    ) -> bool {
        let mut ft = crate::schema::FieldTypes::new();
        for (field, ty) in pins {
            ft.insert(field, *ty);
        }
        let query = parser::parse(dsl).expect("parse should succeed");
        let filter = CompiledFilter::compile(&query.search, &ft).expect("filter compiles");
        let event: serde_json::Map<String, Value> =
            serde_json::from_str(event_json).expect("valid JSON object");
        filter
            .matches_at(&event, &EvalContext::at(fixed_now()))
            .expect("a never stop never cancels")
    }

    fn matches_event_at(dsl: &str, event_json: &str, now: chrono::DateTime<chrono::Utc>) -> bool {
        let query = parser::parse(dsl).expect("parse should succeed");
        let filter = CompiledFilter::compile(&query.search, &crate::schema::FieldTypes::new())
            .expect("filter compiles");
        let event: serde_json::Map<String, Value> =
            serde_json::from_str(event_json).expect("valid JSON object");
        filter
            .matches_at(&event, &EvalContext::at(now))
            .expect("a never stop never cancels")
    }

    #[test]
    fn field_eq_string() {
        assert!(matches_event(
            "service=nginx",
            r#"{"service": "nginx", "message": "ok"}"#
        ));
        assert!(!matches_event(
            "service=nginx",
            r#"{"service": "apache", "message": "ok"}"#
        ));
    }

    #[test]
    fn field_eq_numeric() {
        assert!(matches_event(
            "status=200",
            r#"{"status": 200, "message": "ok"}"#
        ));
        assert!(matches_event(
            "status=200",
            r#"{"status": "200", "message": "ok"}"#
        ));
        assert!(!matches_event(
            "status=200",
            r#"{"status": 404, "message": "ok"}"#
        ));
    }

    #[test]
    fn field_ne() {
        assert!(matches_event(
            "status!=200",
            r#"{"status": 404, "message": "ok"}"#
        ));
        assert!(!matches_event(
            "status!=200",
            r#"{"status": 200, "message": "ok"}"#
        ));
    }

    #[test]
    fn field_gt() {
        assert!(matches_event(
            "status>400",
            r#"{"status": 500, "message": "ok"}"#
        ));
        assert!(!matches_event(
            "status>400",
            r#"{"status": 200, "message": "ok"}"#
        ));
    }

    #[test]
    fn field_gte() {
        assert!(matches_event(
            "status>=400",
            r#"{"status": 400, "message": "ok"}"#
        ));
        assert!(matches_event(
            "status>=400",
            r#"{"status": 500, "message": "ok"}"#
        ));
    }

    #[test]
    fn field_lt() {
        assert!(matches_event(
            "status<300",
            r#"{"status": 200, "message": "ok"}"#
        ));
        assert!(!matches_event(
            "status<300",
            r#"{"status": 500, "message": "ok"}"#
        ));
    }

    #[test]
    fn field_lte() {
        assert!(matches_event(
            "status<=300",
            r#"{"status": 300, "message": "ok"}"#
        ));
    }

    #[test]
    fn field_in_list() {
        assert!(matches_event(
            "status=200,301,404",
            r#"{"status": 301, "message": "ok"}"#
        ));
        assert!(!matches_event(
            "status=200,301,404",
            r#"{"status": 500, "message": "ok"}"#
        ));
    }

    #[test]
    fn field_glob() {
        assert!(matches_event(
            "path=/api/*",
            r#"{"path": "/api/users", "message": "ok"}"#
        ));
        assert!(!matches_event(
            "path=/api/*",
            r#"{"path": "/web/index", "message": "ok"}"#
        ));
    }

    #[test]
    fn field_glob_question_mark() {
        assert!(matches_event(
            "host=web-?",
            r#"{"host": "web-1", "message": "ok"}"#
        ));
        assert!(!matches_event(
            "host=web-?",
            r#"{"host": "web-10", "message": "ok"}"#
        ));
    }

    #[test]
    fn field_glob_with_character_class() {
        // `[` alone doesn't trigger glob auto-detection, so include `*`.
        assert!(matches_event(
            "host=web-[123]*",
            r#"{"host": "web-2-prod", "message": "ok"}"#
        ));
        assert!(!matches_event(
            "host=web-[123]*",
            r#"{"host": "web-5-prod", "message": "ok"}"#
        ));
    }

    #[test]
    fn a_malformed_glob_matches_nothing() {
        let carried = [
            r#"{"host": "a", "message": "ok"}"#,
            r#"{"host": "az", "message": "ok"}"#,
            r#"{"host": "a[z-a]", "message": "ok"}"#,
        ];
        let absent = r#"{"message": "ok"}"#;
        for event in carried.into_iter().chain([absent]) {
            assert!(!matches_event("host=a[z-a]*", event), "{event}");
            assert!(matches_event("host!=a[z-a]*", event), "{event}");
        }
        for event in carried {
            assert!(matches_event("NOT host=a[z-a]*", event), "{event}");
        }
        assert!(!matches_event("NOT host=a[z-a]*", absent));
        assert!(matches_event("host=x,a[z-a]*", r#"{"host": "x"}"#));
        assert!(!matches_event("host=x,a[z-a]*", r#"{"host": "a"}"#));
    }

    fn live_regex_refusal(body: &str) -> String {
        let mut query = parser::parse("msg=/x/").expect("parses");
        let crate::ast::SearchToken::FieldFilter(filter) = &mut query.search.groups[0][0].node
        else {
            panic!("one field filter");
        };
        filter.value = crate::ast::FilterValue::Single(Scalar::Regex(body.to_string()));
        CompiledFilter::compile(&query.search, &crate::schema::FieldTypes::new())
            .expect_err(body)
            .to_string()
    }

    #[test]
    fn the_live_lane_shares_the_parsers_regex_size_limit() {
        assert!(matches_event(r"msg=/\w{10}/", r#"{"msg": "abcdefghij"}"#));

        let too_large = r"\w{64}";
        assert!(parser::parse(&format!("msg=/{too_large}/")).is_err());
        assert_eq!(
            live_regex_refusal(too_large),
            format!(
                "unsupported operation: invalid regex pattern '{too_large}': {}",
                crate::pattern::MSG_REGEX_TOO_LARGE
            )
        );
    }

    #[test]
    fn a_live_regex_refusal_carries_its_own_message() {
        for (body, cause) in [
            ("[a[b]]", crate::pattern::MSG_CLASS_SET_SYNTAX),
            ("[a&&b]", crate::pattern::MSG_CLASS_SET_SYNTAX),
            (r"\B(?:\pL?){15}\PL", crate::pattern::MSG_BATCH_TOO_LARGE),
        ] {
            let parsed = parser::parse(&format!("msg=/{body}/")).expect_err(body);
            assert_eq!(parsed[0].message, cause, "{body}");
            assert_eq!(
                live_regex_refusal(body),
                format!("unsupported operation: invalid regex pattern '{body}': {cause}"),
                "{body}"
            );
        }
    }

    #[test]
    fn a_glob_matches_across_newlines() {
        let event = r#"{"message": "a\nb"}"#;
        assert!(matches_event("message=*", event));
        assert!(matches_event("message=a?b", event));
        assert!(!matches_event("message!=a*", event));
    }

    #[test]
    fn field_regex() {
        assert!(matches_event(
            r"host=/web-\d+/",
            r#"{"host": "web-42", "message": "ok"}"#
        ));
        assert!(!matches_event(
            r"host=/web-\d+/",
            r#"{"host": "db-01", "message": "ok"}"#
        ));
    }

    #[test]
    fn missing_field_no_match() {
        assert!(!matches_event(
            "service=nginx",
            r#"{"host": "web-1", "message": "ok"}"#
        ));
    }

    use crate::schema::CanonicalType as CT;

    const VARCHAR_STATUS: &[(&str, CT)] = &[("status", CT::Varchar)];
    const BIGINT_STATUS: &[(&str, CT)] = &[("status", CT::BigInt)];

    #[test]
    fn pinned_varchar_eq_numeric_compares_as_text_or_reading() {
        assert!(matches_event_pinned(
            "status=200",
            r#"{"status": "200"}"#,
            VARCHAR_STATUS
        ));
        // A wire number matches on the numeric arm, which is the arm that
        // survives `read_json` widening the column: the same event stores
        // "200" beside integers and "200.0" beside a fractional sibling,
        // and batch answers TRUE either way (executed in
        // `tests/filter_parity.rs`).
        assert!(matches_event_pinned(
            "status=200",
            r#"{"status": 200}"#,
            VARCHAR_STATUS
        ));
        assert!(matches_event_pinned(
            "status=200",
            r#"{"status": 200.0}"#,
            VARCHAR_STATUS
        ));
        assert!(matches_event_pinned(
            "status=200",
            r#"{"status": "0200"}"#,
            VARCHAR_STATUS
        ));
        assert!(!matches_event_pinned(
            "status=200",
            r#"{"status": "accepted"}"#,
            VARCHAR_STATUS
        ));
        assert!(!matches_event_pinned(
            "status=200",
            r#"{"status": "404"}"#,
            VARCHAR_STATUS
        ));
        assert!(matches_event_pinned(
            "status=accepted",
            r#"{"status": "accepted"}"#,
            VARCHAR_STATUS
        ));
    }

    #[test]
    fn pinned_varchar_ne_numeric_keeps_unreadable_values() {
        for value in [r#""accepted""#, r#""n/a""#, r#""404""#, "404"] {
            let event = format!(r#"{{"status": {value}}}"#);
            assert!(
                matches_event_pinned("status!=200", &event, VARCHAR_STATUS),
                "status!=200 must match {value}"
            );
        }
        for value in [r#""200""#, "200", "200.0"] {
            let event = format!(r#"{{"status": {value}}}"#);
            assert!(
                !matches_event_pinned("status!=200", &event, VARCHAR_STATUS),
                "status!=200 must not match {value}"
            );
        }
    }

    #[test]
    fn pinned_varchar_ne_numeric_includes_json_null() {
        assert!(matches_event_pinned(
            "status!=200",
            r#"{"status": "404"}"#,
            VARCHAR_STATUS
        ));
        assert!(!matches_event_pinned(
            "status!=200",
            r#"{"status": "200"}"#,
            VARCHAR_STATUS
        ));
        assert!(matches_event_pinned(
            "status!=200",
            r#"{"status": null}"#,
            VARCHAR_STATUS
        ));
    }

    #[test]
    fn absent_field_matches_ne_like_explicit_null() {
        for dsl in ["status!=200", "status!=accepted"] {
            for event in [r#"{"status": null}"#, "{}"] {
                assert!(
                    matches_event_pinned(dsl, event, VARCHAR_STATUS),
                    "{dsl} over {event} must match under a VARCHAR pin"
                );
                assert!(
                    matches_event(dsl, event),
                    "{dsl} over {event} must match unpinned"
                );
            }
        }
    }

    #[test]
    fn pinned_varchar_in_list_compares_as_text() {
        assert!(matches_event_pinned(
            "status=200,301",
            r#"{"status": "301"}"#,
            VARCHAR_STATUS
        ));
        assert!(!matches_event_pinned(
            "status=200,301",
            r#"{"status": "404"}"#,
            VARCHAR_STATUS
        ));
        assert!(!matches_event_pinned(
            "status=200,301",
            r#"{"status": "accepted"}"#,
            VARCHAR_STATUS
        ));
    }

    #[test]
    fn pinned_varchar_ordered_numeric_matches_numeric_text() {
        assert!(matches_event_pinned(
            "status>=400",
            r#"{"status": "404"}"#,
            VARCHAR_STATUS
        ));
        assert!(matches_event_pinned(
            "status>=400",
            r#"{"status": "500"}"#,
            VARCHAR_STATUS
        ));
        assert!(!matches_event_pinned(
            "status>=400",
            r#"{"status": "200"}"#,
            VARCHAR_STATUS
        ));
        assert!(!matches_event_pinned(
            "status>=400",
            r#"{"status": "accepted"}"#,
            VARCHAR_STATUS
        ));
        assert!(!matches_event_pinned(
            "status>=400",
            r#"{"status": null}"#,
            VARCHAR_STATUS
        ));
        assert!(matches_event_pinned(
            "dur>1",
            r#"{"dur": "1.5"}"#,
            &[("dur", CT::Varchar)]
        ));
        assert!(matches_event_pinned(
            "dur<2",
            r#"{"dur": "1.5"}"#,
            &[("dur", CT::Varchar)]
        ));
    }

    #[test]
    fn pinned_varchar_ordered_lexical_stays_lexical() {
        assert!(matches_event_pinned(
            "host>alpha",
            r#"{"host": "beta"}"#,
            &[("host", CT::Varchar)]
        ));
        assert!(!matches_event_pinned(
            "host>alpha",
            r#"{"host": "aleph"}"#,
            &[("host", CT::Varchar)]
        ));
    }

    #[test]
    fn pinned_bigint_glob_and_regex_match_text_form() {
        assert!(matches_event_pinned(
            "status=4*",
            r#"{"status": 404}"#,
            BIGINT_STATUS
        ));
        assert!(!matches_event_pinned(
            "status=4*",
            r#"{"status": 200}"#,
            BIGINT_STATUS
        ));
        assert!(matches_event_pinned(
            "status=/40./",
            r#"{"status": 404}"#,
            BIGINT_STATUS
        ));
    }

    #[test]
    fn pinned_lookup_is_case_folded() {
        assert!(matches_event_pinned(
            "Status=200",
            r#"{"status": "200"}"#,
            VARCHAR_STATUS
        ));
        assert!(!matches_event_pinned(
            "Status!=200",
            r#"{"status": "200"}"#,
            VARCHAR_STATUS
        ));
        // The pin lookup: "accepted" has no numeric reading, so only the
        // VARCHAR pin's text arm can answer `!=` at all — a fold miss
        // falls back to the unpinned Int coercion, which answers FALSE.
        assert!(matches_event_pinned(
            "Status!=200",
            r#"{"status": "accepted"}"#,
            VARCHAR_STATUS
        ));
        assert!(!matches_event("status!=200", r#"{"status": "accepted"}"#));
        assert!(matches_event_pinned(
            "Status!=200",
            r#"{"status": null}"#,
            VARCHAR_STATUS
        ));
    }

    #[test]
    fn pinned_not_over_unknown_is_never_a_match() {
        assert!(!matches_event_pinned(
            "NOT status>=400",
            r#"{"status": "accepted"}"#,
            VARCHAR_STATUS
        ));
        for dsl in [
            "NOT status>=400",
            "NOT status=200",
            "NOT status=200,301",
            "NOT status=2*",
            "NOT status=/2.*/",
            "NOT status>accepted",
        ] {
            assert!(
                !matches_event_pinned(dsl, r#"{"status": null}"#, VARCHAR_STATUS),
                "{dsl} over a NULL column must stay UNKNOWN"
            );
            assert!(
                !matches_event_pinned(dsl, "{}", VARCHAR_STATUS),
                "{dsl} over an absent field must stay UNKNOWN"
            );
        }
        assert!(!matches_event_pinned(
            "NOT status!=200",
            r#"{"status": null}"#,
            VARCHAR_STATUS
        ));
        assert!(matches_event_pinned(
            "NOT status>=400",
            r#"{"status": "200"}"#,
            VARCHAR_STATUS
        ));
    }

    #[test]
    fn unpinned_not_over_null_column_is_not_a_match() {
        assert!(!matches_event("NOT status=200", r#"{"status": null}"#));
        assert!(!matches_event("NOT status=200", r#"{"message": "hi"}"#));
        assert!(matches_event("NOT status=200", r#"{"status": 404}"#));
    }

    #[test]
    fn text_search_positive() {
        assert!(matches_event(
            "error",
            r#"{"message": "Connection error occurred"}"#
        ));
        assert!(!matches_event(
            "error",
            r#"{"message": "All systems nominal"}"#
        ));
    }

    #[test]
    fn text_search_case_insensitive() {
        assert!(matches_event("error", r#"{"message": "HTTP ERROR 500"}"#));
    }

    #[test]
    fn text_search_negated() {
        assert!(matches_event("-debug", r#"{"message": "error occurred"}"#));
        assert!(!matches_event(
            "-debug",
            r#"{"message": "debug: starting up"}"#
        ));
    }

    #[test]
    fn text_search_covers_the_whole_event_through_raw() {
        let event = r#"{"message": "started", "service": "nginx", "debug_mode": false,
             "_raw": "{\"service\":\"nginx\",\"debug_mode\":false,\"message\":\"started\"}"}"#;
        assert!(matches_event("nginx", event));
        assert!(matches_event("debug", event));
        assert!(!matches_event("-debug", event));
        assert!(matches_event("-absent", event));
    }

    #[test]
    fn text_search_missing_message() {
        assert!(!matches_event("error", r#"{"service": "nginx"}"#));
        assert!(matches_event("-debug", r#"{"service": "nginx"}"#));
        assert!(matches_event("NOT debug", r#"{"service": "nginx"}"#));
    }

    #[test]
    fn quoted_search() {
        assert!(matches_event(
            r#""connection refused""#,
            r#"{"message": "connection refused by remote host"}"#
        ));
        assert!(!matches_event(
            r#""connection refused""#,
            r#"{"message": "connection accepted"}"#
        ));
    }

    #[test]
    fn quoted_search_case_insensitive() {
        assert!(matches_event(
            r#""Connection Refused""#,
            r#"{"message": "connection refused"}"#
        ));
    }

    #[test]
    fn wildcard_matches_everything() {
        assert!(matches_event("*", r#"{"service": "nginx"}"#));
    }

    #[test]
    fn or_groups() {
        assert!(matches_event(
            "service=nginx OR service=apache",
            r#"{"service": "nginx", "message": "ok"}"#
        ));
        assert!(matches_event(
            "service=nginx OR service=apache",
            r#"{"service": "apache", "message": "ok"}"#
        ));
        assert!(!matches_event(
            "service=nginx OR service=apache",
            r#"{"service": "postgres", "message": "ok"}"#
        ));
    }

    #[test]
    fn or_groups_with_and() {
        let dsl = "service=nginx status=500 OR service=apache status=404";
        assert!(matches_event(
            dsl,
            r#"{"service": "nginx", "status": 500, "message": "ok"}"#
        ));
        assert!(matches_event(
            dsl,
            r#"{"service": "apache", "status": 404, "message": "ok"}"#
        ));
        assert!(!matches_event(
            dsl,
            r#"{"service": "nginx", "status": 404, "message": "ok"}"#
        ));
    }

    #[test]
    fn empty_search_matches_all() {
        assert!(matches_event("", r#"{"service": "nginx"}"#));
    }

    #[test]
    fn level_is_an_ordinary_field() {
        for dsl in [
            "level=eror",
            "level!=eror",
            "level>=eror",
            "level=error,eror",
            "level=err*",
            "level=/err.*/",
        ] {
            let query = parser::parse(dsl).expect("parse should succeed");
            crate::emitter::emit(&query, "/data/**/*.parquet", EvalContext::at(fixed_now()))
                .expect("emits");
            CompiledFilter::compile(&query.search, &crate::schema::FieldTypes::new())
                .expect("compiles");
        }
        assert!(matches_event("level=gold", r#"{"level": "gold"}"#));
        assert!(!matches_event("level=gold", r#"{"severity": 17}"#));
    }

    #[test]
    fn severity_pin_carries_the_band_vocabulary() {
        let mut ft = crate::schema::FieldTypes::new();
        ft.insert("_severity", crate::schema::CanonicalType::Severity);
        let matches = |dsl: &str, json: &str| {
            let query = parser::parse(dsl).expect("parse should succeed");
            let filter = CompiledFilter::compile(&query.search, &ft).expect("compiles");
            let ev: serde_json::Map<String, Value> = serde_json::from_str(json).unwrap();
            filter
                .matches_at(&ev, &EvalContext::at(fixed_now()))
                .expect("a never stop never cancels")
        };
        assert!(matches("_severity=error", r#"{"_severity": 17}"#));
        assert!(matches("_severity=error,fatal", r#"{"_severity": 21}"#));
        assert!(!matches("_severity=error", r#"{"_severity": 9}"#));
        assert!(matches("_severity=error2", r#"{"_severity": 18}"#));
        assert!(!matches("_severity=error2", r#"{"_severity": 17}"#));

        let query = parser::parse("_severity=spicy").expect("parse should succeed");
        let emit_error = crate::emitter::emit_with_pins(
            &query,
            "/data/**/*.parquet",
            &ft,
            &crate::emitter::InputColumns::unprobed(),
            EvalContext::at(fixed_now()),
        )
        .expect_err("emit refuses")
        .to_string();
        let compile_error = CompiledFilter::compile(&query.search, &ft)
            .expect_err("filter refuses")
            .to_string();
        assert_eq!(compile_error, emit_error);
    }

    #[test]
    fn null_field_no_match() {
        assert!(!matches_event(
            "service=nginx",
            r#"{"service": null, "message": "ok"}"#
        ));
    }

    #[test]
    fn parse_timestamp_rfc3339() {
        let ts = parse_timestamp("2026-02-15T12:00:00Z");
        assert!(ts.is_some());
    }

    #[test]
    fn parse_timestamp_rfc3339_fractional() {
        let ts = parse_timestamp("2026-02-15T12:00:00.123456Z");
        assert!(ts.is_some());
    }

    #[test]
    fn parse_timestamp_iso_with_offset() {
        let ts = parse_timestamp("2026-02-15T12:00:00+05:30");
        assert!(ts.is_some());
    }

    #[test]
    fn parse_timestamp_naive_t_separator() {
        let ts = parse_timestamp("2026-02-15T12:00:00");
        assert!(ts.is_some());
    }

    #[test]
    fn parse_timestamp_naive_fractional() {
        let ts = parse_timestamp("2026-02-15T12:00:00.5");
        assert!(ts.is_some());
    }

    #[test]
    fn parse_timestamp_space_separator() {
        let ts = parse_timestamp("2026-02-15 12:00:00");
        assert!(ts.is_some());
    }

    #[test]
    fn parse_timestamp_space_fractional() {
        let ts = parse_timestamp("2026-02-15 12:00:00.999");
        assert!(ts.is_some());
    }

    #[test]
    fn parse_timestamp_epoch_seconds_string() {
        let ts = parse_timestamp("1739620800");
        assert!(ts.is_some());
    }

    #[test]
    fn parse_timestamp_epoch_millis_string() {
        let ts = parse_timestamp("1739620800000");
        assert!(ts.is_some());
    }

    #[test]
    fn parse_epoch_i64_seconds() {
        let ts = parse_epoch_i64(1_739_620_800);
        assert!(ts.is_some());
    }

    #[test]
    fn parse_epoch_i64_millis() {
        let ts = parse_epoch_i64(1_739_620_800_000);
        assert!(ts.is_some());
        let secs_ts = parse_epoch_i64(1_739_620_800).unwrap();
        assert_eq!(ts.unwrap().timestamp(), secs_ts.timestamp());
    }

    #[test]
    fn parse_epoch_f64_fractional() {
        let ts = parse_epoch_f64(1_739_620_800.5);
        assert!(ts.is_some());
        let dt = ts.unwrap();
        assert_eq!(dt.timestamp(), 1_739_620_800);
        assert!(dt.timestamp_subsec_nanos() > 0);
    }

    #[test]
    fn parse_timestamp_rejects_malformed() {
        assert!(parse_timestamp("not-a-date").is_none());
        assert!(parse_timestamp("").is_none());
    }

    fn event_with_timestamp(ts: &str) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        m.insert("_time".into(), Value::String(ts.to_string()));
        m.insert("message".into(), Value::String("test".into()));
        m
    }

    fn event_with_epoch_secs(secs: i64) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        m.insert(
            "_time".into(),
            Value::Number(serde_json::Number::from(secs)),
        );
        m.insert("message".into(), Value::String("test".into()));
        m
    }

    #[test]
    fn time_filter_recent_event_matches() {
        let now = fixed_now();
        let recent = (now - chrono::Duration::seconds(1)).to_rfc3339();
        let event = event_with_timestamp(&recent);
        assert!(matches_event_at(
            "last=1h",
            &serde_json::to_string(&event).unwrap(),
            now
        ));
    }

    #[test]
    fn time_filter_old_event_excluded() {
        let now = fixed_now();
        let old = (now - chrono::Duration::hours(2)).to_rfc3339();
        let event = event_with_timestamp(&old);
        assert!(!matches_event_at(
            "last=1h",
            &serde_json::to_string(&event).unwrap(),
            now
        ));
    }

    #[test]
    fn time_filter_boundary_matches() {
        let now = fixed_now();
        let boundary = (now - chrono::Duration::seconds(3600)).to_rfc3339();
        let event = event_with_timestamp(&boundary);
        assert!(matches_event_at(
            "last=1h",
            &serde_json::to_string(&event).unwrap(),
            now
        ));
    }

    #[test]
    fn time_filter_just_past_boundary_excluded() {
        let now = fixed_now();
        let past = (now - chrono::Duration::seconds(3601)).to_rfc3339();
        let event = event_with_timestamp(&past);
        assert!(!matches_event_at(
            "last=1h",
            &serde_json::to_string(&event).unwrap(),
            now
        ));
    }

    #[test]
    fn time_filter_epoch_seconds_event() {
        let now = fixed_now();

        let event = event_with_epoch_secs(now.timestamp() - 1);
        let json = serde_json::to_string(&event).unwrap();
        assert!(matches_event_at("last=1h", &json, now));

        let event = event_with_epoch_secs(now.timestamp() - 7200);
        let json = serde_json::to_string(&event).unwrap();
        assert!(!matches_event_at("last=1h", &json, now));
    }

    #[test]
    fn time_filter_no_timestamp_excluded() {
        let mut event = serde_json::Map::new();
        event.insert("message".into(), Value::String("test".into()));
        let json = serde_json::to_string(&event).unwrap();
        assert!(!matches_event("last=1h", &json));
    }

    #[test]
    fn a_cancelled_compile_is_not_an_invalid_pattern() {
        let query = parser::parse("msg=/time(out)?/ host=web-*").expect("parses");
        let stop = crate::cancel::Stop::new();
        stop.request();
        let mut preparer = crate::pattern::table::test_ledger().with_stop(stop);
        let refused = CompiledFilter::compile_with(
            &query.search,
            &crate::schema::FieldTypes::new(),
            &mut preparer,
        )
        .err();
        assert_eq!(refused, Some(FilterError::Cancelled));
    }

    #[test]
    fn matches_at_uses_the_contexts_instant() {
        let query = parser::parse("last=1h").expect("parse should succeed");
        let filter = CompiledFilter::compile(&query.search, &crate::schema::FieldTypes::new())
            .expect("filter compiles");

        let now = fixed_now();
        let event_ts = (now - chrono::Duration::minutes(30)).to_rfc3339();
        let event = event_with_timestamp(&event_ts);

        assert!(
            filter
                .matches_at(&event, &EvalContext::at(now))
                .expect("a never stop never cancels")
        );

        // Under an instant 3 hours earlier the event is in the future,
        // which the window admits.
        let old_now = now - chrono::Duration::hours(3);
        assert!(
            filter
                .matches_at(&event, &EvalContext::at(old_now))
                .expect("a never stop never cancels")
        );

        let future_now = now + chrono::Duration::hours(2);
        assert!(
            !filter
                .matches_at(&event, &EvalContext::at(future_now))
                .expect("a never stop never cancels")
        );
    }

    #[test]
    fn absolute_bounds_are_half_open_at_the_microsecond() {
        let before = r#"{"_time": "2026-03-14T03:00:00.123455Z"}"#;
        let at = r#"{"_time": "2026-03-14T03:00:00.123456Z"}"#;
        let after = r#"{"_time": "2026-03-14T03:00:00.123457Z"}"#;

        let earliest = r#"earliest="2026-03-14T03:00:00.123456Z""#;
        assert!(!matches_event(earliest, before));
        assert!(matches_event(earliest, at), "earliest= is inclusive");
        assert!(matches_event(earliest, after));

        let latest = r#"latest="2026-03-14T03:00:00.123456Z""#;
        assert!(matches_event(latest, before));
        assert!(!matches_event(latest, at), "latest= is exclusive");
        assert!(!matches_event(latest, after));

        let window = concat!(
            r#"earliest="2026-03-14T03:00:00.123456Z" "#,
            r#"latest="2026-03-14T03:00:00.123457Z""#
        );
        assert!(!matches_event(window, before));
        assert!(matches_event(window, at));
        assert!(!matches_event(window, after));
    }
}
