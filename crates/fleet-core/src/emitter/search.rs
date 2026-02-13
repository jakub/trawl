use crate::ast::{FilterOp, FilterValue, SearchStage, SearchToken};

use super::SqlValue;
use super::fields::{coerce_filter_value, quote_field};
use super::state::EmitterState;

/// Translate the search stage tokens into WHERE clauses on the emitter state.
///
/// For single-group queries, emits tokens directly (same as before).
/// For multi-group (OR) queries, collects each group's clauses and
/// combines them as `(a AND b) OR (c AND d)`.
pub(crate) fn emit_search(search: &SearchStage, state: &mut EmitterState) {
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
                state.push_where(group_conditions.join(" OR "));
            }
        }
    }
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
                            state.push_where(format!("{field} {sql_op} {placeholder}"));
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
        SearchToken::TimeFilter(tf) => {
            let interval = tf.duration.to_interval_string();
            // CAST handles both old VARCHAR parquet files and new native TIMESTAMP files.
            // ::TIMESTAMP on now() avoids TIMESTAMPTZ arithmetic requiring ICU.
            state.push_where(format!(
                "CAST(\"timestamp\" AS TIMESTAMP) >= now()::TIMESTAMP - INTERVAL '{interval}'"
            ));
            state.time_filter = Some(tf.duration);
        }
        SearchToken::QuotedSearch(qs) => {
            let pattern = format!("%{}%", qs.phrase);
            let placeholder = state.push_param(SqlValue::String(pattern));
            state.push_where(format!("\"message\" ILIKE {placeholder}"));
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
