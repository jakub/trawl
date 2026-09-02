// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! In-memory event filter compiled from the search stage of a DSL query.
//!
//! [`CompiledFilter`] provides semantically equivalent matching to the SQL
//! WHERE clauses emitted by the search stage emitter. Used for real-time
//! event filtering in the SSE streaming endpoint.
//!
//! Key invariant: `filter.matches_at(event, ctx)` must agree with
//! running the emitted SQL against `DuckDB` for every
//! `(event, search_stage)` pair — under the same `now()` anchor, since
//! `last=` is clock-relative and both lanes read the instant the caller
//! hands them (ADR-0017 §3).
//!
//! Evaluation is therefore three-valued, like the SQL it mirrors: field
//! matchers answer [`Truth`] (`Some(true)`/`Some(false)`/`None` for UNKNOWN),
//! UNKNOWN propagates through NOT/AND/OR by SQL's rules, and only a final
//! `Some(true)` is a match. Text containment is the deliberate two-valued
//! exception (ADR-0015): a missing `message`/`_raw` does not contain a term.
//! Collapsing UNKNOWN to `false` at a field-comparison leaf would survive a
//! top-level filter but invert under `NOT`:
//! `NOT status>=400` over a VARCHAR-pinned `status="accepted"` is a live
//! match while `NOT (TRY_CAST(status AS DECIMAL(38,6)) >= …)` stays NULL
//! and is filtered out — a false-positive live alert.

use aho_corasick::AhoCorasick;
use regex::Regex;
use serde_json::Value;

use crate::ast::{FilterOp, FilterValue, SearchStage, SearchToken};
use crate::compare::{self, PatternForm};
use crate::context::EvalContext;
use crate::emitter::EmitError;
use crate::pin_match::{
    CoercedValue, CompareOp, NullReadPolicy, Truth, and_all, coerce_form, compare_values, or_any,
    pattern_text,
};
use crate::schema::FieldTypes;

/// A compiled filter that can match JSON events in memory.
///
/// Compiled once from a [`SearchStage`], then called per-event
/// in the hot path. All regex patterns are pre-compiled.
pub struct CompiledFilter {
    /// OR-of-AND groups, mirroring the search stage structure.
    groups: Vec<Vec<TokenMatcher>>,
    /// Global time filter (hoisted from groups during parsing).
    time_filter: Option<TimeMatcher>,
    /// Absolute lower bound (`earliest=`).
    earliest: Option<chrono::DateTime<chrono::Utc>>,
    /// Absolute upper bound (`latest=`).
    latest: Option<chrono::DateTime<chrono::Utc>>,
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
    Compare { op: CompareOp, value: CoercedValue },
    InList { values: Vec<CoercedValue> },
    NotInList { values: Vec<CoercedValue> },
    Glob { regex: Regex, form: PatternForm },
    Regex { regex: Regex, form: PatternForm },
}

struct TextMatcher {
    /// SIMD-accelerated case-insensitive searcher (ASCII case folding,
    /// matching `DuckDB` ILIKE semantics).
    searcher: AhoCorasick,
    /// Whether the match is negated (NOT ILIKE).
    negated: bool,
}

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

