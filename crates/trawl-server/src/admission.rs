// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The one DSL admission door every entry point that accepts query text
//! goes through (ADR-0024).
//!
//! `trawl-core` owns the verdict: [`trawl_core::parser::parse`] decides
//! whether the text is a query at all, and
//! [`trawl_core::emitter::validate_pipeline`] runs the shared pipeline
//! validation whose first statement is the bind-time expansion budget. So
//! an over-budget query is refused with the same sentence wherever it
//! arrives, and a query the engine would refuse to emit is refused before
//! it is stored, scheduled or handed a permit.
//!
//! Two properties matter more than the saved parse:
//!
//! * It runs over the WHOLE pipeline, ahead of anything that rewrites it.
//!   `/query` calls it before `try_resolve_from_saved` slices the
//!   `from saved` stage off, so a pipeline over the stage cap cannot
//!   become an under-cap suffix by being resolved.
//! * It runs before the store call on saved create/update, so the pipeline
//!   that fails admission is never persisted to be run later by a
//!   schedule. The scheduler asks again anyway, because a query stored
//!   before this door existed is still out there.
//!
//! The errors are the ones the engine's own lanes raise for the same text,
//! so [`crate::error`]'s existing mapping decides the status and body: a
//! parse failure is `EngineError::Parse` (400, one detail per parse error)
//! and a validation failure is `EngineError::Emit` (400, the sentence).
//! Nothing here formats a message of its own.

use trawl_engine::error::EngineError;

use crate::error::ServerError;

/// Admit one DSL query.
///
/// # Errors
///
/// [`ServerError::Engine`] carrying [`EngineError::Parse`] when the text is
/// not a query, or [`EngineError::Emit`] when the pipeline fails shared
/// validation — including the ADR-0024 refusal for a pipeline over the
/// alias-expansion budget or the stage cap.
pub fn check_dsl(dsl: &str) -> Result<(), ServerError> {
    let ast = trawl_core::parser::parse(dsl)
        .map_err(|errors| ServerError::Engine(EngineError::Parse(errors)))?;
    trawl_core::emitter::validate_pipeline(&ast.pipeline)
        .map_err(|e| ServerError::Engine(EngineError::Emit(e)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::check_dsl;

    /// A doubling chain: every assignment names the previous one twice, so
    /// each link doubles the work the binder does, and the budget refuses
    /// it long before the text gets long.
    fn doubling_chain(links: usize) -> String {
        use std::fmt::Write as _;
        let mut dsl = String::from("* | let a0 = 1");
        for i in 1..=links {
            let _ = write!(dsl, ", a{i} = a{} + a{}", i - 1, i - 1);
        }
        dsl
    }

    #[test]
    fn an_ordinary_query_is_admitted() {
        check_dsl("service=nginx last=1h | stats count() by host").expect("ordinary DSL admitted");
    }

    #[test]
    fn unparseable_text_is_a_parse_error() {
        let err = check_dsl("| | | broken {{{").expect_err("refused");
        assert!(
            matches!(
                err,
                crate::error::ServerError::Engine(trawl_engine::error::EngineError::Parse(_))
            ),
            "{err:?}"
        );
    }

    /// The refusal carries `trawl-core`'s sentence, through the emit
    /// error the engine's own lanes would have raised.
    #[test]
    fn an_over_budget_pipeline_carries_the_shared_sentence() {
        let err = check_dsl(&doubling_chain(12)).expect_err("refused");
        assert!(
            matches!(
                err,
                crate::error::ServerError::Engine(trawl_engine::error::EngineError::Emit(_))
            ),
            "{err:?}"
        );
        let text = err.to_string();
        assert!(text.contains("alias-expansion budget"), "{text}");
        assert!(
            text.contains(&trawl_core::complexity::MAX_LATERAL_EXPANSION.to_string()),
            "{text}"
        );
    }

    #[test]
    fn a_pipeline_over_the_stage_cap_is_refused() {
        let stages = trawl_core::complexity::MAX_PIPELINE_STAGES + 1;
        let dsl = format!("*{}", " | head 10".repeat(stages));
        let err = check_dsl(&dsl).expect_err("refused");
        let text = err.to_string();
        assert!(text.contains("pipeline stages"), "{text}");
    }
}
