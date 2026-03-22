// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! "Did you mean?" suggestions via Levenshtein distance.
//!
//! Provides typo correction for pipe stage names and function names,
//! enabling contextual hints in parse error messages.

/// All recognized pipe stage keywords.
pub const KNOWN_PIPE_STAGES: &[&str] = &[
    "from",
    "stats",
    "timechart",
    "where",
    "sort",
    "limit",
    "head",
    "tail",
    "let",
    "eval",
    "extract",
    "rex",
    "table",
    "fields",
    "top",
    "rare",
    "dedup",
    "drop",
    "rename",
    "pivot",
    "sample",
    "eventstats",
];

/// Known function names — single source of truth, also used by emitter validation.
pub const KNOWN_FUNCTIONS: &[&str] = &[
    // aggregates
    "count",
    "avg",
    "sum",
    "min",
    "max",
    "dc",
    "distinct_count",
    "p50",
    "p90",
    "p95",
    "p99",
    "first",
    "last",
    "values",
    "list",
    "median",
    "stddev",
    // scalars
    "lower",
    "upper",
    "length",
    "len",
    "coalesce",
    "if",
    "replace",
    "substr",
    "trim",
    "ltrim",
    "rtrim",
    "isnull",
    "isnotnull",
    "abs",
    "ceil",
    "ceiling",
    "floor",
    "round",
    "now",
    "typeof",
    "tonumber",
    "tostring",
    // string
    "contains",
    "startswith",
    "endswith",
    "split",
    "concat",
    // date/time
    "date_part",
    "date_trunc",
    "date_diff",
    "strftime",
    "strptime",
    // conditional
    "case",
    // json
    "json",
    "json_extract",
    "json_extract_string",
    "json_valid",
    "json_keys",
    "json_array_length",
];

/// Compute the Levenshtein edit distance between two strings.
///
/// Uses a standard two-row dynamic programming approach — O(min(a,b)) space.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let (a_len, b_len) = (a_chars.len(), b_chars.len());

    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr = vec![0; b_len + 1];

    for i in 1..=a_len {
        curr[0] = i;
        for j in 1..=b_len {
            let cost = usize::from(a_chars[i - 1] != b_chars[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b_len]
}

/// Find the closest match in `candidates` for `input`, within `max_distance`.
///
/// Returns `None` if no candidate is within range or if the input is empty.
pub fn suggest_closest<'a>(
    input: &str,
    candidates: &[&'a str],
    max_distance: usize,
) -> Option<&'a str> {
    if input.is_empty() {
        return None;
    }

    let input_lower = input.to_lowercase();
    let mut best: Option<(&str, usize)> = None;

    for &candidate in candidates {
        let dist = levenshtein(&input_lower, candidate);
        if dist <= max_distance && best.as_ref().is_none_or(|(_, d)| dist < *d) {
            best = Some((candidate, dist));
        }
    }

    best.map(|(name, _)| name)
}

/// Suggest a pipe stage name for a typo.
pub fn suggest_pipe_stage(input: &str) -> Option<&'static str> {
    suggest_closest(input, KNOWN_PIPE_STAGES, 2)
}

