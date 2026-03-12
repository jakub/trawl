//! "Did you mean?" suggestions via Levenshtein distance.
//!
//! Provides typo correction for pipe stage names and function names,
//! enabling contextual hints in parse error messages.

/// All recognized pipe stage keywords.
pub const KNOWN_PIPE_STAGES: &[&str] = &[
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
];

/// Known function names (re-exported from emitter for suggestion use).
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
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
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
        if dist <= max_distance {
            if best.is_none() || dist < best.unwrap().1 {
                best = Some((candidate, dist));
            }
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
}
