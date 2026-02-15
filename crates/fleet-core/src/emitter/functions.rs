use super::EmitError;
use super::fields::quote_field;

/// Known function names accepted by the emitter.
pub(crate) const KNOWN_FUNCTIONS: &[&str] = &[
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
];

/// Validate that a function name is known and argument count is correct.
///
/// Single source of truth for arity — used by both pre-emission
/// validation (`validate.rs`) and translation (`translate_function`).
pub(crate) fn validate_function_arity(name: &str, argc: usize) -> Result<(), EmitError> {
    if !KNOWN_FUNCTIONS.contains(&name) {
        return Err(EmitError::UnknownFunction {
            name: name.to_string(),
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
        "coalesce" => (1, None),
        "if" | "replace" => (3, Some(3)),
        "substr" => (2, Some(3)),
        "round" => (1, Some(2)),
        "now" => (0, Some(0)),
        // everything else: exactly 1
        _ => (1, Some(1)),
    }
}

/// Arg positions (0-indexed) that must be inlined as literal integers
/// rather than parameterized (`DuckDB` requirement).
pub(crate) fn literal_int_positions(name: &str) -> &'static [usize] {
    match name {
        "round" => &[1], // precision arg
        _ => &[],
    }
}

/// Translate a DSL function call to `DuckDB` SQL.
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
        // new aggregate functions
        "first" => require_one_arg(name, args, |a| format!("FIRST({a})")),
        "last" => require_one_arg(name, args, |a| format!("LAST({a})")),
        "values" | "list" => require_one_arg(name, args, |a| format!("LIST(DISTINCT {a})")),
        "median" => require_one_arg(name, args, |a| format!("MEDIAN({a})")),
        "stddev" => require_one_arg(name, args, |a| format!("STDDEV({a})")),
        _ => Err(EmitError::UnknownFunction {
            name: name.to_string(),
        }),
    }
}

/// Check whether a function name refers to an aggregate function.
#[allow(dead_code)] // exposed for future use by validation/planning layers
pub(crate) fn is_aggregate_function(name: &str) -> bool {
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
                name: "bogus".to_string()
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
        ] {
            assert!(
                !is_aggregate_function(name),
                "{name} should not be aggregate"
            );
        }
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
