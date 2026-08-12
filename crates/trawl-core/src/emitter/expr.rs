// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::borrow::Cow;

use crate::ast::{BinaryOp, Expr, FilterOp, FloatLiteral, LiteralValue, Spanned, UnaryOp};
use crate::compare::{self, CompareForm, PatternForm};

use super::EmitError;
use super::SqlValue;
use super::compare::{NullPolicy, comparison_sql, in_list_sql, pattern_target};
use super::fields::quote_field;
use super::functions::{
    format_literal_position, literal_int_positions, translate_function, unit_literal_positions,
    validate_format_literal, validate_unit_literal,
};
use super::state::EmitterState;

/// Recursively translate an expression AST node to a SQL fragment.
pub(crate) fn emit_expr(
    expr: &Spanned<Expr>,
    state: &mut EmitterState,
) -> Result<String, EmitError> {
    match &expr.node {
        Expr::Literal(lit) => Ok(emit_literal(lit, state)),
        Expr::FieldRef(name) => Ok(quote_field(name)),
        Expr::Binary { lhs, op, rhs } => {
            // `level` comparisons route through the severity band helper
            // (ADR-0009): `where level == "error"` means the ERROR band on
            // the numeric severity column, exactly like `level=error` in
            // the search stage.
            if let Some(clause) = try_level_comparison(lhs, *op, rhs)? {
                return Ok(clause);
            }
            // Bare field-vs-literal comparisons consult the pin scope
            // (ADR-0011 slice A′) — same rule table as the search stage.
            if let Some(clause) = try_pinned_comparison(lhs, *op, rhs, state) {
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
            let lit_positions = literal_int_positions(name);
            let unit_positions = unit_literal_positions(name);
            let fmt_position = format_literal_position(name);
            let translated_args: Vec<String> = args
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    if lit_positions.contains(&i) {
                        // DuckDB requires certain args as literal ints, not parameters
                        match &a.node {
                            Expr::Literal(LiteralValue::Int(n)) => Ok(n.to_string()),
                            _ => Err(EmitError::InvalidAggregation {
                                message: format!(
                                    "{name}() argument {} must be an integer literal",
                                    i + 1,
                                ),
                            }),
                        }
                    } else if let Some((_, allowlist)) =
                        unit_positions.iter().find(|(pos, _)| *pos == i)
                    {
                        // Date/time unit args must be string literals from the allowlist.
                        let raw = match &a.node {
                            Expr::Literal(LiteralValue::String(s)) => Some(s.as_str()),
                            _ => None,
                        };
                        validate_unit_literal(name, i, allowlist, raw)?;
                        emit_expr(a, state)
                    } else {
                        if fmt_position == Some(i) {
                            // strftime/strptime format arg: reject invalid format
                            // codes at emit time when it is a string literal, so
                            // batch and streaming fail identically. A non-literal
                            // (field ref) can't be checked here and keeps its
                            // pre-existing runtime behaviour.
                            if let Expr::Literal(LiteralValue::String(s)) = &a.node {
                                validate_format_literal(name, s)?;
                            }
                        }
                        emit_expr(a, state)
                    }
                })
                .collect::<Result<_, _>>()?;
            translate_function(name, &translated_args)
        }
        Expr::InList { expr: target, list } => {
            // A pinned bare-field target with an all-literal list routes
            // each element through the equality rule (ADR-0011 slice A′),
            // mirroring the search stage's IN list.
            if let Some(clause) = try_pinned_in_list(target, list, state) {
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

/// Map a comparison [`BinaryOp`] onto the search stage's [`FilterOp`]
/// vocabulary; `None` for every non-comparison operator.
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

/// The operand flip for `400 < status` → `status > 400`.
fn flip_filter_op(op: FilterOp) -> FilterOp {
    match op {
        FilterOp::Gt => FilterOp::Lt,
        FilterOp::Gte => FilterOp::Lte,
        FilterOp::Lt => FilterOp::Gt,
        FilterOp::Lte => FilterOp::Gte,
        other => other,
    }
}

/// A bare literal, as the rule table's door wants it — the AST value, not
/// a re-rendered one, so a float literal keeps its source token (ADR-0011
/// ruling #6, [`crate::ast::FloatLiteral`]).
///
/// A NEGATIVE numeric literal reaches the parser as `Unary{Neg, Literal}`,
/// never as a signed `Literal`, so the sign is folded back in here — into
/// the `i64` and into the float's SOURCE TOKEN alike, keeping the DECIMAL
/// binding exact. Without the fold every negative literal would fall
/// through to pin-blind emission and raise exactly the VARCHAR-pin binder
/// error slice A′ removes, while its quoted spelling (`> "-400"`, a plain
/// `Literal`) bound pin-aware — two spellings of one number disagreeing.
/// Only `Int`/`Float` fold: `-"400"` and `-true` are arithmetic on a
/// non-number and keep their generic emission.
fn bare_literal(expr: &Expr) -> Option<Cow<'_, LiteralValue>> {
    match expr {
        Expr::Literal(lit) => Some(Cow::Borrowed(lit)),
        Expr::Unary {
            op: UnaryOp::Neg,
            operand,
        } => match &operand.node {
            // `i64::MIN`'s magnitude has no `i64` negation to fold into;
            // it declines, exactly as any other unfoldable shape does.
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

/// Detect a bare field-vs-literal comparison whose field resolves to a
/// catalog pin, and emit it through the shared rule table (ADR-0011 slice
/// A′). Returns `None` — falling through to generic, literal-driven
/// emission, structurally — for every other shape: field-vs-field,
/// function-wrapped fields, arithmetic, `== null`, unpinned fields, and
/// any form the rule table leaves native.
///
/// Both operand orders are accepted for the comparison operators
/// (`400 < status` is `status > 400`); for the pattern operators
/// (`matches`/LIKE/ILIKE) only the LEFT operand is a subject — the right
/// operand is the pattern, not a comparison target.
///
/// NULL policy is [`NullPolicy::Strict`]: plain SQL null propagation,
/// exactly what the pin-blind `where` answers — the search stage's
/// `OR field IS NULL` widening for `!=` deliberately does not apply here,
/// or a repin would change missing-field semantics.
fn try_pinned_comparison(
    lhs: &Spanned<Expr>,
    op: BinaryOp,
    rhs: &Spanned<Expr>,
    state: &mut EmitterState,
) -> Option<String> {
    // Pattern operators: field on the LEFT only.
    if matches!(op, BinaryOp::Matches | BinaryOp::Like | BinaryOp::ILike) {
        let Expr::FieldRef(name) = &lhs.node else {
            return None;
        };
        let Expr::Literal(LiteralValue::String(pattern)) = &rhs.node else {
            return None;
        };
        let pin = state.compare_pin(name)?;
        if compare::pattern_form(Some(pin)) == PatternForm::Native {
            // The column is already text — generic emission is
            // byte-identical, so keep it on the generic path.
            return None;
        }
        let target = pattern_target(&quote_field(name), Some(pin));
        let placeholder = state.push_param(SqlValue::String(pattern.clone()));
        return Some(match op {
            BinaryOp::Matches => format!("regexp_matches({target}, {placeholder})"),
            BinaryOp::Like => format!("({target} LIKE {placeholder})"),
            _ => format!("({target} ILIKE {placeholder})"),
        });
    }

    let filter_op = comparison_filter_op(op)?;
    let (name, filter_op, literal) = match (&lhs.node, &rhs.node) {
        (Expr::FieldRef(name), rhs) => (name, filter_op, bare_literal(rhs)?),
        (lhs, Expr::FieldRef(name)) => (name, flip_filter_op(filter_op), bare_literal(lhs)?),
        _ => return None,
    };
    let pin = state.compare_pin(name)?;
    let form = compare::compare_form_bound(Some(pin), filter_op, &literal)?;
    if matches!(form, CompareForm::Native(_)) {
        // The rule table leaves the shape literal-driven (VARCHAR pin,
        // ordered non-numeric literal) — generic emission is the rule.
        return None;
    }
    let clause = comparison_sql(
        &quote_field(name),
        filter_op,
        form,
        NullPolicy::Strict,
        state,
    );
    // Parenthesize for composition under and/or/not, matching the generic
    // emitter's style; the two-armed shapes arrive parenthesized already.
    if clause.starts_with('(') {
        Some(clause)
    } else {
        Some(format!("({clause})"))
    }
}

/// Detect `field in (literal, …)` over a pinned field and emit each
/// element through the equality rule, mirroring the search stage's IN
/// list (ADR-0011 slice A′).
fn try_pinned_in_list(
    target: &Spanned<Expr>,
    list: &[Spanned<Expr>],
    state: &mut EmitterState,
) -> Option<String> {
    let Expr::FieldRef(name) = &target.node else {
        return None;
    };
    let pin = state.compare_pin(name)?;
    let forms: Vec<CompareForm> = list
        .iter()
        .map(|item| {
            let element = bare_literal(&item.node)?;
            compare::compare_form_bound(Some(pin), FilterOp::Eq, &element)
        })
        .collect::<Option<_>>()?;
    let clause = in_list_sql(&quote_field(name), forms, state);
    if clause.starts_with('(') {
        Some(clause)
    } else {
        Some(format!("({clause})"))
    }
}

/// Detect `level <cmp> "token"` (either operand order) and emit the
/// severity band predicate. Returns `Ok(None)` when the expression is not
/// a level comparison and should take the generic path.
fn try_level_comparison(
    lhs: &Spanned<Expr>,
    op: BinaryOp,
    rhs: &Spanned<Expr>,
) -> Result<Option<String>, EmitError> {
    match super::severity::as_level_comparison(lhs, op, rhs) {
        Some((filter_op, token)) => super::severity::level_predicate(filter_op, token).map(Some),
        None => Ok(None),
    }
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
    if op == BinaryOp::Matches {
        return format!("regexp_matches({lhs}, {rhs})");
    }
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
        BinaryOp::Like => "LIKE",
        BinaryOp::ILike => "ILIKE",
        BinaryOp::Matches => unreachable!(),
    };
    format!("({lhs} {sql_op} {rhs})")
}

fn emit_unary(op: UnaryOp, inner: &str) -> String {
    match op {
        UnaryOp::Not => format!("(NOT {inner})"),
        UnaryOp::Neg => format!("(-{inner})"),
    }
}
