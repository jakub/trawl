// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pin-aware comparison SQL renderers (ADR-0011).

use crate::ast::FilterOp;
use crate::compare::{self, CompareForm, PatternForm};
use crate::conform;

use super::SqlValue;
use super::state::EmitterState;

/// What a NULL column answers under `!=` in one lane.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NullPolicy {
    NeMatchesNull,
    /// Pipeline rule: plain SQL three-valued logic — NULL is UNKNOWN and
    /// the row is filtered, for `!=` like every other operator.
    Strict,
}

/// The text a glob/regex matches against under the pin. The live side
/// mirrors it exactly (`crate::pin_match`).
pub(crate) fn pattern_target(field: &str, pin: Option<crate::schema::CanonicalType>) -> String {
    match compare::pattern_form(pin) {
        PatternForm::Native => field.to_owned(),
        PatternForm::BigIntText | PatternForm::BooleanText | PatternForm::DoubleText => {
            format!("CAST({field} AS VARCHAR)")
        }
        PatternForm::Rfc3339Text => {
            format!(
                "strftime({field}, '{}')",
                compare::TIMESTAMP_PATTERN_SQL_FORMAT
            )
        }
        PatternForm::SeverityText => conform::severity_token_text_sql(field),
    }
}

pub(crate) fn comparison_sql(
    field: &str,
    op: FilterOp,
    form: CompareForm,
    policy: NullPolicy,
    state: &mut EmitterState,
) -> String {
    let sql_op = filter_op_to_sql(op);
    match form {
        CompareForm::NumericOnText(literal) => {
            let placeholder = state.push_param(SqlValue::String(literal));
            format!(
                "{} {sql_op} {}",
                conform::decimal_reading(field),
                conform::decimal_reading(&placeholder)
            )
        }
        // VARCHAR pin + equality-class numeric literal: the exact text OR
        // the column's numeric reading, because the stored text of a
        // number is `read_json`'s inference rendered (`"200.0"`), not the
        // wire spelling the live matcher sees (`crate::compare`).
        CompareForm::TextOrNumeric(literal) => {
            let predicate = text_or_numeric(field, op, literal, state);
            if op == FilterOp::Ne && policy == NullPolicy::NeMatchesNull {
                format!("({predicate} OR {field} IS NULL)")
            } else {
                predicate
            }
        }
        CompareForm::SeverityBand { .. } => {
            let points = compare::severity_points(std::slice::from_ref(&form))
                .expect("a lone SeverityBand always expands");
            let predicate = severity_ranges_sql(field, op, &points);
            if op == FilterOp::Ne && policy == NullPolicy::NeMatchesNull {
                format!("({predicate} OR {field} IS NULL)")
            } else {
                predicate
            }
        }
        form => {
            let val = comparable_value(form);
            let placeholder = state.push_param(val);
            if op == FilterOp::Ne && policy == NullPolicy::NeMatchesNull {
                format!("({field} {sql_op} {placeholder} OR {field} IS NULL)")
            } else {
                format!("{field} {sql_op} {placeholder}")
            }
        }
    }
}

/// A `TextOrNumeric` element has no single bound value, so a list carrying
/// one expands to the OR of its per-element equalities: the same shape the
/// live matcher evaluates (its `InList` is an OR of `=` comparisons).
pub(crate) fn in_list_sql(
    field: &str,
    forms: Vec<CompareForm>,
    state: &mut EmitterState,
) -> String {
    if let Some(points) = compare::severity_points(&forms) {
        return severity_ranges_sql(field, FilterOp::Eq, &points);
    }
    // The forms with no single bound value: the VARCHAR pin's two-armed
    // equality, and the SEVERITY pin's band range (reachable here only
    // mixed with a non-severity element, which no pin can produce — the
    // arm stays total rather than trusting that).
    let expands = |f: &CompareForm| {
        matches!(
            f,
            CompareForm::TextOrNumeric(_) | CompareForm::SeverityBand { .. }
        )
    };
    if forms.iter().any(expands) {
        let predicates: Vec<String> = forms
            .into_iter()
            .map(|form| match form {
                CompareForm::TextOrNumeric(literal) => {
                    text_or_numeric(field, FilterOp::Eq, literal, state)
                }
                form @ CompareForm::SeverityBand { .. } => {
                    let points = compare::severity_points(std::slice::from_ref(&form))
                        .expect("a lone SeverityBand always expands");
                    severity_ranges_sql(field, FilterOp::Eq, &points)
                }
                other => {
                    let placeholder = state.push_param(comparable_value(other));
                    format!("{field} = {placeholder}")
                }
            })
            .collect();
        format!("({})", predicates.join(" OR "))
    } else {
        let placeholders: Vec<String> = forms
            .into_iter()
            .map(|form| state.push_param(comparable_value(form)))
            .collect();
        format!("{field} IN ({})", placeholders.join(", "))
    }
}

