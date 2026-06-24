// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::EmitError;
use super::fields::quote_field;
use crate::parser::suggest;

/// Known function names accepted by the emitter (canonical list in `parser::suggest`).
pub(crate) use crate::parser::suggest::KNOWN_FUNCTIONS;

/// Validate that a function name is known and argument count is correct.
///
/// Single source of truth for arity — used by both pre-emission
/// validation (`validate.rs`) and translation (`translate_function`).
pub(crate) fn validate_function_arity(name: &str, argc: usize) -> Result<(), EmitError> {
    if !KNOWN_FUNCTIONS.contains(&name) {
        return Err(EmitError::UnknownFunction {
            name: name.to_string(),
            suggestion: suggest::suggest_function(name).map(String::from),
        });
    }

    let (min, max) = function_arity(name);
    if argc < min || max.is_some_and(|m| argc > m) {
        let message = match (min, max) {
            (0, Some(0)) => format!("{name}() requires exactly 0 argument(s)"),
            (1, Some(1)) => format!("{name}() requires exactly one argument"),
            (n, Some(m)) if n == m => format!("{name}() requires exactly {n} argument(s)"),
            (min, Some(max)) => format!("{name}() requires {min} to {max} arguments"),
            (1, None) => format!("{name}() requires at least one argument"),
            (min, None) => format!("{name}() requires at least {min} argument(s)"),
        };
        return Err(EmitError::InvalidAggregation { message });
    }

    Ok(())
}

/// Return `(min_args, max_args)` for a function. `None` max means unbounded.
fn function_arity(name: &str) -> (usize, Option<usize>) {
    match name {
        "count" => (0, Some(1)),
        "coalesce" | "concat" => (1, None),
        "if" | "replace" | "split" | "date_diff" => (3, Some(3)),
        "case" => (2, None),
        "substr" => (2, Some(3)),
        "round" => (1, Some(2)),
        "now" => (0, Some(0)),
        "contains"
        | "startswith"
        | "endswith"
        | "date_part"
        | "date_trunc"
        | "strftime"
        | "strptime"
        | "json"
        | "json_extract_string"
        | "json_extract" => (2, Some(2)),
        // everything else: exactly 1
        _ => (1, Some(1)),
    }
}

/// Arg positions (0-indexed) that must be inlined as literal integers
/// rather than parameterized (`DuckDB` requirement).
pub(crate) fn literal_int_positions(name: &str) -> &'static [usize] {
    match name {
        "round" => &[1], // precision arg
        "split" => &[2], // index arg (0-indexed DSL → 1-indexed DuckDB)
        _ => &[],
    }
}

// ── Date/time unit allowlists ─────────────────────────────────────────

/// Units valid for `date_trunc` and `date_diff`.
pub const DATE_UNITS: &[&str] = &[
    "year", "quarter", "month", "week", "day", "hour", "minute", "second",
];

/// Units valid for `date_part` (superset of `DATE_UNITS`).
pub const DATE_PART_UNITS: &[&str] = &[
    "year", "quarter", "month", "week", "day", "hour", "minute", "second", "dow", "doy", "epoch",
];

/// Arg positions (0-indexed) that must be string literal date/time unit names.
///
/// Returns `(arg_index, allowlist)` pairs. The `emit_expr` `FunctionCall` arm
/// calls `validate_unit_literal` for each returned pair.
pub(crate) fn unit_literal_positions(name: &str) -> &'static [(usize, &'static [&'static str])] {
    match name {
        "date_part" => &[(0, DATE_PART_UNITS)],
        "date_trunc" | "date_diff" => &[(0, DATE_UNITS)],
        _ => &[],
    }
}

/// Arg position (0-indexed) of a `strftime`/`strptime` format string, if any.
///
/// When the arg at this index is a string literal, both the batch (`emit_expr`)
/// and streaming (`stream::validate_expr_formats`) paths run it through
/// `validate_format_literal` so an invalid code is rejected identically.
pub(crate) fn format_literal_position(name: &str) -> Option<usize> {
    match name {
        // DSL arg order: strftime(ts, fmt), strptime(str, fmt) — fmt is index 1.
        "strftime" | "strptime" => Some(1),
        _ => None,
    }
}

