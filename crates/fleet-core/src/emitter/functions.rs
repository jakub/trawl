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
