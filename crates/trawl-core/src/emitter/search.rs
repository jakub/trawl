// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::ast::{FilterOp, FilterValue, SearchStage, SearchToken, Spanned};
use crate::compare::{self, CompareForm, PatternForm};

use super::SqlValue;
use super::fields::quote_field;
use super::severity::{LEVEL_FIELD, level_in_list, level_predicate};
use super::state::EmitterState;

/// Translate the search stage tokens into WHERE clauses on the emitter state.
///
/// Time filters are emitted first as a global WHERE clause (hoisted from
/// groups during parsing). Then group tokens are emitted.
///
/// For single-group queries, emits tokens directly (same as before).
/// For multi-group (OR) queries, collects each group's clauses and
/// combines them as `(a AND b) OR (c AND d)`.
pub(crate) fn emit_search(
    search: &SearchStage,
    state: &mut EmitterState,
) -> Result<(), super::EmitError> {
    // Mutual exclusivity: last= and earliest=/latest= cannot be combined.
    if search.time_filter.is_some() && (search.earliest.is_some() || search.latest.is_some()) {
        return Err(super::EmitError::UnsupportedOperation {
            message: "cannot combine 'last=' with 'earliest='/'latest='".to_string(),
        });
    }

    // Emit hoisted time filter as a top-level WHERE clause. TRY_CAST, not
    // CAST: one malformed hot-side value must degrade to NULL (excluded),
    // never throw the whole query (ADR-0008).
    if let Some(tf) = &search.time_filter {
        let interval = tf.node.duration.to_interval_string();
        state.push_where(format!(
            "TRY_CAST(\"_time\" AS TIMESTAMP) >= now()::TIMESTAMP - INTERVAL '{interval}'"
        ));
        state.time_filter = Some(tf.node.duration);
    }

    // Emit absolute time bounds.
    if let Some(earliest) = &search.earliest {
        let p = state.push_param(SqlValue::String(earliest.node.clone()));
        state.push_where(format!(
            "TRY_CAST(\"_time\" AS TIMESTAMP) >= CAST({p} AS TIMESTAMP)"
        ));
    }
    if let Some(latest) = &search.latest {
        let p = state.push_param(SqlValue::String(latest.node.clone()));
        state.push_where(format!(
            "TRY_CAST(\"_time\" AS TIMESTAMP) < CAST({p} AS TIMESTAMP)"
        ));
    }

    match search.groups.len() {
        0 => {}
        1 => {
            // single group — emit directly (backward-compatible)
            for token in &search.groups[0] {
                emit_search_token(&token.node, state)?;
            }
        }
        // multi-group (OR) — collect each group's WHERE clauses separately
        _ => emit_or_groups(&search.groups, state)?,
    }

    Ok(())
}

/// Emit OR-separated groups as a single parenthesised WHERE clause.
///
/// Each group's clauses are collected separately and AND-joined, then the
/// groups are OR-joined: `(a AND b) OR (c AND d)`. The whole expression is
/// wrapped in parens so the OR doesn't interact with other top-level WHERE
/// clauses (e.g. the hoisted time filter). Groups that emit no clauses (a
/// lone `*`, say) drop out.
fn emit_or_groups(
    groups: &[Vec<Spanned<SearchToken>>],
    state: &mut EmitterState,
) -> Result<(), super::EmitError> {
    let mut group_conditions = Vec::new();
    for group in groups {
        let clauses = state.collect_where_clauses(|s| {
            for token in group {
                emit_search_token(&token.node, s)?;
            }
            Ok(())
        })?;
        if !clauses.is_empty() {
            let joined = clauses.join(" AND ");
            group_conditions.push(format!("({joined})"));
        }
    }
    if !group_conditions.is_empty() {
        let or_expr = group_conditions.join(" OR ");
        state.push_where(format!("({or_expr})"));
    }
    Ok(())
}

/// Emit the two-column bare/quoted text-search predicate.
///
/// Bare-word search hits `message` and additionally `_raw` where present
/// (ADR-0009). Negation requires `message` present and the term absent
/// from both columns — `COALESCE(..., TRUE)` keeps a NULL `_raw` from
/// vetoing the row under SQL three-valued logic. The in-memory filter
/// ([`crate::filter`]) mirrors these semantics exactly.
///
/// Searching `_raw` is **whole-event search**, deliberately: except when a
/// collector supplied its own pre-parse line, `_raw` is the server's JSON
/// serialization of the event as it arrived, so an ILIKE over it matches
/// another field's value (`nginx` finds `service=nginx`) *and* a field name
/// (`debug` finds `debug_mode`) — and the negated form excludes on exactly
/// the same basis. That reach is why bare words are worth having; a match
/// confined to one column is what field filters (`message=/debug/`) are for.
/// Documented in the DSL reference under "Text search"; changing it means
/// changing both.
///
/// The `_raw` side comes from [`EmitterState::raw_column`], so the raw-free
/// pass (for sources without the column) substitutes a typed NULL while
/// pushing the same two parameters in the same order — both passes share
/// one parameter list.
fn push_text_search(pattern: String, negated: bool, state: &mut EmitterState) {
    let p1 = state.push_param(SqlValue::String(pattern.clone()));
    let p2 = state.push_param(SqlValue::String(pattern));
    let raw = state.raw_column();
    if negated {
        state.push_where(format!(
            "(\"message\" NOT ILIKE {p1} AND COALESCE({raw} NOT ILIKE {p2}, TRUE))"
        ));
    } else {
        state.push_where(format!("(\"message\" ILIKE {p1} OR {raw} ILIKE {p2})"));
    }
}

