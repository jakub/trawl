use crate::ast::{FilterOp, FilterValue, SearchStage, SearchToken};

use super::SqlValue;
use super::fields::{coerce_filter_value, quote_field};
use super::state::EmitterState;

/// Translate the search stage tokens into WHERE clauses on the emitter state.
pub(crate) fn emit_search(search: &SearchStage, state: &mut EmitterState) {
    for token in &search.tokens {
        emit_search_token(&token.node, state);
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
            // cast to TIMESTAMP to avoid TIMESTAMPTZ arithmetic requiring ICU
            state.push_where(format!(
                "\"timestamp\" >= now()::TIMESTAMP - INTERVAL '{interval}'"
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
