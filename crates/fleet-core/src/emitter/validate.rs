//! Pre-emission pipeline validation.
//!
//! Catches malformed regexes, unknown function names, and arity
//! mismatches before any SQL emission state is mutated. This gives
//! cleaner error reporting and avoids partially-built CTEs on failure.

use crate::ast::{AggExpr, ExtractMode, PipeStage, Spanned};

use super::EmitError;

/// Known function names accepted by the emitter.
const KNOWN_FUNCTIONS: &[&str] = &[
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

/// Validate all pipe stages before emission begins.
///
/// Checks function names, argument counts, and regex validity. Error
/// messages are intentionally identical to those produced during emission
/// so that callers see consistent diagnostics regardless of which layer
/// catches the problem.
pub fn validate_pipeline(stages: &[Spanned<PipeStage>]) -> Result<(), EmitError> {
    for stage in stages {
        match &stage.node {
            PipeStage::Stats(s) => {
                for agg in &s.aggregations {
                    validate_function(agg)?;
                }
            }
            PipeStage::Timechart(tc) => {
                for agg in &tc.aggregations {
                    validate_function(agg)?;
                }
            }
            PipeStage::Pivot(p) => {
                validate_function(&p.aggregation)?;
            }
            PipeStage::Extract(e) => {
                validate_extract(e)?;
            }
            // other stages have no pre-validation needs
            _ => {}
        }
    }
    Ok(())
}

/// Validate a function call: name must be known, arity must match.
fn validate_function(agg: &AggExpr) -> Result<(), EmitError> {
    let name = agg.function.as_str();

    if !KNOWN_FUNCTIONS.contains(&name) {
        return Err(EmitError::UnknownFunction {
            name: name.to_string(),
        });
    }

    let argc = agg.args.len();
    match name {
        // coalesce() requires at least 1 arg
        "coalesce" if argc == 0 => {
            return Err(EmitError::InvalidAggregation {
                message: "coalesce() requires at least one argument".to_string(),
            });
        }
        // count() accepts 0 or 1 args; coalesce() accepts 1+
        // now() takes 0 args
        "now" if argc != 0 => {
            return Err(EmitError::InvalidAggregation {
                message: format!("{name}() requires exactly 0 argument(s)"),
            });
        }
        // 3-arg functions
        "if" | "replace" if argc != 3 => {
            return Err(EmitError::InvalidAggregation {
                message: format!("{name}() requires exactly 3 argument(s)"),
            });
        }
        // 2-3 arg functions
        "substr" if !(2..=3).contains(&argc) => {
            return Err(EmitError::InvalidAggregation {
                message: format!("{name}() requires 2 to 3 arguments"),
            });
        }
        // 1-2 arg functions
        "round" if !(1..=2).contains(&argc) => {
            return Err(EmitError::InvalidAggregation {
                message: format!("{name}() requires 1 to 2 arguments"),
            });
        }
        "count" | "coalesce" | "now" | "if" | "replace" | "substr" | "round" => {}
        // everything else requires exactly 1 arg
        _ if argc != 1 => {
            return Err(EmitError::InvalidAggregation {
                message: format!("{name}() requires exactly one argument"),
            });
        }
        _ => {}
    }

    Ok(())
}

/// Validate extract stage: regex must compile and have named groups.
fn validate_extract(extract: &crate::ast::ExtractStage) -> Result<(), EmitError> {
    match &extract.mode {
        ExtractMode::Regex(pattern) => {
            let re = regex::Regex::new(pattern).map_err(|e| EmitError::UnsupportedOperation {
                message: format!("invalid regex in extract: {e}"),
            })?;

            let has_named_groups = re.capture_names().flatten().next().is_some();
            if !has_named_groups {
                return Err(EmitError::UnsupportedOperation {
                    message: "extract regex must contain at least one named capture group \
                              (?P<name>...)"
                        .to_string(),
                });
            }
        }
        ExtractMode::KeyValue => {
            return Err(EmitError::UnsupportedOperation {
                message: "extract kv is not yet implemented".to_string(),
            });
        }
    }
    Ok(())
}
