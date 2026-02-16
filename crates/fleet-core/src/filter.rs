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
    pub fn matches(&self, event: &serde_json::Map<String, Value>) -> bool {
        // Check time filter first (global, not per-group).
        if let Some(tf) = &self.time_filter {
            if !matches_time_filter(event, tf) {
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
        FilterOp::Ne => CompareOp::Ne,
        FilterOp::Gt => CompareOp::Gt,
        FilterOp::Gte => CompareOp::Gte,
        FilterOp::Lt => CompareOp::Lt,
        FilterOp::Lte => CompareOp::Lte,
        // Glob and Regex handled separately in compile_token;
        // this fallback is defensive only.
        FilterOp::Eq | FilterOp::Glob | FilterOp::Regex => CompareOp::Eq,
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
            // No message field → negated matches, positive doesn't.
            return self.negated;
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
/// `NaN` comparisons return false, matching SQL semantics.
fn apply_f64(a: f64, b: f64, op: CompareOp) -> bool {
    match op {
        CompareOp::Eq => (a - b).abs() < f64::EPSILON,
        CompareOp::Ne => (a - b).abs() >= f64::EPSILON,
        CompareOp::Gt => a > b,
        CompareOp::Gte => a >= b || (a - b).abs() < f64::EPSILON,
        CompareOp::Lt => a < b,
        CompareOp::Lte => a <= b || (a - b).abs() < f64::EPSILON,
    }
}

// ---------------------------------------------------------------------------
// Time filter
// ---------------------------------------------------------------------------

fn matches_time_filter(event: &serde_json::Map<String, Value>, tf: &TimeMatcher) -> bool {
    let Some(Value::String(ts_str)) = event.get("timestamp") else {
        return false;
    };

    // Try parsing common ISO 8601 formats.
    let event_time = parse_timestamp(ts_str);
    let Some(event_time) = event_time else {
        return false;
    };

    #[allow(clippy::cast_possible_wrap)]
    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(tf.duration_secs as i64);
    event_time >= cutoff
}

/// Parse a timestamp string in common ISO 8601 formats.
fn parse_timestamp(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    // Try RFC 3339 first (most common for JSON).
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    // Try without timezone (assume UTC).
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(naive.and_utc());
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Some(naive.and_utc());
    }
    None
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
        // Negated search: no message field → match (NOT contains = true).
        assert!(matches_event("-debug", r#"{"service": "nginx"}"#));
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
}