/// Validate that a date/time unit argument is a known literal.
///
/// `allowlist` is the set of accepted unit names for this `(func, arg)` pair
/// (the caller already fetched it via `unit_literal_positions`, so it is passed
/// in directly rather than re-derived). `unit_literal` is `Some(value)` when the
/// arg at `arg_idx` is a string literal, or `None` when it is a computed
/// expression. Rejects non-literals and unknown units; this check runs in
/// **both** the batch and streaming paths so an unsupported unit can never error
/// in batch while silently nulling in live tail.
pub(crate) fn validate_unit_literal(
    func_name: &str,
    arg_idx: usize,
    allowlist: &[&str],
    unit_literal: Option<&str>,
) -> Result<(), EmitError> {
    let Some(unit) = unit_literal else {
        return Err(EmitError::UnsupportedOperation {
            message: format!(
                "{func_name}() argument {} must be a string literal unit name, not an expression",
                arg_idx + 1
            ),
        });
    };
    if !allowlist.is_empty() && !allowlist.contains(&unit.to_lowercase().as_str()) {
        return Err(EmitError::UnsupportedOperation {
            message: format!(
                "{func_name}() unit {unit:?} is not in the allowed set: {}",
                allowlist.join(", ")
            ),
        });
    }
    Ok(())
}

/// Validate a `strftime`/`strptime` format-string **literal**.
///
/// chrono is the canonical format authority for both the batch (`DuckDB`) and
/// streaming (eval) paths. An invalid code (e.g. `%Q`) or a trailing `%`
/// parses into a `chrono::format::Item::Error`; rejecting it here at
/// emit/compile time makes both paths fail identically instead of
/// erroring-in-batch / silently-nulling-in-stream. The same `Item::Error`
/// detection is used by the defensive runtime guard in `eval::eval_strftime`.
///
/// Only call this for string-literal format args — a non-literal (field ref)
/// can't be checked up front and keeps its pre-existing runtime behaviour.
pub(crate) fn validate_format_literal(func_name: &str, fmt: &str) -> Result<(), EmitError> {
    if chrono::format::StrftimeItems::new(fmt)
        .any(|item| matches!(item, chrono::format::Item::Error))
    {
        return Err(EmitError::InvalidFormat {
            func_name: func_name.to_string(),
            format: fmt.to_string(),
        });
    }
    Ok(())
}

