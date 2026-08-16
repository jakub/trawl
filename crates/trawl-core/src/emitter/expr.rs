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
    format_literal_position, json_read_positions, literal_int_positions, literal_text_positions,
    translate_function, unit_literal_positions, validate_format_literal, validate_function_arity,
    validate_unit_literal,
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
            // Bare field-vs-literal comparisons consult the pin scope
            // (ADR-0011 slice A′) — same rule table as the search stage.
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
            let lit_positions = literal_int_positions(name);
            let unit_positions = unit_literal_positions(name);
            let text_positions = literal_text_positions(name);
            let json_positions = json_read_positions(name);
            let fmt_position = format_literal_position(name);
            if !lit_positions.is_empty() || !unit_positions.is_empty() {
                // Arity BEFORE vocabulary: a call carrying an argument the
                // function does not have must say so, rather than
                // complaining about a literal at a position that is not a
                // literal position at all. Scoped to the functions with
                // literal positions because only they inspect an argument
                // before `translate_function`'s own arity guard runs — and
                // for those the table's message is the guard's, word for
                // word.
                validate_function_arity(name, args.len())?;
            }
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
                        if text_positions.contains(&i) {
                            // The token CHOOSES the emitted expression
                            // (`sev()`'s dialect), so it is never a bound
                            // parameter: `translate_function` has to read
                            // it. Folded here, once, so both the SQL and
                            // the eval lane see one spelling.
                            Ok(raw
                                .expect("a non-literal was refused above")
                                .to_ascii_lowercase())
                        } else {
                            emit_expr(a, state)
                        }
                    } else if json_positions.contains(&i)
                        && let Some(literal) = bare_literal(&a.node)
                    {
                        // Read through `to_json` (`sev()`'s subject): a
                        // bare parameter has no type for `DuckDB` to
                        // infer there, so a literal carries the type it
                        // binds as anyway.
                        Ok(typed_literal(&literal, state))
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

/// Detect a pinned-subject-vs-literal comparison and emit it through the
/// shared rule table (ADR-0011 slice A′, widened by ADR-0013 ruling 9).
/// The subject is whatever [`PinScope::subject_pin`] recognizes — a bare
/// pinned field, or a pin-DECLARING call over one (`sev(level)`) — and
/// every other shape returns `None`, falling through to generic,
/// literal-driven emission, structurally: field-vs-field, an ordinary
/// function wrapper, arithmetic, `== null`, unpinned fields, and any form
/// the rule table leaves native.
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
) -> Result<Option<String>, EmitError> {
    // Pattern operators: the subject is the LEFT operand only.
    if matches!(op, BinaryOp::Matches | BinaryOp::Like | BinaryOp::ILike) {
        let Some((subject, pin)) = state.pin_scope().subject_pin(lhs) else {
            return Ok(None);
        };
        let Expr::Literal(LiteralValue::String(pattern)) = &rhs.node else {
            return Ok(None);
        };
        if compare::pattern_form(Some(pin)) == PatternForm::Native {
            // The column is already text — generic emission is
            // byte-identical, so keep it on the generic path.
            return Ok(None);
        }
        // The subject's SQL is built BEFORE the pattern's parameter, so
        // any placeholder inside it keeps its positional order.
        let target = pattern_target(&subject_sql(&subject, state)?, Some(pin));
        let placeholder = state.push_param(SqlValue::String(pattern.clone()));
        return Ok(Some(match op {
            BinaryOp::Matches => format!("regexp_matches({target}, {placeholder})"),
            BinaryOp::Like => format!("({target} LIKE {placeholder})"),
            _ => format!("({target} ILIKE {placeholder})"),
        }));
    }

    let Some(filter_op) = comparison_filter_op(op) else {
        return Ok(None);
    };
    let resolved = match (
        state.pin_scope().subject_pin(lhs),
        state.pin_scope().subject_pin(rhs),
    ) {
        (Some((subject, pin)), _) => bare_literal(&rhs.node).map(|l| (subject, pin, filter_op, l)),
        // `400 < status` is `status > 400`, subject on the right.
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
        // The rule table leaves the shape literal-driven (VARCHAR pin,
        // ordered non-numeric literal) — generic emission is the rule.
        return Ok(None);
    }
    // Subject first, again for parameter order.
    let target = subject_sql(&subject, state)?;
    let clause = comparison_sql(&target, filter_op, form, NullPolicy::Strict, state);
    // Parenthesize for composition under and/or/not, matching the generic
    // emitter's style; the two-armed shapes arrive parenthesized already.
    Ok(Some(if clause.starts_with('(') {
        clause
    } else {
        format!("({clause})")
    }))
}

/// Detect `field in (literal, …)` over a pinned field and emit each
/// element through the equality rule, mirroring the search stage's IN
/// list (ADR-0011 slice A′).
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
    // Subject first: `in_list_sql` pushes one parameter per element.
    let subject = subject_sql(&subject, state)?;
    let clause = in_list_sql(&subject, forms, state);
    Ok(Some(if clause.starts_with('(') {
        clause
    } else {
        format!("({clause})")
    }))
}

/// The SQL a pinned subject compares as: the quoted column, or the
/// translated call.
///
/// A pin-declaring call is emitted by the ordinary function path — it is
/// the same SQL `| let s = sev(level)` would project — so the comparison
/// and the projection can never read one value two ways.
fn subject_sql(
    subject: &crate::pin_scope::PinnedSubject<'_>,
    state: &mut EmitterState,
) -> Result<String, EmitError> {
    match subject {
        crate::pin_scope::PinnedSubject::Field(name) => Ok(quote_field(name)),
        crate::pin_scope::PinnedSubject::Call(call) => emit_expr(call, state),
    }
}

/// A literal bound as a parameter that carries its OWN type.
///
/// For the positions read through `to_json`
/// ([`super::functions::json_read_positions`]) — `DuckDB` infers a
/// parameter's type from its surroundings, and `to_json(?)` offers none,
/// so the statement fails to prepare. The cast names exactly the type the
/// value binds as, so nothing about the reading changes; a NULL takes
/// VARCHAR, the type every text reading starts from.
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
