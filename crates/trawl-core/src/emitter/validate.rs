// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pre-emission pipeline validation.
//!
//! Catches malformed regexes, unknown function names, arity mismatches,
//! and `level` used as if it were a column, before any SQL emission state
//! is mutated. This gives cleaner error reporting and avoids
//! partially-built CTEs on failure.

use crate::ast::{AggExpr, Expr, ExtractMode, PipeStage, Spanned};

use super::EmitError;
use super::functions::validate_function_arity;
use super::severity::{as_level_comparison, reject_level_field};

/// Validate all pipe stages before emission begins.
///
/// Checks function names, argument counts, regex validity, and `level`
/// references. Error messages are intentionally identical to those
/// produced during emission so that callers see consistent diagnostics
/// regardless of which layer catches the problem.
///
/// This runs over the *whole* pipeline, including the stages the executor
/// later peels off to run in Rust — so a `level` reference is rejected the
/// same way whether it ends up in SQL or in the post-SQL streaming engine.
pub fn validate_pipeline(stages: &[Spanned<PipeStage>]) -> Result<(), EmitError> {
    for (i, stage) in stages.iter().enumerate() {
        validate_level_references(&stage.node)?;
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

/// Reject every `level` reference a stage makes outside a comparison.
///
/// Covers both halves of a stage: the field names it writes into SQL
/// verbatim (group-by keys, projections, sort keys, rename sides,
/// aggregation aliases) and the expressions it carries. A *written*
/// `level` — an alias, a rename target, a `let` binding — is rejected
/// alongside a read one: a column by that name would shadow the severity
/// alias for the rest of the pipeline, so `level` is simply not a usable
/// column name. Search-stage `level` filters never reach here; the emitter
/// turns those into band predicates.
fn validate_level_references(stage: &PipeStage) -> Result<(), EmitError> {
    match stage {
        PipeStage::Stats(s) => validate_grouped_aggs(&s.group_by, &s.aggregations),
        PipeStage::EventStats(s) => validate_grouped_aggs(&s.group_by, &s.aggregations),
        PipeStage::Timechart(t) => validate_grouped_aggs(&t.group_by, &t.aggregations),
        PipeStage::Pivot(p) => {
            reject_each(std::iter::once(p.on_field.as_str()).chain(strs(&p.by)))?;
            validate_agg_level(&p.aggregation)
        }
        PipeStage::Sort(s) => reject_each(s.fields.iter().map(|f| f.field.as_str())),
        PipeStage::Table(t) => reject_each(strs(&t.fields)),
        PipeStage::Drop(d) => reject_each(strs(&d.fields)),
        PipeStage::Dedup(d) => reject_each(strs(&d.fields)),
        PipeStage::Top(t) => reject_each(std::iter::once(t.field.as_str()).chain(strs(&t.by))),
        PipeStage::Rare(r) => reject_each(std::iter::once(r.field.as_str()).chain(strs(&r.by))),
        PipeStage::Rename(r) => reject_each(
            r.renames
                .iter()
                .flat_map(|(from, to)| [from.as_str(), to.as_str()]),
        ),
        PipeStage::Let(l) => l.assignments.iter().try_for_each(|(name, expr)| {
            reject_level_field(name)?;
            validate_expr_fields(expr)
        }),
        PipeStage::Where(w) => validate_expr_fields(&w.condition),
        PipeStage::Extract(e) => reject_each(e.source_field.as_deref()),
        PipeStage::Limit(_)
        | PipeStage::Tail(_)
        | PipeStage::Sample(_)
        | PipeStage::FromSaved(_) => Ok(()),
    }
}

fn reject_each<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<(), EmitError> {
    names.into_iter().try_for_each(reject_level_field)
}

fn strs(fields: &[String]) -> impl Iterator<Item = &str> {
    fields.iter().map(String::as_str)
}

fn validate_grouped_aggs(group_by: &[String], aggs: &[AggExpr]) -> Result<(), EmitError> {
    reject_each(group_by.iter().map(String::as_str))?;
    aggs.iter().try_for_each(validate_agg_level)
}

fn validate_agg_level(agg: &AggExpr) -> Result<(), EmitError> {
    reject_each(agg.alias.as_deref())?;
    agg.args.iter().try_for_each(validate_expr_fields)
}

/// Walk an expression, rejecting `level` field references.
///
/// A `level` comparison against a severity token is the one legal use, so
/// that subtree is left alone — [`as_level_comparison`] is the same
/// arbiter the emitter and the streaming plan compiler consult, so all
/// three agree on what counts as a comparison.
fn validate_expr_fields(expr: &Spanned<Expr>) -> Result<(), EmitError> {
    match &expr.node {
        Expr::FieldRef(name) => reject_level_field(name),
        Expr::Binary { lhs, op, rhs } => {
            if as_level_comparison(lhs, *op, rhs).is_some() {
                return Ok(());
            }
            validate_expr_fields(lhs)?;
            validate_expr_fields(rhs)
        }
        Expr::Unary { operand, .. } => validate_expr_fields(operand),
        Expr::FunctionCall { args, .. } => args.iter().try_for_each(validate_expr_fields),
        Expr::InList { expr: target, list } => {
            validate_expr_fields(target)?;
            list.iter().try_for_each(validate_expr_fields)
        }
        Expr::Literal(_) => Ok(()),
    }
}

/// Validate a function call: name must be known, arity must match.
fn validate_function(agg: &AggExpr) -> Result<(), EmitError> {
    validate_function_arity(agg.function.as_str(), agg.args.len())
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
        ExtractMode::KeyValue { .. } => {
            // kv extraction is handled post-SQL by the Rust pipeline
        }
    }
    Ok(())
}