fn emit_search_token(
    token: &SearchToken,
    state: &mut EmitterState,
) -> Result<(), super::EmitError> {
    match token {
        SearchToken::FieldFilter(ff) => emit_field_filter(ff, state)?,
        SearchToken::TextSearch(ts) => {
            // wildcard = no filter
            if ts.term == "*" {
                return Ok(());
            }
            push_text_search(format!("%{}%", ts.term), ts.negated, state);
        }
        SearchToken::TimeFilter(_)
        | SearchToken::EarliestFilter(_)
        | SearchToken::LatestFilter(_) => {
            // Time filters are hoisted out of groups during parsing and
            // emitted as a top-level WHERE clause in `emit_search()`.
            // This arm is a defensive no-op — it should never fire.
        }
        SearchToken::QuotedSearch(qs) => {
            push_text_search(format!("%{}%", qs.phrase), false, state);
        }
        SearchToken::Not(inner) => {
            let clauses = state.collect_where_clauses(|s| emit_search_token(&inner.node, s))?;
            if !clauses.is_empty() {
                state.push_where(format!("NOT ({})", clauses.join(" AND ")));
            }
        }
        SearchToken::Group(groups) => emit_or_groups(groups, state)?,
    }
    Ok(())
}

/// Emit one field filter — the comparison arm the catalog pin types
/// (ADR-0011 slice A).
fn emit_field_filter(
    ff: &crate::ast::FieldFilter,
    state: &mut EmitterState,
) -> Result<(), super::EmitError> {
    // `level` is a DSL alias for the numeric severity column:
    // band predicates, not string comparison (ADR-0009).
    if ff.field == LEVEL_FIELD {
        let clause = match &ff.value {
            FilterValue::Literal(v) => level_predicate(ff.op, v)?,
            FilterValue::List(vs) => level_in_list(vs)?,
        };
        state.push_where(clause);
        return Ok(());
    }
    let field = quote_field(&ff.field);
    // The catalog pin typing this comparison (ADR-0011 slice A) — `None`
    // outside the catalog-backed paths, which keeps every branch below
    // literal-driven.
    let pin = state.compare_pin(&ff.field);
    match &ff.value {
        FilterValue::Literal(v) => {
            let sql_op = filter_op_to_sql(ff.op);
            match ff.op {
                FilterOp::Glob => {
                    let target = pattern_target(&field, pin);
                    let placeholder = state.push_param(SqlValue::String(v.clone()));
                    state.push_where(format!("{target} GLOB {placeholder}"));
                }
                FilterOp::Regex => {
                    let target = pattern_target(&field, pin);
                    let placeholder = state.push_param(SqlValue::String(v.clone()));
                    state.push_where(format!("regexp_matches({target}, {placeholder})"));
                }
                _ => match compare::compare_form(pin, ff.op, v) {
                    // VARCHAR pin + ordered numeric literal: numeric
                    // comparison over the text column — TRY_CAST so a
                    // non-numeric stored value is NULL (no match), never a
                    // Conversion throw.
                    CompareForm::NumericOnText(n) => {
                        let placeholder = state.push_param(SqlValue::Float(n));
                        state.push_where(format!(
                            "TRY_CAST({field} AS DOUBLE) {sql_op} {placeholder}"
                        ));
                    }
                    // VARCHAR pin + equality-class numeric literal: the
                    // exact text OR the column's numeric reading, because
                    // the stored text of a number is `read_json`'s
                    // inference rendered (`"200.0"`), not the wire spelling
                    // the live matcher sees (`crate::compare`).
                    CompareForm::TextOrNumeric { text, number } => {
                        let predicate = text_or_numeric(&field, ff.op, text, number, state);
                        if ff.op == FilterOp::Ne {
                            // Same NULL policy as the plain `!=` shape
                            // below: a NULL column matches.
                            state.push_where(format!("({predicate} OR {field} IS NULL)"));
                        } else {
                            state.push_where(predicate);
                        }
                    }
                    form => {
                        let val = comparable_value(form);
                        let placeholder = state.push_param(val);
                        if ff.op == FilterOp::Ne {
                            // SQL three-valued logic: NULL != x → UNKNOWN → filtered out.
                            // Include NULLs explicitly so != behaves as users expect.
                            state.push_where(format!(
                                "({field} {sql_op} {placeholder} OR {field} IS NULL)"
                            ));
                        } else {
                            state.push_where(format!("{field} {sql_op} {placeholder}"));
                        }
                    }
                },
            }
        }
        FilterValue::List(vs) => emit_in_list(&field, pin, vs, state),
    }
    Ok(())
}

