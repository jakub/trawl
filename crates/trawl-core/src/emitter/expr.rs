// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::ast::{BinaryOp, Expr, LiteralValue, Spanned, UnaryOp};

use super::EmitError;
use super::SqlValue;
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
            let lhs = emit_expr(target, state)?;
            let items: Vec<String> = list
                .iter()
                .map(|item| emit_expr(item, state))
                .collect::<Result<_, _>>()?;
            Ok(format!("({lhs} IN ({}))", items.join(", ")))
        }
    }
}

fn emit_literal(lit: &LiteralValue, state: &mut EmitterState) -> String {
    match lit {
        LiteralValue::Null => "NULL".to_string(),
        LiteralValue::String(s) => state.push_param(SqlValue::String(s.clone())),
        LiteralValue::Int(n) => state.push_param(SqlValue::Int(*n)),
        LiteralValue::Float(n) => state.push_param(SqlValue::Float(*n)),
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