impl CompiledFilter {
    /// Compile a filter from a parsed search stage.
    ///
    /// `pins` is the field catalog's full pin snapshot (ADR-0011):
    /// comparisons against pinned fields follow the same rule table the
    /// SQL emitter's `emit_with_pins` applies — batch/live parity is part
    /// of the contract. Pass an empty set where no catalog exists
    /// (embedded mode, plain unit tests); every comparison is then
    /// literal-driven.
    ///
    /// Regex and glob patterns are compiled eagerly. Invalid patterns
    /// are silently skipped (they would also fail at SQL execution time).
    ///
    /// # Errors
    ///
    /// Returns the same error the SQL emitter raises when a `SEVERITY`-pinned
    /// field is compared against a literal outside the ladder vocabulary.
    /// Such a filter has no in-memory meaning: compiling it to a
    /// match-nothing predicate would turn a typo into a silently empty
    /// live stream while the same query errors on `/api/v1/query`.
    pub fn compile(search: &SearchStage, pins: &FieldTypes) -> Result<Self, EmitError> {
        let time_filter = search.time_filter.as_ref().map(|tf| TimeMatcher {
            duration_secs: tf.node.duration.to_seconds(),
        });

        let earliest = search.earliest.as_ref().and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(&s.node)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc))
        });

        let latest = search.latest.as_ref().and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(&s.node)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc))
        });

        let groups = compile_groups(&search.groups, pins)?;

        Ok(Self {
            groups,
            time_filter,
            earliest,
            latest,
        })
    }

    /// Test whether a JSON event matches this filter, under the
    /// evaluation context this event is being processed with.
    ///
    /// `last=`/`earliest=`/`latest=` are clock-relative, so the filter is
    /// a `now()` reader like any other and takes its instant from the
    /// caller's [`EvalContext`] — never from a clock of its own
    /// (ADR-0017 §3). In the live lane that context is the event's own,
    /// so the window this filter applies and the `now()` a later
    /// `| where` reads are one instant; there is no second door that
    /// samples per batch, because a bus batch is an upstream client's
    /// POST size and not a boundary a query author can see.
    pub fn matches_at(&self, event: &serde_json::Map<String, Value>, ctx: &EvalContext) -> bool {
        // Check time filter first (global, not per-group).
        if let Some(tf) = &self.time_filter
            && !matches_time_filter_at(event, tf, ctx.now_utc())
        {
            return false;
        }

        // Check absolute time bounds.
        if self.earliest.is_some() || self.latest.is_some() {
            if let Some(ts) = extract_event_timestamp(event) {
                if let Some(earliest) = &self.earliest
                    && ts < *earliest
                {
                    return false;
                }
                if let Some(latest) = &self.latest
                    && ts >= *latest
                {
                    return false;
                }
            } else {
                // no timestamp → can't match time bounds
                return false;
            }
        }

        // Empty groups → match everything.
        if self.groups.is_empty() {
            return true;
        }

        // OR of AND, in SQL's three-valued logic: only TRUE is a match —
        // a WHERE clause evaluating to UNKNOWN filters the row out.
        eval_groups(&self.groups, event) == Some(true)
    }
}

/// Compile OR-of-AND groups, dropping tokens with no in-memory matcher.
fn compile_groups(
    groups: &[Vec<crate::ast::Spanned<SearchToken>>],
    pins: &FieldTypes,
) -> Result<Vec<Vec<TokenMatcher>>, EmitError> {
    groups
        .iter()
        .map(|group| {
            group
                .iter()
                .filter_map(|token| compile_token(&token.node, pins).transpose())
                .collect()
        })
        .collect()
}

