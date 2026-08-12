// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pin-aware comparison SQL renderers (ADR-0011 slices A/A′).
//!
//! One set of renderers, two lanes: the search stage
//! ([`super::search::emit_field_filter`]) and the pipeline expression
//! emitter ([`super::expr`])'s pinned-comparison arm both render a resolved
//! [`CompareForm`] through here, so a `status>=400` means the same SQL in
//! `status>=400 | …` and in `… | where status >= 400`.
//!
//! The one deliberate difference between the lanes is the NULL rule for
//! `!=`, carried as [`NullPolicy`]: the search stage's `!=` includes NULL
//! columns (`… OR field IS NULL` — the documented ADR-0011 slice-A
//! behavior), while a pipeline `where` keeps plain SQL null propagation —
//! a NULL column is UNKNOWN and filtered, exactly what the pin-blind
//! `where` has always answered. Importing the search widening into the
//! pipeline would make a repin change missing-field semantics, which is
//! precisely what slice A′ forbids.

use crate::ast::FilterOp;
use crate::compare::{self, CompareForm, PatternForm};
use crate::conform;

use super::SqlValue;
use super::state::EmitterState;

/// What a NULL column answers under `!=` in one lane.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NullPolicy {
    /// Search-stage rule: `field != x` matches a NULL column too — the
    /// emitted shape carries `OR field IS NULL`.
    NeMatchesNull,
    /// Pipeline rule: plain SQL three-valued logic — NULL is UNKNOWN and
    /// the row is filtered, for `!=` like every other operator.
    // Constructed by the pipeline lane (slice A′ M5); the enum ships with
    // the extraction so the search lane names its policy explicitly.
    #[allow(dead_code)]
    Strict,
}

/// The column expression a glob/regex matches against: the column itself,
/// or the pin's canonical pattern text (ADR-0011 slice A) — glob on a
/// BIGINT column matches its text form instead of leaving the outcome to
/// `DuckDB`'s implicit-cast rules, and a TIMESTAMP renders as the RFC 3339
/// wire form the live matcher sees rather than `DuckDB`'s space-separated
/// default. The live side mirrors this exactly (`crate::filter`).
pub(crate) fn pattern_target(field: &str, pin: Option<crate::schema::CanonicalType>) -> String {
    match compare::pattern_form(pin) {
        PatternForm::Native => field.to_owned(),
        // A conformed column's own CAST rendering IS its canonical pattern
        // text (`404`, `true`, `200.0`, `1e-07`); the live side mirrors
        // that rendering over the value's cast reading rather than
        // stringifying the wire value.
        PatternForm::BigIntText | PatternForm::BooleanText | PatternForm::DoubleText => {
            format!("CAST({field} AS VARCHAR)")
        }
        PatternForm::Rfc3339Text => {
            format!(
                "strftime({field}, '{}')",
                compare::TIMESTAMP_PATTERN_SQL_FORMAT
            )
        }
    }
}

/// Render one comparison clause for a resolved [`CompareForm`].
///
/// This is the non-pattern literal arm both lanes share: `NumericOnText`
/// and `TextOrNumeric` get their special shapes, everything else binds one
/// parameter, and the `!=` NULL widening applies only under
/// [`NullPolicy::NeMatchesNull`].
pub(crate) fn comparison_sql(
    field: &str,
    op: FilterOp,
    form: CompareForm,
    policy: NullPolicy,
    state: &mut EmitterState,
) -> String {
    let sql_op = filter_op_to_sql(op);
    match form {
        // VARCHAR pin + ordered numeric literal: numeric comparison over
        // the text column, in the ONE comparison space (`crate::conform`)
        // — TRY_CAST so a stored text outside it is NULL (no match), never
        // a Conversion throw.
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
                // Same NULL policy as the plain `!=` shape below: a NULL
                // column matches.
                format!("({predicate} OR {field} IS NULL)")
            } else {
                predicate
            }
        }
        form => {
            let val = comparable_value(form);
            let placeholder = state.push_param(val);
            if op == FilterOp::Ne && policy == NullPolicy::NeMatchesNull {
                // SQL three-valued logic: NULL != x → UNKNOWN → filtered out.
                // Include NULLs explicitly so != behaves as users expect.
                format!("({field} {sql_op} {placeholder} OR {field} IS NULL)")
            } else {
                format!("{field} {sql_op} {placeholder}")
            }
        }
    }
}

/// Render an IN list — each element binds like an equality.
///
/// A `TextOrNumeric` element has no single bound value, so a list carrying
/// one expands to the OR of its per-element equalities: the same set
/// membership, and the same shape the live matcher evaluates (its
/// `InList` is an OR of `=` comparisons). Lists with no such element keep
/// the plain `IN (…)` shape, byte-identical to unpinned emission.
pub(crate) fn in_list_sql(
    field: &str,
    forms: Vec<CompareForm>,
    state: &mut EmitterState,
) -> String {
    if forms
        .iter()
        .any(|f| matches!(f, CompareForm::TextOrNumeric(_)))
    {
        let predicates: Vec<String> = forms
            .into_iter()
            .map(|form| match form {
                CompareForm::TextOrNumeric(literal) => {
                    text_or_numeric(field, FilterOp::Eq, literal, state)
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

/// The equality predicate for a VARCHAR-pinned numeric literal
/// (ADR-0011 slice A): the stored text OR the column's numeric reading.
///
/// `=` is
/// `(col = '200' OR COALESCE(dec(col) = dec('200'), FALSE))` and `!=` is
/// its exact complement over non-NULL values, where `dec` is
/// [`conform::decimal_reading`] — the same expression on the column and on
/// the literal, which is what keeps the literal off the `f64` path that
/// made every id above 2^53 equal to its neighbours (ADR-0011 ruling #6).
/// The literal is therefore bound TWICE, as the same string: once as the
/// text arm's value, once as the numeric arm's cast input.
///
/// Both `COALESCE`s are load-bearing: without them a value with no numeric
/// reading (`'accepted'`) would make the whole predicate UNKNOWN —
/// `status=200` would then be filtered out AND survive `NOT`, and
/// `status!=200` would stop returning the enum-shaped rows that motivate
/// the VARCHAR pin. They also absorb a literal the space cannot read
/// (`nan`, `1e40`): its cast is NULL, so the numeric arm contributes
/// nothing and the text arm decides alone. A NULL column stays UNKNOWN
/// either way (`NULL = ? OR FALSE` is NULL), which is what the live
/// matcher answers for an absent key.
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

/// Collapse the equality-class forms to the `SqlValue` they bind.
/// `NumericOnText` and `TextOrNumeric` are handled by their own SQL shapes
/// before this is called.
fn comparable_value(form: CompareForm) -> SqlValue {
    match form {
        // A typed pin binds exactly as the unpinned path does: the column
        // on disk IS the pinned type, so `DuckDB` compares against it
        // directly. The pin travels for the LIVE matcher's sake
        // (`crate::filter`), which has to conform the wire value first.
        CompareForm::Native(val) | CompareForm::Conformed { literal: val, .. } => val,
        CompareForm::Text(s) => SqlValue::String(s),
        CompareForm::NumericOnText(_) | CompareForm::TextOrNumeric(_) => {
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
        FilterOp::Glob => "GLOB",
        FilterOp::Regex => "~",
    }
}
