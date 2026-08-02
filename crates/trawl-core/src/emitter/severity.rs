// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `level` → `severity` band predicate emission (ADR-0009).
//!
//! `level` is a DSL alias for the numeric `severity` column: `level=error`
//! compiles to `severity BETWEEN 17 AND 20` (the ERROR band) and
//! `level>=warn` to `severity >= 13` (the token's exact number). Glob and
//! regex patterns — and unknown tokens — are emit-time errors pointing at
//! `severity_text`, which still matches the original text literally.

use crate::ast::FilterOp;
use crate::severity;

use super::EmitError;

/// The DSL field name that aliases the numeric `severity` column.
pub(crate) const LEVEL_FIELD: &str = "level";

fn unknown_token_error(token: &str) -> EmitError {
    EmitError::UnsupportedOperation {
        message: format!(
            "unknown severity token '{token}' for level= (valid tokens: {}); \
             to match the original text use severity_text instead",
            severity::CANONICAL_TOKENS.join(", ")
        ),
    }
}

fn pattern_error() -> EmitError {
    EmitError::UnsupportedOperation {
        message: format!(
            "level= does not support glob or regex patterns — it maps severity \
             tokens ({}) onto the numeric severity column; to pattern-match the \
             original text use severity_text instead",
            severity::CANONICAL_TOKENS.join(", ")
        ),
    }
}

/// Resolve a token to its number and containing band, or an emit error.
fn resolve(token: &str) -> Result<(u8, (u8, u8)), EmitError> {
    let number = severity::number_for_token(token).ok_or_else(|| unknown_token_error(token))?;
    let band = severity::band_of(number).expect("table numbers are in-ladder");
    Ok((number, band))
}

/// SQL predicate on `severity` for a single `level` comparison.
///
/// Eq → BETWEEN over the band containing the token's number; Ne → NOT
/// BETWEEN (with NULL included, mirroring `!=` on ordinary fields);
/// ordered comparisons → against the token's exact number.
pub(crate) fn level_predicate(op: FilterOp, token: &str) -> Result<String, EmitError> {
    let (number, (lo, hi)) = resolve(token)?;
    Ok(match op {
        FilterOp::Eq => format!("\"severity\" BETWEEN {lo} AND {hi}"),
        FilterOp::Ne => {
            format!("(\"severity\" NOT BETWEEN {lo} AND {hi} OR \"severity\" IS NULL)")
        }
        FilterOp::Gt => format!("\"severity\" > {number}"),
        FilterOp::Gte => format!("\"severity\" >= {number}"),
        FilterOp::Lt => format!("\"severity\" < {number}"),
        FilterOp::Lte => format!("\"severity\" <= {number}"),
        FilterOp::Glob | FilterOp::Regex => return Err(pattern_error()),
    })
}

/// SQL predicate for `level=a,b,...` — OR of the tokens' band ranges.
pub(crate) fn level_in_list(tokens: &[String]) -> Result<String, EmitError> {
    let mut parts = Vec::with_capacity(tokens.len());
    for token in tokens {
        let (_, (lo, hi)) = resolve(token)?;
        parts.push(format!("\"severity\" BETWEEN {lo} AND {hi}"));
    }
    Ok(format!("({})", parts.join(" OR ")))
}
