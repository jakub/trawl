use super::EmitError;
use super::fields::quote_field;

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
        ] {
            assert!(is_aggregate_function(name), "{name} should be aggregate");
        }
    }

    #[test]
    fn scalars_not_aggregate() {
        for name in ["lower", "upper", "length", "len", "coalesce", "bogus"] {
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
