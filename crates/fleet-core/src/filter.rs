//! In-memory event filter compiled from the search stage of a DSL query.
//!
//! [`CompiledFilter`] provides semantically equivalent matching to the SQL
//! WHERE clauses emitted by the search stage emitter. Used for real-time
//! event filtering in the SSE streaming endpoint.
//!
//! Key invariant: `filter.matches(event)` must agree with running the
//! emitted SQL against `DuckDB` for every `(event, search_stage)` pair.

use regex::Regex;
use serde_json::Value;

use crate::ast::{FilterOp, FilterValue, SearchStage, SearchToken};

/// A compiled filter that can match JSON events in memory.
///
/// Compiled once from a [`SearchStage`], then called per-event
/// in the hot path. All regex patterns are pre-compiled.
pub struct CompiledFilter {
    /// OR-of-AND groups, mirroring the search stage structure.
    groups: Vec<Vec<TokenMatcher>>,
    /// Global time filter (hoisted from groups during parsing).
    time_filter: Option<TimeMatcher>,
}

impl std::fmt::Debug for CompiledFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledFilter")
            .field("groups", &self.groups.len())
            .field("time_filter", &self.time_filter.is_some())
            .finish()
    }
}

struct TimeMatcher {
    duration_secs: u64,
}

enum TokenMatcher {
    Field(FieldMatcher),
    Text(TextMatcher),
}

struct FieldMatcher {
    field: String,
    predicate: FieldPredicate,
}

enum FieldPredicate {
    Compare { op: CompareOp, value: CoercedValue },
    InList { values: Vec<CoercedValue> },
    Glob { regex: Regex },
    Regex { regex: Regex },
}

#[derive(Clone, Copy)]
enum CompareOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}

/// A filter value coerced to the most specific numeric type.
///
/// Mirrors the coercion in `emitter::fields::coerce_filter_value()`.
#[derive(Clone, Debug)]
enum CoercedValue {
    Int(i64),
    Float(f64),
    Str(String),
}

struct TextMatcher {
    /// Lowercased search term for case-insensitive matching.
    term_lower: String,
    /// Whether the match is negated (NOT ILIKE).
    negated: bool,
}

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

impl CompiledFilter {
    /// Compile a filter from a parsed search stage.
    ///
    /// Regex and glob patterns are compiled eagerly. Invalid patterns
    /// are silently skipped (they would also fail at SQL execution time).
    pub fn compile(search: &SearchStage) -> Self {
        let time_filter = search.time_filter.as_ref().map(|tf| TimeMatcher {
            duration_secs: tf.node.duration.to_seconds(),
        });

        let groups = search
            .groups
            .iter()
            .map(|group| {
                group
                    .iter()
                    .filter_map(|token| compile_token(&token.node))
                    .collect()
            })
            .collect();

        Self {
            groups,
            time_filter,
        }
    }

    /// Test whether a JSON event matches this filter.
    ///
    /// Convenience wrapper that computes `Utc::now()` per call. For batch
    /// filtering (e.g. SSE streams), prefer [`matches_at`](Self::matches_at)
    /// with a pre-computed timestamp to avoid a syscall per event.
    pub fn matches(&self, event: &serde_json::Map<String, Value>) -> bool {
        self.matches_at(event, chrono::Utc::now())
    }

