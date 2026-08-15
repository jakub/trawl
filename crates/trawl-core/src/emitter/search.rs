// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::ast::{FilterOp, FilterValue, SearchStage, SearchToken, Spanned};
use crate::compare::{self, CompareForm};

use super::SqlValue;
use super::compare::{NullPolicy, comparison_sql, in_list_sql, pattern_target};
use super::fields::quote_field;
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
    let field = quote_field(&ff.field);
    // The catalog pin typing this comparison (ADR-0011 slice A) — `None`
    // outside the catalog-backed paths, which keeps every branch below
    // literal-driven.
    let pin = state.compare_pin(&ff.field);
    match &ff.value {
        FilterValue::Literal(v) => match ff.op {
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
            _ => {
                let form = compare::compare_form(pin, ff.op, v)?;
                let clause = comparison_sql(&field, ff.op, form, NullPolicy::NeMatchesNull, state);
                state.push_where(clause);
            }
        },
        FilterValue::List(vs) => {
            let forms: Vec<CompareForm> = vs
                .iter()
                .map(|v| compare::compare_form(pin, FilterOp::Eq, v))
                .collect::<Result<_, _>>()?;
            let clause = in_list_sql(&field, forms, state);
            state.push_where(clause);
        }
    }
    Ok(())
}