/// Translate a DSL function call to `DuckDB` SQL.
#[allow(clippy::too_many_lines)]
pub(crate) fn translate_function(name: &str, args: &[String]) -> Result<String, EmitError> {
    match name {
        "count" => {
            if args.is_empty() {
                Ok("COUNT(*)".to_string())
            } else {
                Ok(format!("COUNT({})", args[0]))
            }
        }
        "avg" => require_one_arg(name, args, |a| format!("AVG({a})")),
        "sum" => require_one_arg(name, args, |a| format!("SUM({a})")),
        "min" => require_one_arg(name, args, |a| format!("MIN({a})")),
        "max" => require_one_arg(name, args, |a| format!("MAX({a})")),
        "dc" | "distinct_count" => require_one_arg(name, args, |a| format!("COUNT(DISTINCT {a})")),
        "p50" => percentile(name, args, 0.5),
        "p90" => percentile(name, args, 0.9),
        "p95" => percentile(name, args, 0.95),
        "p99" => percentile(name, args, 0.99),
        "lower" => require_one_arg(name, args, |a| format!("LOWER({a})")),
        "upper" => require_one_arg(name, args, |a| format!("UPPER({a})")),
        "length" | "len" => require_one_arg(name, args, |a| format!("LENGTH({a})")),
        "coalesce" => {
            if args.is_empty() {
                return Err(EmitError::InvalidAggregation {
                    message: "coalesce() requires at least one argument".to_string(),
                });
            }
            Ok(format!("COALESCE({})", args.join(", ")))
        }
        // new scalar functions
        "if" => require_n_args(name, args, 3, |a| {
            format!("IF({}, {}, {})", a[0], a[1], a[2])
        }),
        "replace" => require_n_args(name, args, 3, |a| {
            format!("REPLACE({}, {}, {})", a[0], a[1], a[2])
        }),
        "substr" => require_range_args(name, args, 2, 3, |a| {
            if a.len() == 2 {
                format!("SUBSTR({}, {})", a[0], a[1])
            } else {
                format!("SUBSTR({}, {}, {})", a[0], a[1], a[2])
            }
        }),
        "trim" => require_one_arg(name, args, |a| format!("TRIM({a})")),
        "ltrim" => require_one_arg(name, args, |a| format!("LTRIM({a})")),
        "rtrim" => require_one_arg(name, args, |a| format!("RTRIM({a})")),
        "isnull" => require_one_arg(name, args, |a| format!("({a} IS NULL)")),
        "isnotnull" => require_one_arg(name, args, |a| format!("({a} IS NOT NULL)")),
        "abs" => require_one_arg(name, args, |a| format!("ABS({a})")),
        "ceil" | "ceiling" => require_one_arg(name, args, |a| format!("CEIL({a})")),
        "floor" => require_one_arg(name, args, |a| format!("FLOOR({a})")),
        "round" => require_range_args(name, args, 1, 2, |a| {
            if a.len() == 1 {
                format!("ROUND({})", a[0])
            } else {
                format!("ROUND({}, {})", a[0], a[1])
            }
        }),
        "now" => require_n_args(name, args, 0, |_| "now()".to_string()),
        "typeof" => require_one_arg(name, args, |a| format!("TYPEOF({a})")),
        "tonumber" => require_one_arg(name, args, |a| format!("TRY_CAST({a} AS DOUBLE)")),
        "tostring" => require_one_arg(name, args, |a| format!("CAST({a} AS VARCHAR)")),
        // string functions
        "contains" => require_n_args(name, args, 2, |a| format!("CONTAINS({}, {})", a[0], a[1])),
        "startswith" => require_n_args(name, args, 2, |a| {
            format!("STARTS_WITH({}, {})", a[0], a[1])
        }),
        "endswith" => require_n_args(name, args, 2, |a| format!("SUFFIX({}, {})", a[0], a[1])),
        "split" => require_n_args(name, args, 3, |a| {
            // DSL is 0-indexed, DuckDB STRING_SPLIT is 1-indexed
            let idx: i64 = a[2].parse().expect("split index must be literal int");
            format!("STRING_SPLIT({}, {})[{}]", a[0], a[1], idx + 1)
        }),
        "concat" => {
            if args.is_empty() {
                return Err(EmitError::InvalidAggregation {
                    message: "concat() requires at least one argument".to_string(),
                });
            }
            Ok(format!("CONCAT({})", args.join(", ")))
        }
        // date/time functions
        "date_part" => require_n_args(name, args, 2, |a| format!("DATE_PART({}, {})", a[0], a[1])),
        "date_trunc" => {
            require_n_args(name, args, 2, |a| format!("DATE_TRUNC({}, {})", a[0], a[1]))
        }
        "date_diff" => require_n_args(name, args, 3, |a| {
            format!("DATE_DIFF({}, {}, {})", a[0], a[1], a[2])
        }),
        // strftime: emit in DSL order (timestamp, format). DuckDB's STRFTIME is
        // overloaded and accepts (timestamp, format) directly, so no swap is
        // needed. The previous text-swap (`a[1], a[0]`) desynchronized the `?`
        // placeholders from emit_expr's DSL-order param push: whenever the
        // timestamp arg itself produced bound params (e.g. a nested strptime
        // with literal args), the placeholders bound positionally to the wrong
        // values and the call misbound.
        "strftime" => require_n_args(name, args, 2, |a| format!("STRFTIME({}, {})", a[0], a[1])),
        // TRY_STRPTIME (not STRPTIME) so an unparseable input yields NULL, not a
        // whole-query error — matching the streaming eval path, which nulls.
        "strptime" => require_n_args(name, args, 2, |a| {
            format!("TRY_STRPTIME({}, {})", a[0], a[1])
        }),
        // conditional
        "case" => {
            if args.len() < 2 {
                return Err(EmitError::InvalidAggregation {
                    message: "case() requires at least 2 arguments".to_string(),
                });
            }
            let mut sql = String::from("CASE");
            let pairs = args.len() / 2;
            for i in 0..pairs {
                use std::fmt::Write as _;
                let _ = write!(sql, " WHEN {} THEN {}", args[i * 2], args[i * 2 + 1]);
            }
            if args.len() % 2 == 1 {
                use std::fmt::Write as _;
                let _ = write!(sql, " ELSE {}", args[args.len() - 1]);
            }
            sql.push_str(" END");
            Ok(sql)
        }
        // json functions
        "json" | "json_extract_string" => require_n_args(name, args, 2, |a| {
            format!("JSON_EXTRACT_STRING({}, {})", a[0], a[1])
        }),
        "json_extract" => require_n_args(name, args, 2, |a| {
            format!("JSON_EXTRACT({}, {})", a[0], a[1])
        }),
        "json_valid" => require_one_arg(name, args, |a| format!("JSON_VALID({a})")),
        "json_keys" => require_one_arg(name, args, |a| format!("JSON_KEYS({a})")),
        "json_array_length" => require_one_arg(name, args, |a| format!("JSON_ARRAY_LENGTH({a})")),
        // new aggregate functions
        "first" => require_one_arg(name, args, |a| format!("FIRST({a})")),
        "last" => require_one_arg(name, args, |a| format!("LAST({a})")),
        "values" | "list" => require_one_arg(name, args, |a| format!("LIST(DISTINCT {a})")),
        "median" => require_one_arg(name, args, |a| format!("MEDIAN({a})")),
        "stddev" => require_one_arg(name, args, |a| format!("STDDEV({a})")),
        _ => Err(EmitError::UnknownFunction {
            name: name.to_string(),
            suggestion: suggest::suggest_function(name).map(String::from),
        }),
    }
}

