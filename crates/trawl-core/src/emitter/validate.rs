// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pre-emission pipeline validation.
//!
//! Refuses, before any emission state is mutated, a query whose answer
//! the lanes could not agree on: an over-budget expansion, a misplaced
//! aggregate, a call that breaks its function's rule (name, argument
//! count, literal arguments), a reserved or colliding output name, a
//! malformed `extract` regex, and a `from saved` that is not first. A
//! failure reports one clean error instead of leaving half-built CTEs
//! behind. Field names are not checked against any schema.

use crate::ast::{AggExpr, ExtractMode, PipeStage, Spanned};
use crate::pattern::price::{self, Capture};

use super::EmitError;

/// Validate all pipe stages before emission begins.
///
/// In this order, each over the whole pipeline before the next starts:
/// the bind-time expansion budget, aggregate positions, then every call
/// (its name, argument count, unit, format and integer-literal
/// arguments). Then, stage by stage: aggregation aliases, `extract` regex
/// syntax and capture names, the place of `from saved`, and projection
/// collisions. Error messages are the ones emission produces, so callers
/// see the same diagnostic whichever layer catches the problem.
///
/// This runs over the *whole* pipeline, including the stages the executor
/// later peels off to run in Rust — so a bad call or an `extract` capture
/// group naming a reserved column is rejected the same way whether it
/// ends up in SQL or in the post-SQL streaming engine.
pub fn validate_pipeline(stages: &[Spanned<PipeStage>]) -> Result<(), EmitError> {
    // The bind-time expansion budget first, over the whole pipeline
    // (ADR-0024): a query that would make `DuckDB` bind an exponential
    // tree must be refused before anything else looks at it, and the
    // stream compiler runs the same check at the head of its own door so
    // both lanes carry one sentence.
    crate::complexity::check_pipeline_complexity(stages).map_err(|refusal| {
        EmitError::UnsupportedOperation {
            message: refusal.to_string(),
        }
    })?;
    // Then an aggregate written where neither lane can run it (ADR-0001,
    // Amendment 2026-11-08). The stream compiler runs it at the same
    // point, so a query with several defects gets one first sentence from
    // both lanes.
    crate::aggregate::check_positions(stages)?;
    // Then every call, in every stage, the stages after `extract kv`
    // included (ADR-0001, Amendment 2026-11-11).
    crate::calls::check_calls(stages)?;

    for (i, stage) in stages.iter().enumerate() {
        match &stage.node {
            PipeStage::Stats(s) => {
                for agg in &s.aggregations {
                    validate_alias(agg)?;
                }
            }
            PipeStage::Timechart(tc) => {
                for agg in &tc.aggregations {
                    validate_alias(agg)?;
                }
            }
            PipeStage::EventStats(es) => {
                for agg in &es.aggregations {
                    validate_alias(agg)?;
                }
            }
            PipeStage::Pivot(p) => {
                validate_alias(&p.aggregation)?;
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

        // Every projecting stage's output-name set, from the one
        // lane-neutral check the stream compiler runs too (ADR-0013
        // ruling 8) — after the per-stage checks above, so a reserved
        // alias keeps its more specific sentence.
        // Non-projecting stages are a no-op there.
        crate::projection::check_projection(&stage.node)
            .map_err(|message| EmitError::InvalidAggregation { message })?;
    }
    Ok(())
}

/// An aggregation's alias — the fourth pipeline write position, beside
/// `let`/`rename` targets and `extract` capture groups — may not mint a
/// reserved name (ADR-0013 §5).
fn validate_alias(agg: &AggExpr) -> Result<(), EmitError> {
    if let Some(alias) = &agg.alias
        && crate::schema::is_reserved_name(alias)
    {
        return Err(reserved_name_error("aggregation alias", alias));
    }
    Ok(())
}

/// Validate extract stage: the regex must parse and have named groups.
fn validate_extract(extract: &crate::ast::ExtractStage) -> Result<(), EmitError> {
    match &extract.mode {
        ExtractMode::Regex(pattern) => {
            let captures = extract_captures(pattern)?;
            // The pipeline may not mint a reserved name (ADR-0013 §5):
            // ingest strips the `_` prefix off an incoming key, so a
            // capture group that wrote one would be a column the DSL can
            // create and ingest can never carry. The stream compiler does
            // not call `validate_pipeline`, so it mirrors this refusal where
            // it compiles the regex (`stream::compile_extract`), the same
            // way the kv arm seals its keys in both lanes.
            for capture in &captures {
                if crate::schema::is_reserved_name(&capture.name) {
                    return Err(reserved_name_error("extract capture group", &capture.name));
                }
            }
            if let Some(message) = crate::schema::duplicate_target_message(
                captures.iter().map(|capture| capture.name.as_str()),
                "extract",
            ) {
                return Err(EmitError::UnsupportedOperation { message });
            }
        }
        ExtractMode::KeyValue { .. } => {
            // kv extraction is handled post-SQL by the Rust pipeline
        }
    }
    Ok(())
}

/// The named capture groups of an `extract` body, each with its absolute
/// group index: what validation reads, and what emission checks before it
/// rewrites the body ([`batch_extract`]).
///
/// They come from the regex syntax tree (ADR-0063), so reading them
/// builds no regex and translates nothing. The parser validates the
/// whole body before a query gets here; this refuses only what the
/// emitter cannot write, a body that does not parse or that names no
/// group.
fn extract_captures(pattern: &str) -> Result<Vec<Capture>, EmitError> {
    let captures = price::captures(pattern).map_err(|e| EmitError::UnsupportedOperation {
        message: format!("invalid regex in extract: {e}"),
    })?;
    if captures.is_empty() {
        return Err(EmitError::UnsupportedOperation {
            message: "extract regex must contain at least one named capture group (?P<name>...)"
                .to_string(),
        });
    }
    Ok(captures)
}

/// An `extract` body as the batch lane binds it, its unnamed groups made
/// non-capturing and its named groups numbered among themselves
/// ([`crate::pattern::batch_extract`]). Refuses what
/// [`extract_captures`] refuses.
pub(crate) fn batch_extract(pattern: &str) -> Result<crate::pattern::BatchExtract, EmitError> {
    extract_captures(pattern)?;
    crate::pattern::batch_extract(pattern).map_err(|e| EmitError::UnsupportedOperation {
        message: format!("invalid regex in extract: {e}"),
    })
}

/// Wrap [`crate::schema::reserved_name_message`] as an [`EmitError`].
///
/// Two write positions reach it here: an aggregate alias and an `extract`
/// capture group. `let`/`rename` targets are refused earlier by
/// `parser::pipe::assignment_target`, and the stream lane takes the same
/// message text straight from `schema`.
pub(crate) fn reserved_name_error(what: &str, name: &str) -> EmitError {
    EmitError::UnsupportedOperation {
        message: crate::schema::reserved_name_message(what, name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{EventStatsStage, PivotStage, StatsStage, TimechartStage};

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

    /// The aggregate alias is a pipeline write position, so the emitter
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
                on: None,
                span: None,
                aggregations: vec![agg("_time")],
                group_by: vec![],
            })),
            stage(PipeStage::Pivot(PivotStage {
                aggregation: agg("_raw"),
                on_field: "status".to_string(),
                by: vec![],
            })),
            stage(PipeStage::EventStats(EventStatsStage {
                aggregations: vec![agg("_level")],
                group_by: vec![],
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

    /// An `eventstats` slot gets the name and arity check every other
    /// aggregation slot gets.
    #[test]
    fn eventstats_slots_are_checked() {
        for (dsl, sentence) in [
            ("* | eventstats bogus(x) as y", "unknown function: bogus"),
            (
                "* | eventstats avg() as y",
                "invalid aggregation: avg() requires exactly one argument",
            ),
        ] {
            let query = crate::parser::parse(dsl).expect("dsl parses");
            let err = validate_pipeline(&query.pipeline).expect_err("refused");
            assert_eq!(err.message(), sentence, "{dsl}");
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
