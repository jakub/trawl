// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::ast::{FilterOp, FilterValue, Scalar, SearchStage, SearchToken, Spanned};
use crate::compare::{self, CompareForm};

use super::SqlValue;
use super::compare::{NullPolicy, comparison_sql, in_list_sql, pattern_target};
use super::state::{EmitterState, Use};

pub(crate) fn emit_search(
    search: &SearchStage,
    state: &mut EmitterState,
) -> Result<(), super::EmitError> {
    // TRY_CAST, not CAST: one malformed hot-side value must degrade to NULL
    // (excluded), never throw the whole query (ADR-0008).
    if let Some(tf) = &search.time_filter {
        let interval = tf.node.duration.to_interval_string();
        let time = state.read_field(crate::schema::TIME, Use::Operand)?;
        state.push_where(format!(
            "TRY_CAST({time} AS TIMESTAMP) >= now()::TIMESTAMP - INTERVAL '{interval}'"
        ));
        state.time_filter = Some(tf.node.duration);
    }

    if let Some(earliest) = &search.earliest {
        let time = state.read_field(crate::schema::TIME, Use::Operand)?;
        let p = state.push_param(SqlValue::String(earliest.node.instant().cast_text()));
        state.push_where(format!(
            "TRY_CAST({time} AS TIMESTAMP) >= CAST({p} AS TIMESTAMP)"
        ));
    }
    if let Some(latest) = &search.latest {
        let time = state.read_field(crate::schema::TIME, Use::Operand)?;
        let p = state.push_param(SqlValue::String(latest.node.instant().cast_text()));
        state.push_where(format!(
            "TRY_CAST({time} AS TIMESTAMP) < CAST({p} AS TIMESTAMP)"
        ));
    }

    match search.groups.len() {
        0 => {}
        1 => {
            for token in &search.groups[0] {
                emit_search_token(&token.node, state)?;
            }
        }
        _ => emit_or_groups(&search.groups, state)?,
    }

    Ok(())
}

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

fn push_text_search(
    text: &str,
    negated: bool,
    state: &mut EmitterState,
) -> Result<(), super::EmitError> {
    let pattern = contains_pattern(text);
    let p1 = state.push_param(SqlValue::String(pattern.clone()));
    let p2 = state.push_param(SqlValue::String(pattern));
    let message = state.read_field(crate::schema::MESSAGE, Use::Operand)?;
    let raw = state.read_field(crate::schema::RAW, Use::Operand)?;
    let message_contains = text_contains(&message, &p1);
    let raw_contains = text_contains(&raw, &p2);
    if negated {
        state.push_where(format!("(NOT {message_contains} AND NOT {raw_contains})"));
    } else {
        state.push_where(format!("({message_contains} OR {raw_contains})"));
    }
    Ok(())
}