/// Suggest a function name for a typo.
pub fn suggest_function(input: &str) -> Option<&'static str> {
    suggest_closest(input, KNOWN_FUNCTIONS, 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_identical() {
        assert_eq!(levenshtein("stats", "stats"), 0);
    }

    #[test]
    fn levenshtein_one_edit() {
        assert_eq!(levenshtein("stats", "staats"), 1);
        assert_eq!(levenshtein("stats", "stat"), 1);
        assert_eq!(levenshtein("stats", "statx"), 1);
    }

    #[test]
    fn levenshtein_two_edits() {
        assert_eq!(levenshtein("stats", "staatz"), 2);
    }

    #[test]
    fn levenshtein_empty() {
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("", ""), 0);
    }

    #[test]
    fn suggest_pipe_stage_typo() {
        assert_eq!(suggest_pipe_stage("staats"), Some("stats"));
        assert_eq!(suggest_pipe_stage("whre"), Some("where"));
        assert_eq!(suggest_pipe_stage("sortt"), Some("sort"));
        assert_eq!(suggest_pipe_stage("limt"), Some("limit"));
    }

    #[test]
    fn suggest_pipe_stage_exact() {
        assert_eq!(suggest_pipe_stage("stats"), Some("stats"));
    }

    #[test]
    fn suggest_pipe_stage_no_match() {
        assert_eq!(suggest_pipe_stage("zzzzzzz"), None);
        assert_eq!(suggest_pipe_stage(""), None);
    }

    #[test]
    fn suggest_function_typo() {
        assert_eq!(suggest_function("countt"), Some("count"));
        assert_eq!(suggest_function("avgg"), Some("avg"));
        assert_eq!(suggest_function("summ"), Some("sum"));
    }

    #[test]
    fn suggest_function_no_match() {
        assert_eq!(suggest_function("totallyunknown"), None);
    }

    #[test]
    fn suggest_case_insensitive() {
        assert_eq!(suggest_pipe_stage("STATS"), Some("stats"));
        assert_eq!(suggest_pipe_stage("Stats"), Some("stats"));
    }

    /// Ensures every entry in `KNOWN_PIPE_STAGES` actually parses as a valid
    /// pipe stage. Catches drift between this list and the parser's `choice()`.
    #[test]
    fn known_pipe_stages_all_parse() {
        for &stage in KNOWN_PIPE_STAGES {
            // Build a minimal valid query for each stage.
            let query = match stage {
                "stats" => "| stats count()".to_string(),
                "timechart" => "| timechart span=1h count()".to_string(),
                "where" => "| where x > 1".to_string(),
                "sort" => "| sort host".to_string(),
                "limit" | "head" | "tail" => format!("| {stage} 10"),
                "let" => "| let x = 1".to_string(),
                "eval" => "| eval x = 1".to_string(),
                "extract" => r#"| extract "(?P<ip>\d+)" from message"#.to_string(),
                "rex" => r#"| rex "(?P<ip>\d+)" from message"#.to_string(),
                "table" | "fields" | "drop" => format!("| {stage} host"),
                "top" => "| top 5 host".to_string(),
                "rare" => "| rare 5 host".to_string(),
                "dedup" => "| dedup host".to_string(),
                "rename" => "| rename host as hostname".to_string(),
                "pivot" => "| pivot count() on status".to_string(),
                "sample" => "| sample 10%".to_string(),
                "eventstats" => "| eventstats count()".to_string(),
                "from" => "| from saved daily_errors".to_string(),
                _ => panic!("unhandled stage '{stage}' in test — add a case"),
            };
            assert!(
                crate::parser::parse(&query).is_ok(),
                "KNOWN_PIPE_STAGES entry '{stage}' failed to parse with query: {query}"
            );
        }
    }

    /// Ensures every entry in `KNOWN_FUNCTIONS` is recognized by the emitter
    /// (catches drift between the list and the emitter's match arms).
    #[test]
    fn known_functions_all_validate() {
        for &func in KNOWN_FUNCTIONS {
            // Build a query that uses the function in a stats stage.
            // Functions with 0 args: count, now. Others: pass one dummy arg.
            let query = match func {
                "count" | "now" => format!("| stats {func}()"),
                "if" | "replace" => format!("| let x = {func}(a, b, c)"),
                "coalesce" | "concat" => format!("| let x = {func}(a, b)"),
                "substr"
                | "contains"
                | "startswith"
                | "endswith"
                | "date_part"
                | "date_trunc"
                | "strftime"
                | "strptime"
                | "json"
                | "json_extract_string"
                | "json_extract" => {
                    format!(r#"| let x = {func}(a, "b")"#)
                }
                "split" | "date_diff" => format!(r#"| let x = {func}(a, "b", 1)"#),
                "case" => format!(r#"| let x = {func}(a > 1, "yes", "no")"#),
                "round" => format!("| let x = {func}(a)"),
                // Aggregates use stats, scalars use let/eval
                _ if crate::emitter::is_aggregate_function(func) => {
                    format!("| stats {func}(x)")
                }
                _ => format!("| let y = {func}(x)"),
            };
            let ast = crate::parser::parse(&query)
                .unwrap_or_else(|e| panic!("'{func}' failed to parse: {e:?}"));
            let result = crate::emitter::validate_pipeline(&ast.pipeline);
            assert!(
                !matches!(
                    result,
                    Err(crate::emitter::EmitError::UnknownFunction { .. })
                ),
                "KNOWN_FUNCTIONS entry '{func}' rejected as unknown by emitter"
            );
        }
    }
}