fn compile_token(
    token: &SearchToken,
    pins: &FieldTypes,
) -> Result<Option<TokenMatcher>, EmitError> {
    Ok(match token {
        SearchToken::FieldFilter(ff) => {
            // The catalog pin typing this comparison (ADR-0011); the
            // lookup folds through `catalog_key`, same as the emitter.
            let pin = pins.pin_for(&ff.field);
            // Glob/regex match one canonical text per pin, resolved by the
            // shared rule table: plain stringification mirrors the SQL
            // side's bare column only where there is no pin to conform to
            // (unpinned, VARCHAR), and every typed pin renders the value's
            // own conformed reading — RFC 3339 microseconds for TIMESTAMP,
            // DuckDB's DOUBLE text for DOUBLE, the conformed integer for
            // BIGINT, lowercase `true`/`false` for BOOLEAN, the ladder
            // token for SEVERITY — so a pattern cannot mean one thing live
            // and another in batch. Each is corroborated by execution
            // probes in trawl-engine/tests/duckdb_probe.rs, not assumed.
            let form = compare::pattern_form(pin);
            let predicate = match (&ff.op, &ff.value) {
                (FilterOp::Glob, FilterValue::Literal(pattern)) => {
                    let Ok(regex) = Regex::new(&glob_to_regex(pattern)) else {
                        return Ok(None);
                    };
                    FieldPredicate::Glob { regex, form }
                }
                (FilterOp::Regex, FilterValue::Literal(pattern)) => {
                    let Ok(regex) = Regex::new(pattern) else {
                        return Ok(None);
                    };
                    FieldPredicate::Regex { regex, form }
                }
                (op, FilterValue::List(values)) => {
                    let values = values
                        .iter()
                        .map(|v| Ok(coerce_form(compare::compare_form(pin, FilterOp::Eq, v)?)))
                        .collect::<Result<_, EmitError>>()?;
                    match op {
                        FilterOp::Eq => FieldPredicate::InList { values },
                        FilterOp::Ne => FieldPredicate::NotInList { values },
                        FilterOp::Gt | FilterOp::Gte | FilterOp::Lt | FilterOp::Lte => {
                            unreachable!("ordered operators cannot reach a comma-separated list")
                        }
                        FilterOp::Glob | FilterOp::Regex => {
                            unreachable!("pattern operators cannot reach a comma-separated list")
                        }
                    }
                }
                (op, FilterValue::Literal(v)) => FieldPredicate::Compare {
                    op: compile_op(*op),
                    value: coerce_form(compare::compare_form(pin, *op, v)?),
                },
            };
            Some(TokenMatcher::Field(FieldMatcher {
                // The event key is the same `catalog_key` the pin lookup
                // used: an ASCII fold and nothing else, mirroring DuckDB
                // binding `"Status"` to the real `status` column. Ingest
                // folds every incoming field name, so an exact lookup on
                // the folded spelling is the one that finds the value —
                // without the fold a mixed-case reference would read every
                // event as a NULL column, which `!=` reports as a match
                // (`OR col IS NULL`): live tail would stream everything
                // while `/query` returned nothing.
                field: crate::schema::catalog_key(&ff.field),
                predicate,
            }))
        }
        SearchToken::TextSearch(ts) => {
            // Wildcard `*` matches everything — skip.
            if ts.term == "*" {
                return Ok(None);
            }
            let Ok(searcher) = AhoCorasick::builder()
                .ascii_case_insensitive(true)
                .build([&ts.term])
            else {
                return Ok(None);
            };
            Some(TokenMatcher::Text(TextMatcher {
                searcher,
                negated: ts.negated,
            }))
        }
        SearchToken::TimeFilter(_)
        | SearchToken::EarliestFilter(_)
        | SearchToken::LatestFilter(_) => {
            // Time filters are hoisted — this shouldn't appear in groups.
            None
        }
        SearchToken::QuotedSearch(qs) => {
            let Ok(searcher) = AhoCorasick::builder()
                .ascii_case_insensitive(true)
                .build([&qs.phrase])
            else {
                return Ok(None);
            };
            Some(TokenMatcher::Text(TextMatcher {
                searcher,
                negated: false,
            }))
        }
        SearchToken::Not(inner) => {
            let Some(inner_matcher) = compile_token(&inner.node, pins)? else {
                return Ok(None);
            };
            Some(TokenMatcher::Not(Box::new(inner_matcher)))
        }
        SearchToken::Group(groups) => {
            let compiled_groups = compile_groups(groups, pins)?;
            Some(TokenMatcher::OrGroup(compiled_groups))
        }
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
        // Glob and Regex are handled before compile_op is called in
        // compile_token. This arm is defensive only — if reached, the
        // filter value was already matched as a literal comparison.
        FilterOp::Glob | FilterOp::Regex => {
            debug_assert!(false, "glob/regex should be handled in compile_token");
            CompareOp::Eq
        }
    }
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

/// SQL `OR` of `AND` groups — the search stage's own shape.
fn eval_groups(groups: &[Vec<TokenMatcher>], event: &serde_json::Map<String, Value>) -> Truth {
    or_any(
        groups
            .iter()
            .map(|group| and_all(group.iter().map(|m| m.eval(event)))),
    )
}

impl TokenMatcher {
    fn eval(&self, event: &serde_json::Map<String, Value>) -> Truth {
        match self {
            Self::Field(fm) => fm.eval(event),
            Self::Text(tm) => Some(tm.eval(event)),
            // `NOT UNKNOWN` is UNKNOWN, never a match — the SQL `NOT (...)`
            // this mirrors stays NULL and the row is filtered out.
            Self::Not(inner) => inner.eval(event).map(|b| !b),
            Self::OrGroup(groups) => eval_groups(groups, event),
        }
    }
}

impl FieldMatcher {
    fn eval(&self, event: &serde_json::Map<String, Value>) -> Truth {
        let event_val = event.get(&self.field).filter(|v| !v.is_null());
        let Some(event_val) = event_val else {
            // SQL three-valued logic: a missing field is a NULL column and
            // a JSON null is a NULL value, so every comparison is UNKNOWN
            // — except `!=`, whose emitted form carries `OR col IS NULL`
            // and is therefore TRUE. Decided here, before coercion, so
            // every coercion class agrees with the SQL on nulls
            // (stringifying null to "" diverges on ordered comparisons).
            return match &self.predicate {
                FieldPredicate::Compare {
                    op: CompareOp::Ne, ..
                }
                | FieldPredicate::NotInList { .. } => Some(true),
                _ => None,
            };
        };

        match &self.predicate {
            FieldPredicate::Compare { op, value } => {
                compare_values(event_val, *op, value, NullReadPolicy::NeMatches)
            }
            FieldPredicate::InList { values } => {
                or_any(values.iter().map(|v| {
                    compare_values(event_val, CompareOp::Eq, v, NullReadPolicy::NeMatches)
                }))
            }
            FieldPredicate::NotInList { values } => {
                and_all(values.iter().map(|v| {
                    compare_values(event_val, CompareOp::Ne, v, NullReadPolicy::NeMatches)
                }))
            }
            FieldPredicate::Glob { regex, form } | FieldPredicate::Regex { regex, form } => {
                // No canonical text (a TIMESTAMP pin over a value with no
                // timestamp reading) is a NULL column in batch, and
                // `strftime(NULL, …) GLOB p` is NULL: UNKNOWN, not FALSE.
                pattern_text(event_val, *form).map(|s| regex.is_match(&s))
            }
        }
    }
}

impl TextMatcher {
    /// Bare-word search hits `message` and additionally `_raw` where
    /// present (ADR-0009). Mirrors the SQL exactly:
    ///
    /// - positive: `COALESCE("message" ILIKE p, FALSE) OR
    ///   COALESCE("_raw" ILIKE p, FALSE)` — a match in either column passes;
    ///   both missing/null is false.
    /// - negated: the two-valued complement — every carried column must be
    ///   term-free, and both missing/null is true.
    ///
    /// Searching `_raw` is whole-event search: unless a collector supplied a
    /// pre-parse line, `_raw` is the server's JSON serialization of the event,
    /// so a term matches another field's value *and* a field name — and the
    /// negated form excludes on the same basis (see
    /// [`crate::emitter`]'s `push_text_search` and the DSL reference).
    /// A missing/non-string column contributes false to containment. The leaf
    /// therefore always has a true or false answer, and grammar compositions
    /// such as `NOT term` inherit totality without a special case.
    fn eval(&self, event: &serde_json::Map<String, Value>) -> bool {
        let msg = match event.get("message") {
            Some(Value::String(s)) => self.searcher.is_match(s),
            _ => false,
        };
        let raw = match event.get("_raw") {
            Some(Value::String(s)) => self.searcher.is_match(s),
            _ => false,
        };
        self.negated != (msg || raw)
    }
}

// ---------------------------------------------------------------------------
// Time filter
// ---------------------------------------------------------------------------

/// Extract the event timestamp as a UTC datetime.
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

/// Parse a timestamp string in common formats.
///
/// Covers the most common log timestamp formats. Tried in order
/// of decreasing specificity to avoid ambiguous matches.
fn parse_timestamp(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    // RFC 3339 (most common for JSON): 2026-02-15T12:00:00Z, 2026-02-15T12:00:00.123Z
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    // ISO 8601 with timezone offset: 2026-02-15T12:00:00+05:30
    if let Ok(dt) = chrono::DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%:z") {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    // ISO 8601 with fractional seconds, no timezone (assume UTC).
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(naive.and_utc());
    }
    // ISO 8601, no fractional seconds, no timezone (assume UTC).
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(naive.and_utc());
    }
    // Space-separated: 2026-02-15 12:00:00
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Some(naive.and_utc());
    }
    // Space-separated with fractional seconds: 2026-02-15 12:00:00.123456
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f") {
        return Some(naive.and_utc());
    }
    // Epoch-seconds or epoch-millis as string.
    if let Ok(n) = s.parse::<i64>() {
        return parse_epoch_i64(n);
    }
    if let Ok(f) = s.parse::<f64>() {
        return parse_epoch_f64(f);
    }
    None
}