    /// Test whether a JSON event matches this filter using a pre-computed
    /// timestamp for the time filter cutoff.
    ///
    /// Avoids a `Utc::now()` syscall per event — compute `now` once per
    /// batch and pass it to each event.
    pub fn matches_at(
        &self,
        event: &serde_json::Map<String, Value>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> bool {
        // Check time filter first (global, not per-group).
        if let Some(tf) = &self.time_filter {
            if !matches_time_filter_at(event, tf, now) {
                return false;
            }
        }

        // Empty groups → match everything.
        if self.groups.is_empty() {
            return true;
        }

        // OR of AND: any group where all matchers pass.
        self.groups
            .iter()
            .any(|group| group.iter().all(|m| m.matches(event)))
    }
}

fn compile_token(token: &SearchToken) -> Option<TokenMatcher> {
    match token {
        SearchToken::FieldFilter(ff) => {
            let predicate = match (&ff.op, &ff.value) {
                (FilterOp::Glob, FilterValue::Literal(pattern)) => {
                    let regex = Regex::new(&glob_to_regex(pattern)).ok()?;
                    FieldPredicate::Glob { regex }
                }
                (FilterOp::Regex, FilterValue::Literal(pattern)) => {
                    let regex = Regex::new(pattern).ok()?;
                    FieldPredicate::Regex { regex }
                }
                (_, FilterValue::List(values)) => FieldPredicate::InList {
                    values: values.iter().map(|v| coerce_value(v)).collect(),
                },
                (op, FilterValue::Literal(v)) => FieldPredicate::Compare {
                    op: compile_op(*op),
                    value: coerce_value(v),
                },
            };
            Some(TokenMatcher::Field(FieldMatcher {
                field: ff.field.clone(),
                predicate,
            }))
        }
        SearchToken::TextSearch(ts) => {
            // Wildcard `*` matches everything — skip.
            if ts.term == "*" {
                return None;
            }
            Some(TokenMatcher::Text(TextMatcher {
                term_lower: ts.term.to_lowercase(),
                negated: ts.negated,
            }))
        }
        SearchToken::TimeFilter(_) => {
            // Time filters are hoisted — this shouldn't appear in groups.
            None
        }
        SearchToken::QuotedSearch(qs) => Some(TokenMatcher::Text(TextMatcher {
            term_lower: qs.phrase.to_lowercase(),
            negated: false,
        })),
    }
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

/// Coerce a string filter value to the most specific type.
///
/// Matches `emitter::fields::coerce_filter_value()` exactly.
fn coerce_value(s: &str) -> CoercedValue {
    if let Ok(i) = s.parse::<i64>() {
        return CoercedValue::Int(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return CoercedValue::Float(f);
    }
    CoercedValue::Str(s.to_string())
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

impl TokenMatcher {
    fn matches(&self, event: &serde_json::Map<String, Value>) -> bool {
        match self {
            Self::Field(fm) => fm.matches(event),
            Self::Text(tm) => tm.matches(event),
        }
    }
}

impl FieldMatcher {
    fn matches(&self, event: &serde_json::Map<String, Value>) -> bool {
        let Some(event_val) = event.get(&self.field) else {
            // Missing field → no match (mirrors SQL NULL semantics).
            return false;
        };

        match &self.predicate {
            FieldPredicate::Compare { op, value } => compare_values(event_val, *op, value),
            FieldPredicate::InList { values } => values
                .iter()
                .any(|v| compare_values(event_val, CompareOp::Eq, v)),
            FieldPredicate::Glob { regex } | FieldPredicate::Regex { regex } => {
                let s = json_to_string(event_val);
                regex.is_match(&s)
            }
        }
    }
}

impl TextMatcher {
    fn matches(&self, event: &serde_json::Map<String, Value>) -> bool {
        let Some(Value::String(msg)) = event.get("message") else {
            // Missing/null message → no match (matches SQL NULL semantics).
            // DuckDB: NULL ILIKE/NOT ILIKE → NULL → excluded from results.
            return false;
        };
        let contains = msg.to_lowercase().contains(&self.term_lower);
        if self.negated { !contains } else { contains }
    }
}

/// Compare a JSON event value against a coerced filter value.
///
/// Implements type promotion matching `DuckDB`'s implicit casting:
/// - Int filter: try to extract event value as i64 (number or string parse)
/// - Float filter: try to extract event value as f64
/// - String filter: compare as strings (convert event value to string if needed)
fn compare_values(event_val: &Value, op: CompareOp, filter_val: &CoercedValue) -> bool {
    match filter_val {
        CoercedValue::Int(fv) => {
            if let Some(ev) = extract_i64(event_val) {
                apply_ord(ev.cmp(fv), op)
            } else if let Some(ev) = extract_f64(event_val) {
                // Promote filter value to f64 for mixed comparison.
                #[allow(clippy::cast_precision_loss)]
                apply_f64(ev, *fv as f64, op)
            } else {
                // String filter value that happened to parse as int —
                // fall back to string comparison.
                false
            }
        }
        CoercedValue::Float(fv) => {
            if let Some(ev) = extract_f64(event_val) {
                apply_f64(ev, *fv, op)
            } else {
                false
            }
        }
        CoercedValue::Str(fv) => {
            let ev = json_to_string(event_val);
            apply_ord(ev.as_str().cmp(fv.as_str()), op)
        }
    }
}

/// Try to extract an i64 from a JSON value (number or parseable string).
fn extract_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Try to extract an f64 from a JSON value (number or parseable string).
fn extract_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Convert a JSON value to its string representation for comparison.
fn json_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        // Arrays and objects: serialize to JSON string.
        other => other.to_string(),
    }
}

/// Apply an ordering-based comparison operator.
fn apply_ord(ord: std::cmp::Ordering, op: CompareOp) -> bool {
    match op {
        CompareOp::Eq => ord.is_eq(),
        CompareOp::Ne => !ord.is_eq(),
        CompareOp::Gt => ord.is_gt(),
        CompareOp::Gte => ord.is_ge(),
        CompareOp::Lt => ord.is_lt(),
        CompareOp::Lte => ord.is_le(),
    }
}

/// Apply a comparison on f64 values.
///
/// Uses exact IEEE 754 operators to match `DuckDB`'s behavior.
/// `NaN` comparisons return false, matching SQL semantics.
#[allow(clippy::float_cmp)] // intentional: must match DuckDB's exact comparison
fn apply_f64(a: f64, b: f64, op: CompareOp) -> bool {
    match op {
        CompareOp::Eq => a == b,
        CompareOp::Ne => a != b,
        CompareOp::Gt => a > b,
        CompareOp::Gte => a >= b,
        CompareOp::Lt => a < b,
        CompareOp::Lte => a <= b,
    }
}

// ---------------------------------------------------------------------------
// Time filter
// ---------------------------------------------------------------------------

fn matches_time_filter_at(
    event: &serde_json::Map<String, Value>,
    tf: &TimeMatcher,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let Some(ts_val) = event.get("timestamp") else {
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

    /// Helper: parse DSL, compile filter, test against event.
    fn matches_event(dsl: &str, event_json: &str) -> bool {
        let query = parser::parse(dsl).expect("parse should succeed");
        let filter = CompiledFilter::compile(&query.search);
        let event: serde_json::Map<String, Value> =
            serde_json::from_str(event_json).expect("valid JSON object");
        filter.matches(&event)
    }

    // ── field filters ─────────────────────────────────────────────────

    #[test]
    fn field_eq_string() {
        assert!(matches_event(
            "service:nginx",
            r#"{"service": "nginx", "message": "ok"}"#
        ));
        assert!(!matches_event(
            "service:nginx",
            r#"{"service": "apache", "message": "ok"}"#
        ));
    }

    #[test]
    fn field_eq_numeric() {
        // Numeric filter against numeric JSON value.
        assert!(matches_event(
            "status:200",
            r#"{"status": 200, "message": "ok"}"#
        ));
        // Numeric filter against string JSON value (implicit cast).
        assert!(matches_event(
            "status:200",
            r#"{"status": "200", "message": "ok"}"#
        ));
        assert!(!matches_event(
            "status:200",
            r#"{"status": 404, "message": "ok"}"#
        ));
    }

    #[test]
    fn field_ne() {
        assert!(matches_event(
            "status:!=200",
            r#"{"status": 404, "message": "ok"}"#
        ));
        assert!(!matches_event(
            "status:!=200",
            r#"{"status": 200, "message": "ok"}"#
        ));
    }

    #[test]
    fn field_gt() {
        assert!(matches_event(
            "status:>400",
            r#"{"status": 500, "message": "ok"}"#
        ));
        assert!(!matches_event(
            "status:>400",
            r#"{"status": 200, "message": "ok"}"#
        ));
    }

    #[test]
    fn field_gte() {
        assert!(matches_event(
            "status:>=400",
            r#"{"status": 400, "message": "ok"}"#
        ));
        assert!(matches_event(
            "status:>=400",
            r#"{"status": 500, "message": "ok"}"#
        ));
    }

    #[test]
    fn field_lt() {
        assert!(matches_event(
            "status:<300",
            r#"{"status": 200, "message": "ok"}"#
        ));
        assert!(!matches_event(
            "status:<300",
            r#"{"status": 500, "message": "ok"}"#
        ));
    }

    #[test]
    fn field_lte() {
        assert!(matches_event(
            "status:<=300",
            r#"{"status": 300, "message": "ok"}"#
        ));
    }

    #[test]
    fn field_in_list() {
        assert!(matches_event(
            "status:200,301,404",
            r#"{"status": 301, "message": "ok"}"#
        ));
        assert!(!matches_event(
            "status:200,301,404",
            r#"{"status": 500, "message": "ok"}"#
        ));
    }

    #[test]
    fn field_glob() {
        // Parser auto-detects glob from `*` in value.
        assert!(matches_event(
            "path:/api/*",
            r#"{"path": "/api/users", "message": "ok"}"#
        ));
        assert!(!matches_event(
            "path:/api/*",
            r#"{"path": "/web/index", "message": "ok"}"#
        ));
    }

    #[test]
    fn field_glob_question_mark() {
        // Parser auto-detects glob from `?` in value.
        assert!(matches_event(
            "host:web-?",
            r#"{"host": "web-1", "message": "ok"}"#
        ));
        assert!(!matches_event(
            "host:web-?",
            r#"{"host": "web-10", "message": "ok"}"#
        ));
    }

    #[test]
    fn field_glob_with_character_class() {
        // `[` alone doesn't trigger glob auto-detection, so include `*`.
        // Character class semantics are covered by glob_conversion_* tests.
        assert!(matches_event(
            "host:web-[123]*",
            r#"{"host": "web-2-prod", "message": "ok"}"#
        ));
        assert!(!matches_event(
            "host:web-[123]*",
            r#"{"host": "web-5-prod", "message": "ok"}"#
        ));
    }

    #[test]
    fn field_regex() {
        assert!(matches_event(
            r"host:/web-\d+/",
            r#"{"host": "web-42", "message": "ok"}"#
        ));
        assert!(!matches_event(
            r"host:/web-\d+/",
            r#"{"host": "db-01", "message": "ok"}"#
        ));
    }

    #[test]
    fn missing_field_no_match() {
        assert!(!matches_event(
            "service:nginx",
            r#"{"host": "web-1", "message": "ok"}"#
        ));
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

    #[test]
    fn text_search_missing_message() {
        // No message field: positive search → no match.
        assert!(!matches_event("error", r#"{"service": "nginx"}"#));
        // Negated search: no message field → no match (SQL NULL semantics:
        // NULL NOT ILIKE → NULL → excluded from results).
        assert!(!matches_event("-debug", r#"{"service": "nginx"}"#));
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
            "service:nginx OR service:apache",
            r#"{"service": "nginx", "message": "ok"}"#
        ));
        // Matches second group.
        assert!(matches_event(
            "service:nginx OR service:apache",
            r#"{"service": "apache", "message": "ok"}"#
        ));
        // Matches neither.
        assert!(!matches_event(
            "service:nginx OR service:apache",
            r#"{"service": "postgres", "message": "ok"}"#
        ));
    }

    #[test]
    fn or_groups_with_and() {
        // `service:nginx level:error OR service:apache level:warn`
        // Group 1: service=nginx AND level=error
        // Group 2: service=apache AND level=warn
        assert!(matches_event(
            "service:nginx level:error OR service:apache level:warn",
            r#"{"service": "nginx", "level": "error", "message": "ok"}"#
        ));
        // Matches group 2.
        assert!(matches_event(
            "service:nginx level:error OR service:apache level:warn",
            r#"{"service": "apache", "level": "warn", "message": "ok"}"#
        ));
        // Neither group fully matches (service=nginx but level=warn).
        assert!(!matches_event(
            "service:nginx level:error OR service:apache level:warn",
            r#"{"service": "nginx", "level": "warn", "message": "ok"}"#
        ));
    }

    // ── empty search ──────────────────────────────────────────────────

    #[test]
    fn empty_search_matches_all() {
        assert!(matches_event("", r#"{"service": "nginx"}"#));
    }

    // ── null/missing field ────────────────────────────────────────────

    #[test]
    fn null_field_no_match() {
        assert!(!matches_event(
            "service:nginx",
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
    fn parse_timestamp_invalid() {
        assert!(parse_timestamp("not-a-date").is_none());
        assert!(parse_timestamp("").is_none());
    }

    // ── deterministic time filter tests ─────────────────────────────

    /// Helper: create an event with the given RFC 3339 timestamp.
    fn event_with_timestamp(ts: &str) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        m.insert("timestamp".into(), Value::String(ts.to_string()));
        m.insert("message".into(), Value::String("test".into()));
        m
    }

    /// Helper: create an event with an epoch-seconds timestamp.
    fn event_with_epoch_secs(secs: i64) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        m.insert(
            "timestamp".into(),
            Value::Number(serde_json::Number::from(secs)),
        );
        m.insert("message".into(), Value::String("test".into()));
        m
    }

    #[test]
    fn time_filter_recent_event_matches() {
        // Event 1 second ago → should match last:1h
        let recent = (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        let event = event_with_timestamp(&recent);
        assert!(matches_event(
            "last:1h",
            &serde_json::to_string(&event).unwrap()
        ));
    }

    #[test]
    fn time_filter_old_event_excluded() {
        // Event 2 hours ago → should NOT match last:1h
        let old = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let event = event_with_timestamp(&old);
        assert!(!matches_event(
            "last:1h",
            &serde_json::to_string(&event).unwrap()
        ));
    }

    #[test]
    fn time_filter_boundary_matches() {
        // Event at exactly the cutoff (3600 seconds ago) → should match
        // (>= comparison). We use 3599s to avoid sub-millisecond races.
        let boundary = (chrono::Utc::now() - chrono::Duration::seconds(3599)).to_rfc3339();
        let event = event_with_timestamp(&boundary);
        assert!(matches_event(
            "last:1h",
            &serde_json::to_string(&event).unwrap()
        ));
    }

    #[test]
    fn time_filter_just_past_boundary_excluded() {
        // Event 1 second past the cutoff → should NOT match last:1h.
        let past = (chrono::Utc::now() - chrono::Duration::seconds(3601)).to_rfc3339();
        let event = event_with_timestamp(&past);
        assert!(!matches_event(
            "last:1h",
            &serde_json::to_string(&event).unwrap()
        ));
    }

    #[test]
    fn time_filter_epoch_seconds_event() {
        // Epoch-seconds timestamp 1 second ago → should match last:1h.
        let recent_epoch = chrono::Utc::now().timestamp() - 1;
        let event = event_with_epoch_secs(recent_epoch);
        let json = serde_json::to_string(&event).unwrap();
        assert!(matches_event("last:1h", &json));

        // Epoch-seconds timestamp 2 hours ago → should NOT match.
        let old_epoch = chrono::Utc::now().timestamp() - 7200;
        let event = event_with_epoch_secs(old_epoch);
        let json = serde_json::to_string(&event).unwrap();
        assert!(!matches_event("last:1h", &json));
    }

    #[test]
    fn time_filter_no_timestamp_excluded() {
        // Event without a timestamp field → should NOT match.
        let mut event = serde_json::Map::new();
        event.insert("message".into(), Value::String("test".into()));
        let json = serde_json::to_string(&event).unwrap();
        assert!(!matches_event("last:1h", &json));
    }

    // ── matches_at with explicit timestamp ──────────────────────────

    #[test]
    fn matches_at_uses_provided_now() {
        let query = parser::parse("last:1h").expect("parse should succeed");
        let filter = CompiledFilter::compile(&query.search);

        // Event 30 min ago from "now".
        let now = chrono::Utc::now();
        let event_ts = (now - chrono::Duration::minutes(30)).to_rfc3339();
        let event = event_with_timestamp(&event_ts);

        // With real now → should match (30 min < 1 hour).
        assert!(filter.matches_at(&event, now));

        // With a fake "now" that is 2 hours before the event → event
        // is in the future relative to this now, should match.
        let old_now = now - chrono::Duration::hours(3);
        assert!(filter.matches_at(&event, old_now));

        // With a fake "now" where event is >1h old → should NOT match.
        let future_now = now + chrono::Duration::hours(2);
        assert!(!filter.matches_at(&event, future_now));
    }
}
