// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pre-emission pipeline validation.
//!
//! Catches malformed regexes, unknown function names, arity mismatches,
//! and reserved-namespace capture-group names, before any SQL emission
//! state is mutated. This gives cleaner error reporting and avoids
//! partially-built CTEs on failure.

use crate::ast::{AggExpr, ExtractMode, PipeStage, Spanned};

use super::EmitError;
use super::functions::validate_function_arity;

/// Validate all pipe stages before emission begins.
///
/// Checks function names, argument counts and regex validity. Error
/// messages are intentionally identical to those produced during
/// emission so that callers see consistent diagnostics regardless of
/// which layer catches the problem.
///
/// This runs over the *whole* pipeline, including the stages the executor
/// later peels off to run in Rust — so an `extract` capture group naming
/// a reserved column is rejected the same way whether it ends up in SQL
/// or in the post-SQL streaming engine.
pub fn validate_pipeline(stages: &[Spanned<PipeStage>]) -> Result<(), EmitError> {
    for (i, stage) in stages.iter().enumerate() {
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
            PipeStage::FromSaved(_) if i > 0 => {
                return Err(EmitError::UnsupportedOperation {
                    message: "'from saved' must be the first pipe stage".to_string(),
                });
            }
            // other stages have no pre-validation needs
            _ => {}
        }
    }
    Ok(())
}

/// Validate a function call: name must be known, arity must match, and
/// its alias — the fourth pipeline write position, beside `let`/`rename`
/// targets and `extract` capture groups — may not MINT a reserved name
/// (ADR-0013 §5).
fn validate_function(agg: &AggExpr) -> Result<(), EmitError> {
    validate_function_arity(agg.function.as_str(), agg.args.len())?;
    if let Some(alias) = &agg.alias
        && crate::schema::is_reserved_name(alias)
    {
        return Err(reserved_name_error("aggregation alias", alias));
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
            // The pipeline may not MINT a reserved name (ADR-0013 §5):
            // ingest strips the `_` prefix off an incoming key, so a
            // capture group that wrote one would be a column the DSL can
            // create and ingest can never carry. This is the one place
            // the regex is already compiled, and it is reached by BOTH
            // lanes through `validate_pipeline`.
            for name in re.capture_names().flatten() {
                if crate::schema::is_reserved_name(name) {
                    return Err(reserved_name_error("extract capture group", name));
                }
            }
        }
        ExtractMode::KeyValue { .. } => {
            // kv extraction is handled post-SQL by the Rust pipeline
        }
    }
    Ok(())
}

/// The one refusal both pipeline doors share: an assignment target,
/// rename target or capture group in trawl's `_` namespace.
pub(crate) fn reserved_name_error(what: &str, name: &str) -> EmitError {
    EmitError::UnsupportedOperation {
        message: format!(
            "{what} '{name}' is in trawl's reserved namespace — names \
             starting with '_' are trawl's contract slots and only trawl \
             writes them (ADR-0013); choose a name without the underscore"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{PivotStage, StatsStage, TimechartStage};

    fn agg(alias: &str) -> AggExpr {
        AggExpr {
            function: "count".to_string(),
            args: vec![],
            alias: Some(alias.to_string()),
        }
    }

    fn stage(node: PipeStage) -> Spanned<PipeStage> {
        Spanned::new(node, 0..0)
    }

    /// The aggregate alias is a pipeline WRITE position, so the emitter
    /// door seals it exactly as the parser door does (ADR-0013 §5) — a
    /// hand-built AST cannot mint a reserved column either.
    #[test]
    fn aggregation_alias_cannot_mint_a_reserved_name() {
        let stages = [
            stage(PipeStage::Stats(StatsStage {
                aggregations: vec![agg("_severity")],
                group_by: vec!["service".to_string()],
            })),
            stage(PipeStage::Timechart(TimechartStage {
                span: None,
                aggregations: vec![agg("_time")],
                group_by: vec![],
            })),
            stage(PipeStage::Pivot(PivotStage {
                aggregation: agg("_raw"),
                on_field: "status".to_string(),
                by: vec![],
            })),
        ];
        for s in stages {
            let err = validate_pipeline(std::slice::from_ref(&s))
                .expect_err("reserved alias must be refused");
            assert!(
                err.to_string().contains("reserved namespace"),
                "{err} ({s:?})"
            );
        }
    }

    #[test]
    fn ordinary_aggregation_alias_is_accepted() {
        let s = stage(PipeStage::Stats(StatsStage {
            aggregations: vec![agg("total")],
            group_by: vec![],
        }));
        validate_pipeline(std::slice::from_ref(&s)).expect("plain alias must validate");
    }
}