/// Check whether a function name refers to an aggregate function.
pub fn is_aggregate_function(name: &str) -> bool {
    matches!(
        name,
        "count"
            | "avg"
            | "sum"
            | "min"
            | "max"
            | "dc"
            | "distinct_count"
            | "p50"
            | "p90"
            | "p95"
            | "p99"
            | "first"
            | "last"
            | "values"
            | "list"
            | "median"
            | "stddev"
    )
}

/// Generate a default alias for an aggregation expression.
///
/// `count()` → `"count"`, `avg(duration)` → `"avg_duration"`.
pub(crate) fn default_agg_alias(func_name: &str, first_arg: Option<&str>) -> String {
    match first_arg {
        Some(arg) => quote_field(&format!("{func_name}_{arg}")),
        None => quote_field(func_name),
    }
}

fn require_one_arg(
    name: &str,
    args: &[String],
    f: impl FnOnce(&str) -> String,
) -> Result<String, EmitError> {
    if args.len() != 1 {
        return Err(EmitError::InvalidAggregation {
            message: format!("{name}() requires exactly one argument"),
        });
    }
    Ok(f(&args[0]))
}

fn require_n_args(
    name: &str,
    args: &[String],
    n: usize,
    f: impl FnOnce(&[String]) -> String,
) -> Result<String, EmitError> {
    if args.len() != n {
        return Err(EmitError::InvalidAggregation {
            message: format!("{name}() requires exactly {n} argument(s)"),
        });
    }
    Ok(f(args))
}