/// Both `COALESCE`s are load-bearing: without them a value with no numeric
/// reading (`'accepted'`) makes the whole predicate UNKNOWN, so the row
/// would fail both `status=200` and its `NOT`, and `status!=200` would
/// stop returning it.
fn text_or_numeric(field: &str, op: FilterOp, literal: String, state: &mut EmitterState) -> String {
    let text_param = state.push_param(SqlValue::String(literal.clone()));
    let num_param = state.push_param(SqlValue::String(literal));
    let column = conform::decimal_reading(field);
    let number = conform::decimal_reading(&num_param);
    if op == FilterOp::Ne {
        format!("({field} != {text_param} AND COALESCE({column} != {number}, TRUE))")
    } else {
        format!("({field} = {text_param} OR COALESCE({column} = {number}, FALSE))")
    }
}

/// Ranges and not the point set, because an `IN` list over a computed
/// left-hand side leaves `DuckDB`'s fast path: over 1M rows a single band
/// costs ~392 ms as `IN (17, 18, 19, 20)` against ~6 ms as `BETWEEN 17 AND 20`
/// (`trawl-engine/tests/severity_set_bench.rs`).
///
/// The subject must not carry bound placeholders: a multi-run set repeats
/// the subject, while the emitter pushes one parameter per `push_param`
/// call and not per occurrence, so a subject containing `?` would
/// desynchronize every parameter after it.
/// `severity_subjects_never_push_parameters` asserts that end to end.
/// **Anyone widening `PinnedSubject` must revisit this**.
fn severity_ranges_sql(subject: &str, op: FilterOp, points: &[i64]) -> String {
    debug_assert!(
        matches!(op, FilterOp::Eq | FilterOp::Ne),
        "the severity set is an equality-class shape"
    );
    debug_assert!(!points.is_empty(), "an empty set renders no predicate");
    let ranges = compare::severity_ranges(points);
    let positive = ranges
        .iter()
        .map(|(lo, hi)| {
            if lo == hi {
                format!("{subject} = {lo}")
            } else {
                format!("{subject} BETWEEN {lo} AND {hi}")
            }
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    let positive = if ranges.len() > 1 {
        format!("({positive})")
    } else {
        positive
    };
    if op == FilterOp::Ne {
        format!("NOT ({positive})")
    } else {
        positive
    }
}

fn comparable_value(form: CompareForm) -> SqlValue {
    match form {
        CompareForm::Native(val) | CompareForm::Conformed { literal: val, .. } => val,
        CompareForm::Text(s) => SqlValue::String(s),
        CompareForm::SeverityExact(n) => SqlValue::Int(n),
        CompareForm::NumericOnText(_)
        | CompareForm::TextOrNumeric(_)
        | CompareForm::SeverityBand { .. } => {
            debug_assert!(
                false,
                "the pinned numeric forms have their own emission shape"
            );
            SqlValue::String(String::new())
        }
    }
}

pub(crate) fn filter_op_to_sql(op: FilterOp) -> &'static str {
    match op {
        FilterOp::Eq => "=",
        FilterOp::Ne => "!=",
        FilterOp::Gt => ">",
        FilterOp::Gte => ">=",
        FilterOp::Lt => "<",
        FilterOp::Lte => "<=",
    }
}
