// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::borrow::Cow;

use crate::ast::{BinaryOp, Expr, FilterOp, FloatLiteral, LiteralValue, Spanned, UnaryOp};
use crate::compare::{self, CompareForm, PatternForm};
use crate::pattern::{PatternKind, literal_operand};

use super::EmitError;
use super::SqlValue;
use super::compare::{NullPolicy, comparison_sql, in_list_sql, pattern_target};
use super::functions::{
    absent_argument_type, check_call, int_literal_arg, json_read_positions, literal_int_positions,
    literal_text_positions, translate_function,
};
use super::relation::Nullness;
use super::state::{EmitterState, Use};

pub(crate) fn emit_expr(
    expr: &Spanned<Expr>,
    state: &mut EmitterState,
) -> Result<String, EmitError> {
    match &expr.node {
        Expr::Literal(lit) => Ok(emit_literal(lit, state)),
        Expr::FieldRef(name) => state.read_field(name, Use::Operand),
        Expr::Binary { lhs, op, rhs } => {
            if let Some(kind) = PatternKind::of_operator(*op) {
                return emit_pattern_test(lhs, kind, rhs, state);
            }
            if let Some(clause) = try_pinned_comparison(lhs, *op, rhs, state)? {
                return Ok(clause);
            }
            let l = emit_expr(lhs, state)?;
            let r = emit_expr(rhs, state)?;
            Ok(emit_binary(&l, *op, &r))
        }
        Expr::Unary { op, operand } => {
            let inner = emit_expr(operand, state)?;
            Ok(emit_unary(*op, &inner))
        }
        Expr::FunctionCall { name, args } => {
            let translated_args = emit_call_args(name, args, state)?;
            translate_function(name, &translated_args, state)
        }
        Expr::InList { expr: target, list } => {
            if let Some(clause) = try_pinned_in_list(target, list, state)? {
                return Ok(clause);
            }
            let lhs = emit_expr(target, state)?;
            let items: Vec<String> = list
                .iter()
                .map(|item| emit_expr(item, state))
                .collect::<Result<_, _>>()?;
            Ok(format!("({lhs} IN ({}))", items.join(", ")))
        }
    }
}

/// The one argument walk for every position a call can sit in: the
/// expression arm above and every aggregation call site
/// (`emitter::pipeline`'s `stats`/`timechart`/`pivot`/`eventstats`).
/// Emitting an aggregation's arguments with a bare `emit_expr` instead
/// would bind `stats sev(level, "syslog")`'s dialect as a parameter, and
/// the translation cannot read a `?`.
pub(crate) fn emit_call_args(
    name: &str,
    args: &[Spanned<Expr>],
    state: &mut EmitterState,
) -> Result<Vec<String>, EmitError> {
    // The call's own rules first, parent before its arguments, so emit
    // refuses in the order every other lane does.
    check_call(name, args)?;
    let lit_positions = literal_int_positions(name);
    let text_positions = literal_text_positions(name);
    let json_positions = json_read_positions(name);
    let translated_args: Vec<String> = args
        .iter()
        .enumerate()
        .map(|(i, a)| {
            if lit_positions.contains(&i) {
                // DuckDB requires certain args as literal ints, not parameters
                int_literal_arg(name, i, &a.node).map(|n| n.to_string())
            } else if text_positions.contains(&i) {
                // The token chooses the emitted expression (`sev()`'s
                // dialect), so it is never a bound parameter:
                // `translate_function` has to read it. Case-folded here,
                // so the vocabulary check and the dialect parse downstream
                // read one spelling.
                match &a.node {
                    Expr::Literal(LiteralValue::String(s)) => Ok(s.to_ascii_lowercase()),
                    _ => unreachable!("check_call refuses a non-literal {name}() dialect"),
                }
            } else if json_positions.contains(&i)
                && let Some(literal) = bare_literal(&a.node)
            {
                Ok(typed_literal(&literal, state))
            } else {
                match absent_argument_type(name, i).map(|ty| (ty, state.nullness(&a.node))) {
                    Some((ty, Nullness::Untyped)) => {
                        Ok(format!("CAST({} AS {ty})", emit_expr(a, state)?))
                    }
                    Some((ty, Nullness::UntypedUnlessMade)) => Ok(format!(
                        "COALESCE({}, CAST(NULL AS {ty}))",
                        emit_expr(a, state)?
                    )),
                    Some((_, Nullness::Typed)) | None => emit_expr(a, state),
                }
            }
        })
        .collect::<Result<_, _>>()?;
    Ok(translated_args)
}