fn require_range_args(
    name: &str,
    args: &[String],
    min: usize,
    max: usize,
    f: impl FnOnce(&[String]) -> String,
) -> Result<String, EmitError> {
    if args.len() < min || args.len() > max {
        return Err(EmitError::InvalidAggregation {
            message: format!("{name}() requires {min} to {max} arguments"),
        });
    }
    Ok(f(args))
}

fn percentile(name: &str, args: &[String], p: f64) -> Result<String, EmitError> {
    if args.len() != 1 {
        return Err(EmitError::InvalidAggregation {
            message: format!("{name}() requires exactly one argument"),
        });
    }
    Ok(format!(
        "PERCENTILE_CONT({p}) WITHIN GROUP (ORDER BY {})",
        args[0]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| (*s).to_string()).collect()
    }

    // ── translate_function: aggregates ──────────────────────────────────

    #[test]
    fn translate_count_no_args() {
        assert_eq!(translate_function("count", &[]).unwrap(), "COUNT(*)");
    }

    #[test]
    fn translate_count_with_field() {
        assert_eq!(
            translate_function("count", &args(&["host"])).unwrap(),
            "COUNT(host)"
        );
    }

    #[test]
    fn translate_avg() {
        assert_eq!(
            translate_function("avg", &args(&["duration"])).unwrap(),
            "AVG(duration)"
        );
    }

    #[test]
    fn translate_sum() {
        assert_eq!(
            translate_function("sum", &args(&["bytes"])).unwrap(),
            "SUM(bytes)"
        );
    }

    #[test]
    fn translate_min() {
        assert_eq!(
            translate_function("min", &args(&["latency"])).unwrap(),
            "MIN(latency)"
        );
    }

    #[test]
    fn translate_max() {
        assert_eq!(
            translate_function("max", &args(&["latency"])).unwrap(),
            "MAX(latency)"
        );
    }

    #[test]
    fn translate_dc() {
        assert_eq!(
            translate_function("dc", &args(&["host"])).unwrap(),
            "COUNT(DISTINCT host)"
        );
    }

    #[test]
    fn translate_distinct_count() {
        assert_eq!(
            translate_function("distinct_count", &args(&["host"])).unwrap(),
            "COUNT(DISTINCT host)"
        );
    }

    #[test]
    fn translate_p50() {
        assert_eq!(
            translate_function("p50", &args(&["duration"])).unwrap(),
            "PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY duration)"
        );
    }

    #[test]
    fn translate_p90() {
        assert_eq!(
            translate_function("p90", &args(&["duration"])).unwrap(),
            "PERCENTILE_CONT(0.9) WITHIN GROUP (ORDER BY duration)"
        );
    }

    #[test]
    fn translate_p95() {
        assert_eq!(
            translate_function("p95", &args(&["duration"])).unwrap(),
            "PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY duration)"
        );
    }

    #[test]
    fn translate_p99() {
        assert_eq!(
            translate_function("p99", &args(&["duration"])).unwrap(),
            "PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY duration)"
        );
    }

    // ── translate_function: scalars ─────────────────────────────────────

    #[test]
    fn translate_lower() {
        assert_eq!(
            translate_function("lower", &args(&["host"])).unwrap(),
            "LOWER(host)"
        );
    }

    #[test]
    fn translate_upper() {
        assert_eq!(
            translate_function("upper", &args(&["host"])).unwrap(),
            "UPPER(host)"
        );
    }

    #[test]
    fn translate_length() {
        assert_eq!(
            translate_function("length", &args(&["msg"])).unwrap(),
            "LENGTH(msg)"
        );
    }

    #[test]
    fn translate_len_alias() {
        assert_eq!(
            translate_function("len", &args(&["msg"])).unwrap(),
            "LENGTH(msg)"
        );
    }

    #[test]
    fn translate_coalesce() {
        assert_eq!(
            translate_function("coalesce", &args(&["a", "b", "c"])).unwrap(),
            "COALESCE(a, b, c)"
        );
    }

    // ── translate_function: new scalars ────────────────────────────────

    #[test]
    fn translate_if() {
        assert_eq!(
            translate_function("if", &args(&["x > 0", "'pos'", "'neg'"])).unwrap(),
            "IF(x > 0, 'pos', 'neg')"
        );
    }

    #[test]
    fn translate_replace() {
        assert_eq!(
            translate_function("replace", &args(&["msg", "'foo'", "'bar'"])).unwrap(),
            "REPLACE(msg, 'foo', 'bar')"
        );
    }

    #[test]
    fn translate_substr_two_args() {
        assert_eq!(
            translate_function("substr", &args(&["msg", "1"])).unwrap(),
            "SUBSTR(msg, 1)"
        );
    }

    #[test]
    fn translate_substr_three_args() {
        assert_eq!(
            translate_function("substr", &args(&["msg", "1", "5"])).unwrap(),
            "SUBSTR(msg, 1, 5)"
        );
    }

    #[test]
    fn translate_trim() {
        assert_eq!(
            translate_function("trim", &args(&["msg"])).unwrap(),
            "TRIM(msg)"
        );
    }

    #[test]
    fn translate_isnull() {
        assert_eq!(
            translate_function("isnull", &args(&["x"])).unwrap(),
            "(x IS NULL)"
        );
    }

    #[test]
    fn translate_isnotnull() {
        assert_eq!(
            translate_function("isnotnull", &args(&["x"])).unwrap(),
            "(x IS NOT NULL)"
        );
    }

    #[test]
    fn translate_abs() {
        assert_eq!(translate_function("abs", &args(&["x"])).unwrap(), "ABS(x)");
    }

    #[test]
    fn translate_ceil() {
        assert_eq!(
            translate_function("ceil", &args(&["x"])).unwrap(),
            "CEIL(x)"
        );
    }

    #[test]
    fn translate_ceiling_alias() {
        assert_eq!(
            translate_function("ceiling", &args(&["x"])).unwrap(),
            "CEIL(x)"
        );
    }

    #[test]
    fn translate_floor() {
        assert_eq!(
            translate_function("floor", &args(&["x"])).unwrap(),
            "FLOOR(x)"
        );
    }

    #[test]
    fn translate_round_one_arg() {
        assert_eq!(
            translate_function("round", &args(&["x"])).unwrap(),
            "ROUND(x)"
        );
    }

    #[test]
    fn translate_round_two_args() {
        assert_eq!(
            translate_function("round", &args(&["x", "2"])).unwrap(),
            "ROUND(x, 2)"
        );
    }

    #[test]
    fn translate_now() {
        assert_eq!(translate_function("now", &[]).unwrap(), "now()");
    }

    #[test]
    fn translate_typeof() {
        assert_eq!(
            translate_function("typeof", &args(&["x"])).unwrap(),
            "TYPEOF(x)"
        );
    }

    #[test]
    fn translate_tonumber() {
        assert_eq!(
            translate_function("tonumber", &args(&["x"])).unwrap(),
            "TRY_CAST(x AS DOUBLE)"
        );
    }

    #[test]
    fn translate_tostring() {
        assert_eq!(
            translate_function("tostring", &args(&["x"])).unwrap(),
            "CAST(x AS VARCHAR)"
        );
    }

    #[test]
    fn translate_strftime_dsl_order() {
        // DSL (ts, fmt) emits in the same order — STRFTIME is overloaded in
        // DuckDB so no swap is needed, and emitting in DSL order keeps the `?`
        // placeholders aligned with emit_expr's DSL-order param push.
        assert_eq!(
            translate_function("strftime", &args(&["ts", "fmt"])).unwrap(),
            "STRFTIME(ts, fmt)"
        );
    }

    #[test]
    fn translate_strptime_uses_try_variant() {
        // TRY_STRPTIME nulls on unparseable input (matches streaming eval), so a
        // single bad value never errors the whole batch query.
        assert_eq!(
            translate_function("strptime", &args(&["s", "fmt"])).unwrap(),
            "TRY_STRPTIME(s, fmt)"
        );
    }

    // ── translate_function: new aggregates ───────────────────────────────

    #[test]
    fn translate_first() {
        assert_eq!(
            translate_function("first", &args(&["msg"])).unwrap(),
            "FIRST(msg)"
        );
    }

    #[test]
    fn translate_last() {
        assert_eq!(
            translate_function("last", &args(&["msg"])).unwrap(),
            "LAST(msg)"
        );
    }

    #[test]
    fn translate_values() {
        assert_eq!(
            translate_function("values", &args(&["level"])).unwrap(),
            "LIST(DISTINCT level)"
        );
    }

    #[test]
    fn translate_list_alias() {
        assert_eq!(
            translate_function("list", &args(&["level"])).unwrap(),
            "LIST(DISTINCT level)"
        );
    }

    #[test]
    fn translate_median() {
        assert_eq!(
            translate_function("median", &args(&["dur"])).unwrap(),
            "MEDIAN(dur)"
        );
    }

    #[test]
    fn translate_stddev() {
        assert_eq!(
            translate_function("stddev", &args(&["dur"])).unwrap(),
            "STDDEV(dur)"
        );
    }

    // ── translate_function: error cases ─────────────────────────────────

    #[test]
    fn translate_unknown_function() {
        let err = translate_function("bogus", &[]).unwrap_err();
        assert_eq!(
            err,
            EmitError::UnknownFunction {
                name: "bogus".to_string(),
                suggestion: None,
            }
        );
    }

    #[test]
    fn translate_avg_no_args_errors() {
        let err = translate_function("avg", &[]).unwrap_err();
        assert!(matches!(err, EmitError::InvalidAggregation { .. }));
    }

    #[test]
    fn translate_avg_too_many_args_errors() {
        let err = translate_function("avg", &args(&["a", "b"])).unwrap_err();
        assert!(matches!(err, EmitError::InvalidAggregation { .. }));
    }

    #[test]
    fn translate_coalesce_no_args_errors() {
        let err = translate_function("coalesce", &[]).unwrap_err();
        assert!(matches!(err, EmitError::InvalidAggregation { .. }));
    }

    #[test]
    fn translate_p99_no_args_errors() {
        let err = translate_function("p99", &[]).unwrap_err();
        assert!(matches!(err, EmitError::InvalidAggregation { .. }));
    }

    // ── is_aggregate_function ───────────────────────────────────────────

    #[test]
    fn aggregates_detected() {
        for name in [
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
        ] {
            assert!(is_aggregate_function(name), "{name} should be aggregate");
        }
    }

    #[test]
    fn scalars_not_aggregate() {
        for name in [
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
            "bogus",
            "tonumber",
            "tostring",
        ] {
            assert!(
                !is_aggregate_function(name),
                "{name} should not be aggregate"
            );
        }
    }

    // ── validate_unit_literal ───────────────────────────────────────────

    #[test]
    fn date_part_accepts_allowlisted_units() {
        for unit in [
            "year", "month", "day", "hour", "minute", "second", "dow", "doy", "epoch",
        ] {
            assert!(
                validate_unit_literal("date_part", 0, DATE_PART_UNITS, Some(unit)).is_ok(),
                "date_part should accept unit {unit:?}"
            );
        }
    }

    #[test]
    fn date_trunc_accepts_allowlisted_units() {
        for unit in [
            "year", "quarter", "month", "week", "day", "hour", "minute", "second",
        ] {
            assert!(
                validate_unit_literal("date_trunc", 0, DATE_UNITS, Some(unit)).is_ok(),
                "date_trunc should accept unit {unit:?}"
            );
        }
    }

    #[test]
    fn date_diff_accepts_allowlisted_units() {
        for unit in [
            "year", "quarter", "month", "week", "day", "hour", "minute", "second",
        ] {
            assert!(
                validate_unit_literal("date_diff", 0, DATE_UNITS, Some(unit)).is_ok(),
                "date_diff should accept unit {unit:?}"
            );
        }
    }

    #[test]
    fn date_part_rejects_unknown_unit() {
        let err =
            validate_unit_literal("date_part", 0, DATE_PART_UNITS, Some("nanosecond")).unwrap_err();
        assert!(
            matches!(err, EmitError::UnsupportedOperation { ref message } if message.contains("nanosecond")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn date_trunc_rejects_dow_not_in_allowlist() {
        // dow is in DATE_PART_UNITS but NOT in DATE_UNITS (date_trunc allowlist)
        let err = validate_unit_literal("date_trunc", 0, DATE_UNITS, Some("dow")).unwrap_err();
        assert!(matches!(err, EmitError::UnsupportedOperation { .. }));
    }

    #[test]
    fn date_diff_rejects_non_literal() {
        // None means the arg was a computed expression, not a string literal
        let err = validate_unit_literal("date_diff", 0, DATE_UNITS, None).unwrap_err();
        assert!(
            matches!(err, EmitError::UnsupportedOperation { ref message } if message.contains("literal")),
            "unexpected: {err}"
        );
    }

    #[test]
    fn emit_date_part_unknown_unit_errors() {
        use crate::emitter::emit;
        use crate::parser;
        let q = parser::parse(r#"* | let h = date_part("nanosecond", timestamp)"#).unwrap();
        let err = emit(&q, "/data/**/*.parquet").unwrap_err();
        assert!(matches!(err, EmitError::UnsupportedOperation { .. }));
    }

    #[test]
    fn emit_date_trunc_dow_errors() {
        use crate::emitter::emit;
        use crate::parser;
        let q = parser::parse(r#"* | let d = date_trunc("dow", timestamp)"#).unwrap();
        let err = emit(&q, "/data/**/*.parquet").unwrap_err();
        assert!(matches!(err, EmitError::UnsupportedOperation { .. }));
    }

    // ── validate_format_literal ─────────────────────────────────────────

    #[test]
    fn validate_format_literal_accepts_standard_codes() {
        for fmt in ["%Y-%m-%d", "%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%m/%d/%Y"] {
            assert!(
                validate_format_literal("strftime", fmt).is_ok(),
                "should accept format {fmt:?}"
            );
        }
    }

    #[test]
    fn validate_format_literal_rejects_invalid_code() {
        let err = validate_format_literal("strftime", "%Q").unwrap_err();
        assert!(
            matches!(err, EmitError::InvalidFormat { ref func_name, ref format }
                if func_name == "strftime" && format == "%Q"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_format_literal_rejects_trailing_percent() {
        let err = validate_format_literal("strptime", "%Y-%m-%d %").unwrap_err();
        assert!(matches!(err, EmitError::InvalidFormat { .. }), "{err}");
    }

    #[test]
    fn emit_strftime_invalid_format_errors() {
        use crate::emitter::emit;
        use crate::parser;
        let q = parser::parse(r#"* | let s = strftime(timestamp, "%Q")"#).unwrap();
        let err = emit(&q, "/data/**/*.parquet").unwrap_err();
        assert!(matches!(err, EmitError::InvalidFormat { .. }), "{err}");
    }

    #[test]
    fn emit_strptime_invalid_format_errors() {
        use crate::emitter::emit;
        use crate::parser;
        let q = parser::parse(r#"* | let t = strptime(message, "%Q")"#).unwrap();
        let err = emit(&q, "/data/**/*.parquet").unwrap_err();
        assert!(matches!(err, EmitError::InvalidFormat { .. }), "{err}");
    }

    // ── default_agg_alias ───────────────────────────────────────────────

    #[test]
    fn alias_count_no_arg() {
        assert_eq!(default_agg_alias("count", None), "\"count\"");
    }

    #[test]
    fn alias_avg_with_field() {
        assert_eq!(
            default_agg_alias("avg", Some("duration")),
            "\"avg_duration\""
        );
    }

    #[test]
    fn alias_dc_with_field() {
        assert_eq!(default_agg_alias("dc", Some("host")), "\"dc_host\"");
    }
}