/// Parse an epoch timestamp from an i64.
///
/// Heuristic: values < 1e12 are epoch-seconds, >= 1e12 are epoch-millis.
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

/// Parse an epoch timestamp from an f64 (fractional seconds).
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

// ---------------------------------------------------------------------------
// Glob → regex conversion
// ---------------------------------------------------------------------------

/// Convert a `DuckDB` GLOB pattern to an anchored regex.
///
/// `DuckDB` GLOB is a full-match pattern (like SQL LIKE):
/// - `*` matches any sequence of characters
/// - `?` matches any single character
/// - `[...]` matches a character class (`[!...]` for negation)
/// - Everything else is literal (case-sensitive)
fn glob_to_regex(glob: &str) -> String {
    let mut regex = String::from("^");
    let mut chars = glob.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            '[' => {
                regex.push('[');
                // DuckDB uses `!` for negation in character classes.
                if chars.peek() == Some(&'!') {
                    chars.next();
                    regex.push('^');
                }
                // Copy until closing `]`.
                let mut first = true;
                loop {
                    match chars.next() {
                        Some(']') if !first => {
                            regex.push(']');
                            break;
                        }
                        Some(nc) => {
                            regex.push(nc);
                            first = false;
                        }
                        None => {
                            // Unterminated bracket — close it.
                            regex.push(']');
                            break;
                        }
                    }
                }
            }
            c if is_regex_meta(c) => {
                regex.push('\\');
                regex.push(c);
            }
            c => regex.push(c),
        }
    }

    regex.push('$');
    regex
}

