// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::ast::{FilterOp, FilterValue, SearchStage, SearchToken};

use super::SqlValue;
use super::fields::{coerce_filter_value, quote_field};
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

    // Emit hoisted time filter as a top-level WHERE clause.
    if let Some(tf) = &search.time_filter {
        let interval = tf.node.duration.to_interval_string();
        state.push_where(format!(
            "CAST(\"timestamp\" AS TIMESTAMP) >= now()::TIMESTAMP - INTERVAL '{interval}'"
        ));
        state.time_filter = Some(tf.node.duration);
    }

    // Emit absolute time bounds.
    if let Some(earliest) = &search.earliest {
        let p = state.push_param(SqlValue::String(earliest.node.clone()));
        state.push_where(format!(
            "CAST(\"timestamp\" AS TIMESTAMP) >= CAST({p} AS TIMESTAMP)"
        ));
    }
    if let Some(latest) = &search.latest {
        let p = state.push_param(SqlValue::String(latest.node.clone()));
        state.push_where(format!(
            "CAST(\"timestamp\" AS TIMESTAMP) < CAST({p} AS TIMESTAMP)"
        ));
    }

    match search.groups.len() {
        0 => {}
        1 => {
            // single group — emit directly (backward-compatible)
            for token in &search.groups[0] {
                emit_search_token(&token.node, state);
            }
        }
        _ => {
            // multi-group (OR) — collect each group's WHERE clauses separately
            let mut group_conditions = Vec::new();
            for group in &search.groups {
                let clauses = state.collect_where_clauses(|s| {
                    for token in group {
                        emit_search_token(&token.node, s);
                    }
                });
                if !clauses.is_empty() {
                    let joined = clauses.join(" AND ");
                    group_conditions.push(format!("({joined})"));
                }
            }
            if !group_conditions.is_empty() {
                let or_expr = group_conditions.join(" OR ");
                // Wrap in parens so the OR doesn't interact with other
                // top-level WHERE clauses (e.g. the hoisted time filter).
                state.push_where(format!("({or_expr})"));
            }
        }
    }

    Ok(())
}

fn emit_search_token(token: &SearchToken, state: &mut EmitterState) {
    match token {
        SearchToken::FieldFilter(ff) => {
            let field = quote_field(&ff.field);
            match &ff.value {
                FilterValue::Literal(v) => {
                    let sql_op = filter_op_to_sql(ff.op);
                    match ff.op {
                        FilterOp::Glob => {
                            let placeholder = state.push_param(SqlValue::String(v.clone()));
                            state.push_where(format!("{field} GLOB {placeholder}"));
                        }
                        FilterOp::Regex => {
                            let placeholder = state.push_param(SqlValue::String(v.clone()));
                            state.push_where(format!("regexp_matches({field}, {placeholder})"));
                        }
                        _ => {
                            let val = coerce_filter_value(v);
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
                    }
                }
                FilterValue::List(vs) => {
                    let placeholders: Vec<String> = vs
                        .iter()
                        .map(|v| {
                            let val = coerce_filter_value(v);
                            state.push_param(val)
                        })
                        .collect();
                    state.push_where(format!("{field} IN ({})", placeholders.join(", ")));
                }
            }
        }
        SearchToken::TextSearch(ts) => {
            // wildcard = no filter
            if ts.term == "*" {
                return;
            }
            let pattern = format!("%{}%", ts.term);
            let placeholder = state.push_param(SqlValue::String(pattern));
            if ts.negated {
                state.push_where(format!("\"message\" NOT ILIKE {placeholder}"));
            } else {
                state.push_where(format!("\"message\" ILIKE {placeholder}"));
            }
        }
        SearchToken::TimeFilter(_)
        | SearchToken::EarliestFilter(_)
        | SearchToken::LatestFilter(_) => {
            // Time filters are hoisted out of groups during parsing and
            // emitted as a top-level WHERE clause in `emit_search()`.
            // This arm is a defensive no-op — it should never fire.
        }
        SearchToken::QuotedSearch(qs) => {
            let pattern = format!("%{}%", qs.phrase);
            let placeholder = state.push_param(SqlValue::String(pattern));
            state.push_where(format!("\"message\" ILIKE {placeholder}"));
        }
        SearchToken::Not(inner) => {
            let clauses = state.collect_where_clauses(|s| {
                emit_search_token(&inner.node, s);
            });
            if !clauses.is_empty() {
                state.push_where(format!("NOT ({})", clauses.join(" AND ")));
            }
        }
        SearchToken::Group(groups) => {
            let mut group_conditions = Vec::new();
            for group in groups {
                let clauses = state.collect_where_clauses(|s| {
                    for token in group {
                        emit_search_token(&token.node, s);
                    }
                });
                if !clauses.is_empty() {
                    let joined = clauses.join(" AND ");
                    group_conditions.push(format!("({joined})"));
                }
            }
            if !group_conditions.is_empty() {
                let or_expr = group_conditions.join(" OR ");
                state.push_where(format!("({or_expr})"));
            }
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