fn comparison_filter_op(op: BinaryOp) -> Option<FilterOp> {
    match op {
        BinaryOp::Eq => Some(FilterOp::Eq),
        BinaryOp::Ne => Some(FilterOp::Ne),
        BinaryOp::Gt => Some(FilterOp::Gt),
        BinaryOp::Gte => Some(FilterOp::Gte),
        BinaryOp::Lt => Some(FilterOp::Lt),
        BinaryOp::Lte => Some(FilterOp::Lte),
        _ => None,
    }
}

fn flip_filter_op(op: FilterOp) -> FilterOp {
    match op {
        FilterOp::Gt => FilterOp::Lt,
        FilterOp::Gte => FilterOp::Lte,
        FilterOp::Lt => FilterOp::Gt,
        FilterOp::Lte => FilterOp::Gte,
        other => other,
    }
}

/// A negative numeric literal leaves the parser as `Unary{Neg, Literal}`,
/// never as a signed `Literal`, so the sign is folded back in here. Without
/// the fold a bare `> -400` would emit pin-blind while its quoted spelling
/// `> "-400"` bound pin-aware.
fn bare_literal(expr: &Expr) -> Option<Cow<'_, LiteralValue>> {
    match expr {
        Expr::Literal(lit) => Some(Cow::Borrowed(lit)),
        Expr::Unary {
            op: UnaryOp::Neg,
            operand,
        } => match &operand.node {
            Expr::Literal(LiteralValue::Int(n)) => {
                Some(Cow::Owned(LiteralValue::Int(n.checked_neg()?)))
            }
            Expr::Literal(LiteralValue::Float(f)) => Some(Cow::Owned(LiteralValue::Float(
                FloatLiteral::new(-f.value(), format!("-{}", f.text())),
            ))),
            _ => None,
        },
        _ => None,
    }
}

fn emit_pattern_test(
    lhs: &Spanned<Expr>,
    kind: PatternKind,
    rhs: &Spanned<Expr>,
    state: &mut EmitterState,
) -> Result<String, EmitError> {
    let pattern = literal_operand(&rhs.node).map_err(|_| EmitError::ComputedPattern)?;
    // The subject's SQL is built before the pattern's parameter, so any
    // placeholder inside it keeps its positional order.
    let target = match state.pin_scope().subject_pin(lhs) {
        Some((subject, pin)) if compare::pattern_form(Some(pin)) != PatternForm::Native => {
            pattern_target(&subject_sql(&subject, state)?, Some(pin))
        }
        _ => emit_expr(lhs, state)?,
    };
    Ok(match kind {
        PatternKind::Regex => format!(
            "regexp_matches({target}, {})",
            state.push_regex_param(pattern)?
        ),
        PatternKind::Like => format!(
            "({target} LIKE {})",
            state.push_param(SqlValue::String(pattern.to_owned()))
        ),
        PatternKind::ILike => format!(
            "({target} ILIKE {})",
            state.push_param(SqlValue::String(pattern.to_owned()))
        ),
        PatternKind::Glob => unreachable!("no expression operator takes a glob"),
    })
}

fn try_pinned_comparison(
    lhs: &Spanned<Expr>,
    op: BinaryOp,
    rhs: &Spanned<Expr>,
    state: &mut EmitterState,
) -> Result<Option<String>, EmitError> {
    let Some(filter_op) = comparison_filter_op(op) else {
        return Ok(None);
    };
    let resolved = match (
        state.pin_scope().subject_pin(lhs),
        state.pin_scope().subject_pin(rhs),
    ) {
        (Some((subject, pin)), _) => bare_literal(&rhs.node).map(|l| (subject, pin, filter_op, l)),
        (None, Some((subject, pin))) => {
            bare_literal(&lhs.node).map(|l| (subject, pin, flip_filter_op(filter_op), l))
        }
        (None, None) => None,
    };
    let Some((subject, pin, filter_op, literal)) = resolved else {
        return Ok(None);
    };
    let Some(form) = compare::compare_form_bound(Some(pin), filter_op, &literal)? else {
        return Ok(None);
    };
    if matches!(form, CompareForm::Native(_)) {
        return Ok(None);
    }
    // Subject first, again for parameter order.
    let target = subject_sql(&subject, state)?;
    let clause = comparison_sql(&target, filter_op, form, NullPolicy::Strict, state);
    Ok(Some(if clause.starts_with('(') {
        clause
    } else {
        format!("({clause})")
    }))
}