/// Characters that have special meaning in regex and need escaping.
fn is_regex_meta(c: char) -> bool {
    matches!(
        c,
        '.' | '+' | '^' | '$' | '|' | '(' | ')' | '{' | '}' | '\\'
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    /// The fixed instant these tests evaluate at.
    ///
    /// Nothing here samples a clock: a filter's window is measured
    /// against the context it is handed (ADR-0017 §3), so a test that
    /// read `Utc::now()` would be racing the wall clock between event
    /// construction and evaluation for no gain.
    fn fixed_now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-08-24T12:00:00Z")
            .expect("literal is RFC 3339")
            .with_timezone(&chrono::Utc)
    }

    /// Helper: parse DSL, compile filter, test against event.
    fn matches_event(dsl: &str, event_json: &str) -> bool {
        let query = parser::parse(dsl).expect("parse should succeed");
        let filter = CompiledFilter::compile(&query.search, &crate::schema::FieldTypes::new())
            .expect("filter compiles");
        let event: serde_json::Map<String, Value> =
            serde_json::from_str(event_json).expect("valid JSON object");
        filter.matches_at(&event, &EvalContext::at(fixed_now()))
    }

    /// Helper: like `matches_event`, with catalog pins (ADR-0011).
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
        filter.matches_at(&event, &EvalContext::at(fixed_now()))
    }

    /// Helper: like `matches_event`, with an explicit `now`. Time-window
    /// tests use this to name the instant the window is measured from,
    /// rather than inheriting the file-wide fixture.
    fn matches_event_at(dsl: &str, event_json: &str, now: chrono::DateTime<chrono::Utc>) -> bool {
        let query = parser::parse(dsl).expect("parse should succeed");
        let filter = CompiledFilter::compile(&query.search, &crate::schema::FieldTypes::new())
            .expect("filter compiles");
        let event: serde_json::Map<String, Value> =
            serde_json::from_str(event_json).expect("valid JSON object");
        filter.matches_at(&event, &EvalContext::at(now))
    }

    // ── field filters ─────────────────────────────────────────────────

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
        // Numeric filter against numeric JSON value.
        assert!(matches_event(
            "status=200",
            r#"{"status": 200, "message": "ok"}"#
        ));
        // Numeric filter against string JSON value (implicit cast).
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
        // Parser auto-detects glob from `*` in value.
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
        // Parser auto-detects glob from `?` in value.
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
        // Character class semantics are covered by glob_conversion_* tests.
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

    // ── pin-aware comparisons (ADR-0011) ─────────────────────────────

    use crate::schema::CanonicalType as CT;

    const VARCHAR_STATUS: &[(&str, CT)] = &[("status", CT::Varchar)];
    const BIGINT_STATUS: &[(&str, CT)] = &[("status", CT::BigInt)];

    #[test]
    fn pinned_varchar_eq_numeric_compares_as_text_or_reading() {
        // String-stored "200" matches on the text arm.
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
        // Other spellings of the same number share its decimal reading.
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
        // A non-numeric literal keeps the pure text arm.
        assert!(matches_event_pinned(
            "status=accepted",
            r#"{"status": "accepted"}"#,
            VARCHAR_STATUS
        ));
    }

    /// `!=` is the complement of `=` over non-null values, including for
    /// the values with no numeric reading that motivate a VARCHAR pin:
    /// `status!=200` must still return `"accepted"`, not drop it into
    /// UNKNOWN. Mirrors the SQL's `COALESCE(…, TRUE)`.
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
        // SQL emits `(status != '200' OR status IS NULL)` — a JSON null
        // must match here too, or batch and live disagree on every
        // repaired event.
        assert!(matches_event_pinned(
            "status!=200",
            r#"{"status": null}"#,
            VARCHAR_STATUS
        ));
    }

    /// An absent key IS the null case: in batch, a row whose file never
    /// carried the field reads the column as NULL, so `!=` includes it
    /// through `OR col IS NULL`. Absent is the dominant shape on the bus
    /// (the canonicalizer only fills envelope fields), so a divergence
    /// here would drop every field-less event from live tail while
    /// `/query` returned them all. Executed against `DuckDB` in
    /// `tests/filter_parity.rs` (both pinned matrices carry the field-less
    /// event).
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
        // "404"/"500" have a DECIMAL(38,6) reading, so both sides compare
        // numerically.
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
        // Values with no reading are NULL — never a match, never an
        // error.
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
        // The reading is exact, not integral: "1.5" sits between 1 and 2.
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
        // Both halves fold, over an event whose key is spelled the way
        // ingest actually writes it (lowercase). The event lookup: DuckDB
        // binds `"Status"` to the real `status` column, so a mixed-case
        // reference must read the value, not an absent key — and `!=` over
        // an absent key matches (`OR col IS NULL`), so a fold miss here
        // streams every event live while `/query` returns none.
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
        // A JSON null still matches `!=` through the folded key.
        assert!(matches_event_pinned(
            "Status!=200",
            r#"{"status": null}"#,
            VARCHAR_STATUS
        ));
    }

    #[test]
    fn pinned_not_over_unknown_is_never_a_match() {
        // A TRY_CAST miss is NULL, so `NOT (TRY_CAST(status AS
        // DECIMAL(38,6)) >= …)` is NULL and the batch query drops the row — the live
        // stream must not fire on it (executed against DuckDB in
        // tests/filter_parity.rs::pinned_varchar_matrix_parity).
        assert!(!matches_event_pinned(
            "NOT status>=400",
            r#"{"status": "accepted"}"#,
            VARCHAR_STATUS
        ));
        // Same for a NULL column, across every predicate class.
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
        // `!=` is the total form (`OR col IS NULL`): TRUE on a null, so
        // NOT genuinely inverts to no-match.
        assert!(!matches_event_pinned(
            "NOT status!=200",
            r#"{"status": null}"#,
            VARCHAR_STATUS
        ));
        // A real FALSE still inverts — UNKNOWN is not a blanket veto.
        assert!(matches_event_pinned(
            "NOT status>=400",
            r#"{"status": "200"}"#,
            VARCHAR_STATUS
        ));
    }

    #[test]
    fn unpinned_not_over_null_column_is_not_a_match() {
        // The same rule without a catalog: `NOT (status = 200)` over a
        // NULL is NULL in SQL, so no live match either.
        assert!(!matches_event("NOT status=200", r#"{"status": null}"#));
        assert!(!matches_event("NOT status=200", r#"{"message": "hi"}"#));
        assert!(matches_event("NOT status=200", r#"{"status": 404}"#));
    }

    // ── text search ───────────────────────────────────────────────────

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

    /// Whole-event search (ADR-0009): with a server-filled `_raw` — the JSON
    /// serialization of the event — a bare term reaches another field's value
    /// and a field name, and the negated form excludes on the same basis.
    /// Mirrors `bare_word_search_reaches_the_whole_event_through_raw` in the
    /// engine's execution tests.
    #[test]
    fn text_search_covers_the_whole_event_through_raw() {
        let event = r#"{"message": "started", "service": "nginx", "debug_mode": false,
             "_raw": "{\"service\":\"nginx\",\"debug_mode\":false,\"message\":\"started\"}"}"#;
        // Another field's value, absent from `message`.
        assert!(matches_event("nginx", event));
        // A field name, present nowhere else.
        assert!(matches_event("debug", event));
        // Negation is the exact mirror.
        assert!(!matches_event("-debug", event));
        assert!(matches_event("-absent", event));
    }

    #[test]
    fn text_search_missing_message() {
        // No message field: positive search → no match.
        assert!(!matches_event("error", r#"{"service": "nginx"}"#));
        // Text containment is total (ADR-0015): absent columns do not contain
        // the term, so both negation spellings match.
        assert!(matches_event("-debug", r#"{"service": "nginx"}"#));
        assert!(matches_event("NOT debug", r#"{"service": "nginx"}"#));
    }

    // ── quoted search ─────────────────────────────────────────────────

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

    // ── wildcard ──────────────────────────────────────────────────────

    #[test]
    fn wildcard_matches_everything() {
        assert!(matches_event("*", r#"{"service": "nginx"}"#));
    }

    // ── OR groups ─────────────────────────────────────────────────────

    #[test]
    fn or_groups() {
        // Matches first group.
        assert!(matches_event(
            "service=nginx OR service=apache",
            r#"{"service": "nginx", "message": "ok"}"#
        ));
        // Matches second group.
        assert!(matches_event(
            "service=nginx OR service=apache",
            r#"{"service": "apache", "message": "ok"}"#
        ));
        // Matches neither.
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
        // Matches group 2.
        assert!(matches_event(
            dsl,
            r#"{"service": "apache", "status": 404, "message": "ok"}"#
        ));
        // Neither group fully matches.
        assert!(!matches_event(
            dsl,
            r#"{"service": "nginx", "status": 404, "message": "ok"}"#
        ));
    }

    // ── empty search ──────────────────────────────────────────────────

    #[test]
    fn empty_search_matches_all() {
        assert!(matches_event("", r#"{"service": "nginx"}"#));
    }

    // ── the severity vocabulary rides the pin, not the name ───────────

    /// `level` is ordinary sender vocabulary (ADR-0013 §6): every shape
    /// compiles in both lanes and matches the sender's own column, with
    /// no severity meaning attached to the name.
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

    /// The band vocabulary lives on the SEVERITY pin, where the live
    /// lane and the SQL agree by construction.
    #[test]
    fn severity_pin_carries_the_band_vocabulary() {
        let mut ft = crate::schema::FieldTypes::new();
        ft.insert("_severity", crate::schema::CanonicalType::Severity);
        let matches = |dsl: &str, json: &str| {
            let query = parser::parse(dsl).expect("parse should succeed");
            let filter = CompiledFilter::compile(&query.search, &ft).expect("compiles");
            let ev: serde_json::Map<String, Value> = serde_json::from_str(json).unwrap();
            filter.matches_at(&ev, &EvalContext::at(fixed_now()))
        };
        assert!(matches("_severity=error", r#"{"_severity": 17}"#));
        assert!(matches("_severity=error,fatal", r#"{"_severity": 21}"#));
        assert!(!matches("_severity=error", r#"{"_severity": 9}"#));
        assert!(matches("_severity=error2", r#"{"_severity": 18}"#));
        assert!(!matches("_severity=error2", r#"{"_severity": 17}"#));

        // An unknown value refuses in both lanes, with one sentence.
        let query = parser::parse("_severity=spicy").expect("parse should succeed");
        let emit_error = crate::emitter::emit_with_pins(
            &query,
            "/data/**/*.parquet",
            &ft,
            EvalContext::at(fixed_now()),
        )
        .expect_err("emit refuses")
        .to_string();
        let compile_error = CompiledFilter::compile(&query.search, &ft)
            .expect_err("filter refuses")
            .to_string();
        assert_eq!(compile_error, emit_error);
    }

    // ── null/missing field ────────────────────────────────────────────

    #[test]
    fn null_field_no_match() {
        assert!(!matches_event(
            "service=nginx",
            r#"{"service": null, "message": "ok"}"#
        ));
    }

    // ── glob → regex conversion ───────────────────────────────────────

    #[test]
    fn glob_conversion_star() {
        assert_eq!(glob_to_regex("*.txt"), r"^.*\.txt$");
    }

    #[test]
    fn glob_conversion_question() {
        assert_eq!(glob_to_regex("file?.log"), r"^file.\.log$");
    }

    #[test]
    fn glob_conversion_character_class() {
        assert_eq!(glob_to_regex("[abc]"), "^[abc]$");
    }

    #[test]
    fn glob_conversion_negated_class() {
        assert_eq!(glob_to_regex("[!abc]"), "^[^abc]$");
    }

    #[test]
    fn glob_conversion_escapes_dots() {
        assert_eq!(glob_to_regex("foo.bar"), r"^foo\.bar$");
    }

    // ── timestamp parsing ────────────────────────────────────────────

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
        // Should be the same instant as epoch-seconds version.
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

    // ── deterministic time filter tests ─────────────────────────────

    /// Helper: create an event with the given RFC 3339 timestamp.
    fn event_with_timestamp(ts: &str) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        m.insert("_time".into(), Value::String(ts.to_string()));
        m.insert("message".into(), Value::String("test".into()));
        m
    }

    /// Helper: create an event with an epoch-seconds timestamp.
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
        // Event 1 second before `now` → matches last=1h.
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
        // Event 2 hours before `now` → does NOT match last=1h.
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
        // Event at exactly the cutoff (3600 seconds before `now`) → matches:
        // the window comparison is >=. Injected `now` makes the boundary
        // exact — the wall-clock variant needed slack and still flaked.
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
        // Event 1 second past the cutoff → does NOT match last=1h.
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

        // Epoch-seconds timestamp 1 second before `now` → matches last=1h.
        let event = event_with_epoch_secs(now.timestamp() - 1);
        let json = serde_json::to_string(&event).unwrap();
        assert!(matches_event_at("last=1h", &json, now));

        // Epoch-seconds timestamp 2 hours before `now` → does NOT match.
        let event = event_with_epoch_secs(now.timestamp() - 7200);
        let json = serde_json::to_string(&event).unwrap();
        assert!(!matches_event_at("last=1h", &json, now));
    }

    #[test]
    fn time_filter_no_timestamp_excluded() {
        // Event without a timestamp field → should NOT match.
        let mut event = serde_json::Map::new();
        event.insert("message".into(), Value::String("test".into()));
        let json = serde_json::to_string(&event).unwrap();
        assert!(!matches_event("last=1h", &json));
    }

    // ── matches_at reads the context it is handed ───────────────────

    #[test]
    fn matches_at_uses_the_contexts_instant() {
        let query = parser::parse("last=1h").expect("parse should succeed");
        let filter = CompiledFilter::compile(&query.search, &crate::schema::FieldTypes::new())
            .expect("filter compiles");

        // Event 30 min ago from "now".
        let now = fixed_now();
        let event_ts = (now - chrono::Duration::minutes(30)).to_rfc3339();
        let event = event_with_timestamp(&event_ts);

        // Under that instant → matches (30 min < 1 hour).
        assert!(filter.matches_at(&event, &EvalContext::at(now)));

        // Under an instant 3 hours earlier the event is in the future,
        // which the window admits.
        let old_now = now - chrono::Duration::hours(3);
        assert!(filter.matches_at(&event, &EvalContext::at(old_now)));

        // Under an instant 2 hours later the event is >1h old → no
        // match. Three answers from one filter and one event: the
        // context is the only input that moved.
        let future_now = now + chrono::Duration::hours(2);
        assert!(!filter.matches_at(&event, &EvalContext::at(future_now)));
    }
}