/// Emit an IN list — each element binds like an equality.
///
/// A `TextOrNumeric` element has no single bound value, so a list carrying
/// one expands to the OR of its per-element equalities: the same set
/// membership, and the same shape the live matcher evaluates (its
/// `InList` is an OR of `=` comparisons). Lists with no such element keep
/// the plain `IN (…)` shape, byte-identical to unpinned emission.
fn emit_in_list(
    field: &str,
    pin: Option<crate::schema::CanonicalType>,
    values: &[String],
    state: &mut EmitterState,
) {
    let forms: Vec<CompareForm> = values
        .iter()
        .map(|v| compare::compare_form(pin, FilterOp::Eq, v))
        .collect();
    if forms
        .iter()
        .any(|f| matches!(f, CompareForm::TextOrNumeric { .. }))
    {
        let predicates: Vec<String> = forms
            .into_iter()
            .map(|form| match form {
                CompareForm::TextOrNumeric { text, number } => {
                    text_or_numeric(field, FilterOp::Eq, text, number, state)
                }
                other => {
                    let placeholder = state.push_param(comparable_value(other));
                    format!("{field} = {placeholder}")
                }
            })
            .collect();
        state.push_where(format!("({})", predicates.join(" OR ")));
    } else {
        let placeholders: Vec<String> = forms
            .into_iter()
            .map(|form| state.push_param(comparable_value(form)))
            .collect();
        state.push_where(format!("{field} IN ({})", placeholders.join(", ")));
    }
}

/// The column expression a glob/regex matches against: the column itself,
/// or the pin's canonical pattern text (ADR-0011 slice A) — glob on a
/// BIGINT column matches its text form instead of leaving the outcome to
/// `DuckDB`'s implicit-cast rules, and a TIMESTAMP renders as the RFC 3339
/// wire form the live matcher sees rather than `DuckDB`'s space-separated
/// default. The live side mirrors this exactly (`crate::filter`).
fn pattern_target(field: &str, pin: Option<crate::schema::CanonicalType>) -> String {
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

/// The equality predicate for a VARCHAR-pinned numeric literal
/// (ADR-0011 slice A): the stored text OR the column's numeric reading.
///
/// `=` is `(col = '200' OR COALESCE(TRY_CAST(col AS DOUBLE) = 200, FALSE))`
/// and `!=` is its exact complement over non-NULL values. Both `COALESCE`s
/// are load-bearing: without them a value with no numeric reading
/// (`'accepted'`) would make the whole predicate UNKNOWN — `status=200`
/// would then be filtered out AND survive `NOT`, and `status!=200` would
/// stop returning the enum-shaped rows that motivate the VARCHAR pin. A
/// NULL column stays UNKNOWN either way (`NULL = ? OR FALSE` is NULL),
/// which is what the live matcher answers for an absent key.
fn text_or_numeric(
    field: &str,
    op: FilterOp,
    text: String,
    number: f64,
    state: &mut EmitterState,
) -> String {
    let text_param = state.push_param(SqlValue::String(text));
    let num_param = state.push_param(SqlValue::Float(number));
    let cast = format!("TRY_CAST({field} AS DOUBLE)");
    if op == FilterOp::Ne {
        format!("({field} != {text_param} AND COALESCE({cast} != {num_param}, TRUE))")
    } else {
        format!("({field} = {text_param} OR COALESCE({cast} = {num_param}, FALSE))")
    }
}

/// Collapse the equality-class forms to the `SqlValue` they bind.
/// `NumericOnText` and `TextOrNumeric` are handled by their own SQL shapes
/// before this is called.
fn comparable_value(form: CompareForm) -> SqlValue {
    match form {
        CompareForm::Native(val) => val,
        CompareForm::Text(s) => SqlValue::String(s),
        CompareForm::NumericOnText(_) | CompareForm::TextOrNumeric { .. } => {
            debug_assert!(
                false,
                "the pinned numeric forms have their own emission shape"
            );
            SqlValue::String(String::new())
        }
    }
}

fn filter_op_to_sql(op: FilterOp) -> &'static str {
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