/// Escaping `\` itself is what keeps a term ending in a backslash from
/// escaping the closing `%`.
fn contains_pattern(text: &str) -> String {
    let mut pattern = String::with_capacity(text.len() + 2);
    pattern.push('%');
    for ch in text.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

/// Foreign sources can carry a numeric or boolean `message`/`_raw`, and
/// heterogeneous NDJSON is inferred as `JSON`; non-string cells must not
/// become matches through stringification while a JSON string cell must
/// remain searchable.
fn text_contains(column: &str, param: &str) -> String {
    format!(
        "COALESCE((CASE WHEN typeof({column}) = 'VARCHAR' THEN CAST({column} AS VARCHAR) WHEN typeof({column}) = 'JSON' AND json_type(CAST({column} AS JSON)) = 'VARCHAR' THEN json_extract_string(CAST({column} AS JSON), '$') ELSE NULL END) ILIKE {param} ESCAPE '\\', FALSE)"
    )
}

fn emit_search_token(
    token: &SearchToken,
    state: &mut EmitterState,
) -> Result<(), super::EmitError> {
    match token {
        SearchToken::FieldFilter(ff) => emit_field_filter(ff, state)?,
        SearchToken::TextSearch(ts) => {
            if ts.term == "*" {
                return Ok(());
            }
            push_text_search(&ts.term, ts.negated, state)?;
        }
        SearchToken::TimeFilter(_)
        | SearchToken::EarliestFilter(_)
        | SearchToken::LatestFilter(_) => {
            // Time filters are hoisted out of groups during parsing and
            // emitted as a top-level WHERE clause in `emit_search()`.
            // This arm is a defensive no-op — it should never fire.
        }
        SearchToken::QuotedSearch(qs) => {
            push_text_search(&qs.phrase, false, state)?;
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

fn emit_field_filter(
    ff: &crate::ast::FieldFilter,
    state: &mut EmitterState,
) -> Result<(), super::EmitError> {
    let field = state.read_field(&ff.field, Use::Operand)?;
    let pin = state.compare_pin(&ff.field);
    let clause = match (&ff.value, ff.op) {
        (FilterValue::Single(Scalar::Exact(v)), op) => {
            let form = compare::compare_form(pin, op, v)?;
            comparison_sql(&field, op, form, NullPolicy::NeMatchesNull, state)
        }
        (FilterValue::Single(pattern), op @ (FilterOp::Eq | FilterOp::Ne)) => {
            let target = pattern_target(&field, pin);
            pattern_sql(&target, pattern, op == FilterOp::Ne, state)?
        }
        (FilterValue::Single(_), _) => {
            unreachable!("the parser refuses an ordered operator with a pattern (ADR-0060)")
        }
        (FilterValue::List(elements), FilterOp::Eq) => {
            let mut exact = Vec::new();
            let mut patterns = Vec::new();
            for element in elements {
                match element {
                    Scalar::Exact(v) => exact.push(compare::compare_form(pin, FilterOp::Eq, v)?),
                    pattern => patterns.push(pattern),
                }
            }
            if patterns.is_empty() {
                in_list_sql(&field, exact, state)
            } else {
                let target = pattern_target(&field, pin);
                let mut terms = Vec::with_capacity(patterns.len() + 1);
                if !exact.is_empty() {
                    terms.push(in_list_sql(&field, exact, state));
                }
                for pattern in patterns {
                    terms.push(pattern_sql(&target, pattern, false, state)?);
                }
                format!("({})", terms.join(" OR "))
            }
        }
        (FilterValue::List(elements), FilterOp::Ne) => {
            enum Term<'a> {
                Exact(CompareForm),
                Pattern(&'a Scalar),
            }
            let terms = elements
                .iter()
                .map(|element| match element {
                    Scalar::Exact(v) => {
                        compare::compare_form(pin, FilterOp::Eq, v).map(Term::Exact)
                    }
                    pattern => Ok(Term::Pattern(pattern)),
                })
                .collect::<Result<Vec<_>, _>>()?;
            let target = pattern_target(&field, pin);
            terms
                .into_iter()
                .map(|term| match term {
                    Term::Exact(form) => Ok(comparison_sql(
                        &field,
                        FilterOp::Ne,
                        form,
                        NullPolicy::NeMatchesNull,
                        state,
                    )),
                    Term::Pattern(pattern) => pattern_sql(&target, pattern, true, state),
                })
                .collect::<Result<Vec<_>, _>>()?
                .join(" AND ")
        }
        (FilterValue::List(_), FilterOp::Gt | FilterOp::Gte | FilterOp::Lt | FilterOp::Lte) => {
            unreachable!("ordered operators cannot reach a comma-separated list")
        }
    };
    state.push_where(clause);
    Ok(())
}

fn pattern_sql(
    target: &str,
    pattern: &Scalar,
    negated: bool,
    state: &mut EmitterState,
) -> Result<String, super::EmitError> {
    let predicate = match pattern {
        Scalar::Glob(glob) => {
            let placeholder = state.push_param(SqlValue::String(glob.clone()));
            format!("{target} GLOB {placeholder}")
        }
        Scalar::Regex(body) => {
            let placeholder = state.push_regex_param(body)?;
            format!("regexp_matches({target}, {placeholder})")
        }
        Scalar::Exact(_) => unreachable!("an exact value binds through compare_form"),
    };
    Ok(if negated {
        format!("NOT COALESCE({predicate}, FALSE)")
    } else {
        predicate
    })
}
