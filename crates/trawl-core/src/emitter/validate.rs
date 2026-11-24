// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::ast::{AggExpr, ExtractMode, PipeStage, Spanned};
use crate::pattern::price::{self, Capture};

use super::EmitError;

pub fn validate_pipeline(stages: &[Spanned<PipeStage>]) -> Result<(), EmitError> {
    crate::complexity::check_pipeline_complexity(stages).map_err(|refusal| {
        EmitError::UnsupportedOperation {
            message: refusal.to_string(),
        }
    })?;
    crate::aggregate::check_positions(stages)?;
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
            _ => {}
        }

        // After the per-stage checks above, so a reserved alias keeps its more
        // specific sentence.
        crate::projection::check_projection(&stage.node)
            .map_err(|message| EmitError::InvalidAggregation { message })?;
    }
    Ok(())
}

fn validate_alias(agg: &AggExpr) -> Result<(), EmitError> {
    if let Some(alias) = &agg.alias
        && crate::schema::is_reserved_name(alias)
    {
        return Err(reserved_name_error("aggregation alias", alias));
    }
    Ok(())
}

fn validate_extract(extract: &crate::ast::ExtractStage) -> Result<(), EmitError> {
    match &extract.mode {
        ExtractMode::Regex(pattern) => {
            let captures = extract_captures(pattern)?;
            // The stream compiler does not call `validate_pipeline`, so it mirrors this
            // refusal where it compiles the regex (`stream::compile_extract`).
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

pub(crate) fn batch_extract(pattern: &str) -> Result<crate::pattern::BatchExtract, EmitError> {
    extract_captures(pattern)?;
    crate::pattern::batch_extract(pattern).map_err(|e| EmitError::UnsupportedOperation {
        message: format!("invalid regex in extract: {e}"),
    })
}

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