fn try_pinned_in_list(
    target: &Spanned<Expr>,
    list: &[Spanned<Expr>],
    state: &mut EmitterState,
) -> Result<Option<String>, EmitError> {
    let Some((subject, pin)) = state.pin_scope().subject_pin(target) else {
        return Ok(None);
    };
    let mut forms: Vec<CompareForm> = Vec::with_capacity(list.len());
    for item in list {
        let Some(element) = bare_literal(&item.node) else {
            return Ok(None);
        };
        let Some(form) = compare::compare_form_bound(Some(pin), FilterOp::Eq, &element)? else {
            return Ok(None);
        };
        forms.push(form);
    }
    // Subject first, for parameter order.
    let subject = subject_sql(&subject, state)?;
    let clause = in_list_sql(&subject, forms, state);
    Ok(Some(if clause.starts_with('(') {
        clause
    } else {
        format!("({clause})")
    }))
}

/// A pin-declaring call is emitted by the ordinary function path, so it
/// is the same SQL `| let s = sev(level)` would project and the
/// comparison and the projection cannot read one value two ways.
fn subject_sql(
    subject: &crate::pin_scope::PinnedSubject<'_>,
    state: &mut EmitterState,
) -> Result<String, EmitError> {
    match subject {
        crate::pin_scope::PinnedSubject::Field(name) => state.read_field(name, Use::Operand),
        crate::pin_scope::PinnedSubject::Call(call) => emit_expr(call, state),
    }
}

/// `DuckDB` infers a parameter's type from its surroundings, and `to_json(?)`
/// offers none, so the statement fails to prepare.
fn typed_literal(lit: &LiteralValue, state: &mut EmitterState) -> String {
    let ty = match lit {
        LiteralValue::String(_) | LiteralValue::Null => "VARCHAR",
        LiteralValue::Int(_) => "BIGINT",
        LiteralValue::Float(_) => "DOUBLE",
        LiteralValue::Bool(_) => "BOOLEAN",
    };
    format!("CAST({} AS {ty})", emit_literal(lit, state))
}

fn emit_literal(lit: &LiteralValue, state: &mut EmitterState) -> String {
    match lit {
        LiteralValue::Null => "NULL".to_string(),
        LiteralValue::String(s) => state.push_param(SqlValue::String(s.clone())),
        LiteralValue::Int(n) => state.push_param(SqlValue::Int(*n)),
        LiteralValue::Float(n) => state.push_param(SqlValue::Float(n.value())),
        LiteralValue::Bool(b) => state.push_param(SqlValue::Bool(*b)),
    }
}

fn emit_binary(lhs: &str, op: BinaryOp, rhs: &str) -> String {
    let sql_op = match op {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Mod => "%",
        BinaryOp::Eq => "=",
        BinaryOp::Ne => "!=",
        BinaryOp::Gt => ">",
        BinaryOp::Gte => ">=",
        BinaryOp::Lt => "<",
        BinaryOp::Lte => "<=",
        BinaryOp::And => "AND",
        BinaryOp::Or => "OR",
        BinaryOp::Matches | BinaryOp::Like | BinaryOp::ILike => {
            unreachable!("a pattern operator emits through emit_pattern_test")
        }
    };
    format!("({lhs} {sql_op} {rhs})")
}

fn emit_unary(op: UnaryOp, inner: &str) -> String {
    match op {
        UnaryOp::Not => format!("(NOT {inner})"),
        UnaryOp::Neg => format!("(-{inner})"),
    }
}
