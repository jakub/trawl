// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! In-memory expression evaluator for streaming pipeline stages.
//!
//! Evaluates `Expr` AST nodes against a `serde_json::Map` event
//! rather than emitting SQL. This is the runtime analog of
//! `emitter/expr.rs`.

use crate::ast::{BinaryOp, Expr, LiteralValue, Spanned, UnaryOp};
use crate::emitter::map_field_name;
use serde_json::{Map, Value};

/// Result of evaluating an expression against an event.
#[derive(Debug, Clone, PartialEq)]
pub enum EvalValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<EvalValue>),
}

impl EvalValue {
    /// Truthiness check (SQL-style: `Null` and `false` are falsy).
    pub fn is_truthy(&self) -> bool {
        match self {
            Self::Null => false,
            Self::Bool(b) => *b,
            Self::Int(n) => *n != 0,
            Self::Float(n) => *n != 0.0,
            Self::Str(s) => !s.is_empty(),
            Self::Array(a) => !a.is_empty(),
        }
    }

    /// Try to extract a numeric value as f64.
    #[allow(clippy::cast_precision_loss)]
    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int(n) => Some(*n as f64),
            Self::Float(n) => Some(*n),
            _ => None,
        }
    }

    /// Try to coerce to a string representation.
    fn as_str_repr(&self) -> Option<String> {
        match self {
            Self::Str(s) => Some(s.clone()),
            Self::Int(n) => Some(n.to_string()),
            Self::Float(n) => Some(n.to_string()),
            Self::Bool(b) => Some(b.to_string()),
            Self::Null | Self::Array(_) => None,
        }
    }
}

impl From<EvalValue> for Value {
    fn from(v: EvalValue) -> Self {
        match v {
            EvalValue::Null => Value::Null,
            EvalValue::Bool(b) => Value::Bool(b),
            EvalValue::Int(n) => Value::Number(n.into()),
            EvalValue::Float(n) => {
                serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
            }
            EvalValue::Str(s) => Value::String(s),
            EvalValue::Array(a) => Value::Array(a.into_iter().map(Value::from).collect()),
        }
    }
}

impl From<&Value> for EvalValue {
    fn from(v: &Value) -> Self {
        match v {
            Value::Bool(b) => Self::Bool(*b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Self::Int(i)
                } else if let Some(f) = n.as_f64() {
                    Self::Float(f)
                } else {
                    Self::Null
                }
            }
            Value::String(s) => Self::Str(s.clone()),
            Value::Array(a) => Self::Array(a.iter().map(Self::from).collect()),
            Value::Null | Value::Object(_) => Self::Null,
        }
    }
}

/// Evaluate an expression AST node against an event map.
pub fn eval_expr(expr: &Spanned<Expr>, event: &Map<String, Value>) -> EvalValue {
    match &expr.node {
        Expr::Literal(lit) => eval_literal(lit),
        Expr::FieldRef(name) => {
            let mapped = map_field_name(name);
            event.get(mapped).map_or(EvalValue::Null, EvalValue::from)
        }
        Expr::Binary { lhs, op, rhs } => {
            eval_binary(&eval_expr(lhs, event), *op, &eval_expr(rhs, event))
        }
        Expr::Unary { op, operand } => eval_unary(*op, eval_expr(operand, event)),
        Expr::FunctionCall { name, args } => {
            let evaluated: Vec<EvalValue> = args.iter().map(|a| eval_expr(a, event)).collect();
            eval_scalar_fn(name, &evaluated)
        }
        Expr::InList { expr: target, list } => {
            let target_val = eval_expr(target, event);
            if matches!(target_val, EvalValue::Null) {
                return EvalValue::Null;
            }
            for item in list {
                let item_val = eval_expr(item, event);
                if eval_eq(&target_val, &item_val) == EvalValue::Bool(true) {
                    return EvalValue::Bool(true);
                }
            }
            EvalValue::Bool(false)
        }
    }
}

fn eval_literal(lit: &LiteralValue) -> EvalValue {
    match lit {
        LiteralValue::Null => EvalValue::Null,
        LiteralValue::Bool(b) => EvalValue::Bool(*b),
        LiteralValue::Int(n) => EvalValue::Int(*n),
        LiteralValue::Float(n) => EvalValue::Float(*n),
        LiteralValue::String(s) => EvalValue::Str(s.clone()),
    }
}

// ── binary operations ──────────────────────────────────────────────

fn eval_binary(lhs: &EvalValue, op: BinaryOp, rhs: &EvalValue) -> EvalValue {
    // short-circuit logical ops (they handle Null specially)
    match op {
        BinaryOp::And => return eval_and(lhs, rhs),
        BinaryOp::Or => return eval_or(lhs, rhs),
        _ => {}
    }

    // null propagation for everything else
    if matches!(lhs, EvalValue::Null) || matches!(rhs, EvalValue::Null) {
        return EvalValue::Null;
    }

    match op {
        BinaryOp::Add => eval_arithmetic(lhs, rhs, |a, b| a + b, |a, b| a + b),
        BinaryOp::Sub => eval_arithmetic(lhs, rhs, |a, b| a - b, |a, b| a - b),
        BinaryOp::Mul => eval_arithmetic(lhs, rhs, |a, b| a * b, |a, b| a * b),
        BinaryOp::Div => eval_div(lhs, rhs),
        BinaryOp::Mod => eval_mod(lhs, rhs),
        BinaryOp::Eq => eval_eq(lhs, rhs),
        BinaryOp::Ne => match eval_eq(lhs, rhs) {
            EvalValue::Bool(b) => EvalValue::Bool(!b),
            other => other,
        },
        BinaryOp::Gt => eval_cmp(lhs, rhs, |o| o == std::cmp::Ordering::Greater),
        BinaryOp::Gte => eval_cmp(lhs, rhs, |o| {
            o == std::cmp::Ordering::Greater || o == std::cmp::Ordering::Equal
        }),
        BinaryOp::Lt => eval_cmp(lhs, rhs, |o| o == std::cmp::Ordering::Less),
        BinaryOp::Lte => eval_cmp(lhs, rhs, |o| {
            o == std::cmp::Ordering::Less || o == std::cmp::Ordering::Equal
        }),
        BinaryOp::Matches => eval_matches(lhs, rhs),
        BinaryOp::Like => eval_like(lhs, rhs, false),
        BinaryOp::ILike => eval_like(lhs, rhs, true),
        BinaryOp::And | BinaryOp::Or => unreachable!(),
    }
}

fn eval_and(lhs: &EvalValue, rhs: &EvalValue) -> EvalValue {
    // SQL three-valued logic: false AND anything = false
    if matches!(lhs, EvalValue::Bool(false)) {
        return EvalValue::Bool(false);
    }
    if matches!(rhs, EvalValue::Bool(false)) {
        return EvalValue::Bool(false);
    }
    if matches!(lhs, EvalValue::Null) || matches!(rhs, EvalValue::Null) {
        return EvalValue::Null;
    }
    EvalValue::Bool(lhs.is_truthy() && rhs.is_truthy())
}

fn eval_or(lhs: &EvalValue, rhs: &EvalValue) -> EvalValue {
    // SQL three-valued logic: true OR anything = true
    if lhs.is_truthy() {
        return EvalValue::Bool(true);
    }
    if rhs.is_truthy() {
        return EvalValue::Bool(true);
    }
    if matches!(lhs, EvalValue::Null) || matches!(rhs, EvalValue::Null) {
        return EvalValue::Null;
    }
    EvalValue::Bool(false)
}

fn eval_arithmetic(
    lhs: &EvalValue,
    rhs: &EvalValue,
    int_op: impl FnOnce(i64, i64) -> i64,
    float_op: impl FnOnce(f64, f64) -> f64,
) -> EvalValue {
    match (lhs, rhs) {
        (EvalValue::Int(a), EvalValue::Int(b)) => EvalValue::Int(int_op(*a, *b)),
        _ => match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => EvalValue::Float(float_op(a, b)),
            _ => EvalValue::Null,
        },
    }
}

fn eval_div(lhs: &EvalValue, rhs: &EvalValue) -> EvalValue {
    match (lhs, rhs) {
        (EvalValue::Int(a), EvalValue::Int(b)) => {
            if *b == 0 {
                EvalValue::Null
            } else {
                EvalValue::Int(a / b)
            }
        }
        _ => match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => {
                if b == 0.0 {
                    EvalValue::Null
                } else {
                    EvalValue::Float(a / b)
                }
            }
            _ => EvalValue::Null,
        },
    }
}

fn eval_mod(lhs: &EvalValue, rhs: &EvalValue) -> EvalValue {
    match (lhs, rhs) {
        (EvalValue::Int(a), EvalValue::Int(b)) => {
            if *b == 0 {
                EvalValue::Null
            } else {
                EvalValue::Int(a % b)
            }
        }
        _ => match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => {
                if b == 0.0 {
                    EvalValue::Null
                } else {
                    EvalValue::Float(a % b)
                }
            }
            _ => EvalValue::Null,
        },
    }
}

fn eval_eq(lhs: &EvalValue, rhs: &EvalValue) -> EvalValue {
    match (lhs, rhs) {
        (EvalValue::Int(a), EvalValue::Int(b)) => EvalValue::Bool(a == b),
        (EvalValue::Bool(a), EvalValue::Bool(b)) => EvalValue::Bool(a == b),
        (EvalValue::Str(a), EvalValue::Str(b)) => EvalValue::Bool(a == b),
        // cross-type numeric comparison
        _ => match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => EvalValue::Bool(a == b),
            // string coercion: compare as strings if one side is a string
            _ => match (lhs.as_str_repr(), rhs.as_str_repr()) {
                (Some(a), Some(b)) => EvalValue::Bool(a == b),
                _ => EvalValue::Null,
            },
        },
    }
}

fn eval_cmp(
    lhs: &EvalValue,
    rhs: &EvalValue,
    pred: impl FnOnce(std::cmp::Ordering) -> bool,
) -> EvalValue {
    let ordering = match (lhs, rhs) {
        (EvalValue::Int(a), EvalValue::Int(b)) => Some(a.cmp(b)),
        (EvalValue::Str(a), EvalValue::Str(b)) => Some(a.cmp(b)),
        _ => match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => a.partial_cmp(&b), // both are values, not refs
            _ => None,
        },
    };
    ordering.map_or(EvalValue::Null, |o| EvalValue::Bool(pred(o)))
}

fn eval_matches(lhs: &EvalValue, rhs: &EvalValue) -> EvalValue {
    match (lhs, rhs) {
        (EvalValue::Str(text), EvalValue::Str(pattern)) => {
            // compile regex on the fly — for hot-path use, the caller
            // should pre-compile via `CompiledStage`
            regex::Regex::new(pattern).map_or(EvalValue::Bool(false), |re| {
                EvalValue::Bool(re.is_match(text))
            })
        }
        _ => EvalValue::Null,
    }
}

/// Evaluate SQL LIKE/ILIKE pattern matching.
///
/// `%` matches any sequence, `_` matches a single character.
fn eval_like(lhs: &EvalValue, rhs: &EvalValue, case_insensitive: bool) -> EvalValue {
    match (lhs, rhs) {
        (EvalValue::Str(text), EvalValue::Str(pattern)) => {
            let mut regex_str = String::from("^");
            for ch in pattern.chars() {
                match ch {
                    '%' => regex_str.push_str(".*"),
                    '_' => regex_str.push('.'),
                    // escape regex metacharacters
                    '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
                    | '\\' => {
                        regex_str.push('\\');
                        regex_str.push(ch);
                    }
                    _ => regex_str.push(ch),
                }
            }
            regex_str.push('$');
            if case_insensitive {
                regex_str.insert_str(0, "(?i)");
            }
            regex::Regex::new(&regex_str).map_or(EvalValue::Bool(false), |re| {
                EvalValue::Bool(re.is_match(text))
            })
        }
        _ => EvalValue::Null,
    }
}

// ── unary operations ───────────────────────────────────────────────

fn eval_unary(op: UnaryOp, operand: EvalValue) -> EvalValue {
    match op {
        UnaryOp::Not => match operand {
            EvalValue::Null => EvalValue::Null,
            EvalValue::Bool(b) => EvalValue::Bool(!b),
            other => EvalValue::Bool(!other.is_truthy()),
        },
        UnaryOp::Neg => match operand {
            EvalValue::Int(n) => EvalValue::Int(-n),
            EvalValue::Float(n) => EvalValue::Float(-n),
            _ => EvalValue::Null,
        },
    }
}

// ── scalar functions ───────────────────────────────────────────────

fn eval_scalar_fn(name: &str, args: &[EvalValue]) -> EvalValue {
    match name {
        // string
        "lower" => unary_str(args, str::to_lowercase),
        "upper" => unary_str(args, str::to_uppercase),
        #[allow(clippy::cast_possible_wrap)]
        "length" | "len" => args.first().map_or(EvalValue::Null, |v| match v {
            EvalValue::Str(s) => EvalValue::Int(s.len() as i64),
            _ => EvalValue::Null,
        }),
        "trim" => unary_str(args, |s| s.trim().to_string()),
        "ltrim" => unary_str(args, |s| s.trim_start().to_string()),
        "rtrim" => unary_str(args, |s| s.trim_end().to_string()),
        "replace" => {
            if args.len() != 3 {
                return EvalValue::Null;
            }
            match (&args[0], &args[1], &args[2]) {
                (EvalValue::Str(s), EvalValue::Str(from), EvalValue::Str(to)) => {
                    EvalValue::Str(s.replace(from.as_str(), to.as_str()))
                }
                _ => EvalValue::Null,
            }
        }
        "substr" => eval_substr(args),

        // numeric
        "abs" => args.first().map_or(EvalValue::Null, |v| match v {
            EvalValue::Int(n) => EvalValue::Int(n.abs()),
            EvalValue::Float(n) => EvalValue::Float(n.abs()),
            _ => EvalValue::Null,
        }),
        "ceil" | "ceiling" => eval_ceil(args),
        "floor" => eval_floor(args),
        "round" => eval_round(args),

        // conditional / type
        "if" => {
            if args.len() != 3 {
                return EvalValue::Null;
            }
            if args[0].is_truthy() {
                args[1].clone()
            } else {
                args[2].clone()
            }
        }
        "coalesce" => {
            for arg in args {
                if !matches!(arg, EvalValue::Null) {
                    return arg.clone();
                }
            }
            EvalValue::Null
        }
        "isnull" => args.first().map_or(EvalValue::Null, |v| {
            EvalValue::Bool(matches!(v, EvalValue::Null))
        }),
        "isnotnull" => args.first().map_or(EvalValue::Null, |v| {
            EvalValue::Bool(!matches!(v, EvalValue::Null))
        }),
        "typeof" => args.first().map_or(EvalValue::Null, |v| {
            EvalValue::Str(
                match v {
                    EvalValue::Null => "NULL",
                    EvalValue::Bool(_) => "BOOLEAN",
                    EvalValue::Int(_) => "INTEGER",
                    EvalValue::Float(_) => "DOUBLE",
                    EvalValue::Str(_) => "VARCHAR",
                    EvalValue::Array(_) => "ARRAY",
                }
                .to_string(),
            )
        }),
        "now" => {
            let now = chrono::Utc::now();
            EvalValue::Str(now.to_rfc3339())
        }

        _ => EvalValue::Null,
    }
}

fn unary_str(args: &[EvalValue], f: impl FnOnce(&str) -> String) -> EvalValue {
    args.first().map_or(EvalValue::Null, |v| match v {
        EvalValue::Str(s) => EvalValue::Str(f(s)),
        _ => EvalValue::Null,
    })
}

#[allow(clippy::cast_possible_truncation)]
fn eval_ceil(args: &[EvalValue]) -> EvalValue {
    args.first().map_or(EvalValue::Null, |v| match v {
        EvalValue::Int(n) => EvalValue::Int(*n),
        EvalValue::Float(n) => EvalValue::Int(n.ceil() as i64),
        _ => EvalValue::Null,
    })
}

#[allow(clippy::cast_possible_truncation)]
fn eval_floor(args: &[EvalValue]) -> EvalValue {
    args.first().map_or(EvalValue::Null, |v| match v {
        EvalValue::Int(n) => EvalValue::Int(*n),
        EvalValue::Float(n) => EvalValue::Int(n.floor() as i64),
        _ => EvalValue::Null,
    })
}

#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn eval_substr(args: &[EvalValue]) -> EvalValue {
    if args.len() < 2 || args.len() > 3 {
        return EvalValue::Null;
    }
    let EvalValue::Str(s) = &args[0] else {
        return EvalValue::Null;
    };
    let EvalValue::Int(start) = args[1] else {
        return EvalValue::Null;
    };
    // SQL SUBSTR is 1-indexed
    let start_idx = if start < 1 { 0 } else { (start - 1) as usize };
    let chars: Vec<char> = s.chars().collect();

    if start_idx >= chars.len() {
        return EvalValue::Str(String::new());
    }

    if args.len() == 3 {
        let len = match &args[2] {
            EvalValue::Int(n) => {
                if *n < 0 {
                    return EvalValue::Str(String::new());
                }
                *n as usize
            }
            _ => return EvalValue::Null,
        };
        let end = (start_idx + len).min(chars.len());
        EvalValue::Str(chars[start_idx..end].iter().collect())
    } else {
        EvalValue::Str(chars[start_idx..].iter().collect())
    }
}

#[allow(clippy::cast_possible_truncation)]
fn eval_round(args: &[EvalValue]) -> EvalValue {
    if args.is_empty() || args.len() > 2 {
        return EvalValue::Null;
    }
    let val = match &args[0] {
        EvalValue::Int(n) => return EvalValue::Int(*n),
        EvalValue::Float(n) => *n,
        _ => return EvalValue::Null,
    };
    let precision = if args.len() == 2 {
        match &args[1] {
            EvalValue::Int(n) => *n,
            _ => return EvalValue::Null,
        }
    } else {
        0
    };

    if precision == 0 {
        EvalValue::Int(val.round() as i64)
    } else {
        let factor = 10_f64.powi(precision as i32);
        EvalValue::Float((val * factor).round() / factor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{BinaryOp, Expr, LiteralValue, Spanned, UnaryOp};
    use serde_json::json;

    fn span<T>(node: T) -> Spanned<T> {
        Spanned { node, span: 0..0 }
    }

    fn lit_int(n: i64) -> Spanned<Expr> {
        span(Expr::Literal(LiteralValue::Int(n)))
    }

    fn lit_float(n: f64) -> Spanned<Expr> {
        span(Expr::Literal(LiteralValue::Float(n)))
    }

    fn lit_str(s: &str) -> Spanned<Expr> {
        span(Expr::Literal(LiteralValue::String(s.to_string())))
    }

    fn lit_bool(b: bool) -> Spanned<Expr> {
        span(Expr::Literal(LiteralValue::Bool(b)))
    }

    fn lit_null() -> Spanned<Expr> {
        span(Expr::Literal(LiteralValue::Null))
    }

    fn field(name: &str) -> Spanned<Expr> {
        span(Expr::FieldRef(name.to_string()))
    }

    fn binary(lhs: Spanned<Expr>, op: BinaryOp, rhs: Spanned<Expr>) -> Spanned<Expr> {
        span(Expr::Binary {
            lhs: Box::new(lhs),
            op,
            rhs: Box::new(rhs),
        })
    }

    fn unary(op: UnaryOp, operand: Spanned<Expr>) -> Spanned<Expr> {
        span(Expr::Unary {
            op,
            operand: Box::new(operand),
        })
    }

    fn call(name: &str, args: Vec<Spanned<Expr>>) -> Spanned<Expr> {
        span(Expr::FunctionCall {
            name: name.to_string(),
            args,
        })
    }

    fn in_list(expr: Spanned<Expr>, list: Vec<Spanned<Expr>>) -> Spanned<Expr> {
        span(Expr::InList {
            expr: Box::new(expr),
            list,
        })
    }

    fn empty_event() -> Map<String, Value> {
        Map::new()
    }

    fn event(pairs: &Value) -> Map<String, Value> {
        pairs.as_object().unwrap().clone()
    }

    // ── literals ───────────────────────────────────────────────────

    #[test]
    fn literal_int() {
        assert_eq!(eval_expr(&lit_int(42), &empty_event()), EvalValue::Int(42));
    }

    #[test]
    fn literal_float() {
        assert_eq!(
            eval_expr(&lit_float(1.5), &empty_event()),
            EvalValue::Float(1.5)
        );
    }

    #[test]
    fn literal_string() {
        assert_eq!(
            eval_expr(&lit_str("hello"), &empty_event()),
            EvalValue::Str("hello".to_string())
        );
    }

    #[test]
    fn literal_bool() {
        assert_eq!(
            eval_expr(&lit_bool(true), &empty_event()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn literal_null() {
        assert_eq!(eval_expr(&lit_null(), &empty_event()), EvalValue::Null);
    }

    // ── field refs ─────────────────────────────────────────────────

    #[test]
    fn field_ref_present() {
        let ev = event(&json!({"host": "web-1"}));
        assert_eq!(
            eval_expr(&field("host"), &ev),
            EvalValue::Str("web-1".to_string())
        );
    }

    #[test]
    fn field_ref_missing() {
        assert_eq!(eval_expr(&field("host"), &empty_event()), EvalValue::Null);
    }

    #[test]
    fn field_ref_timestamp_mapping() {
        let ev = event(&json!({"timestamp": "2026-01-01T00:00:00Z"}));
        assert_eq!(
            eval_expr(&field("@timestamp"), &ev),
            EvalValue::Str("2026-01-01T00:00:00Z".to_string())
        );
    }

    #[test]
    fn field_ref_time_alias() {
        let ev = event(&json!({"timestamp": "2026-01-01T00:00:00Z"}));
        assert_eq!(
            eval_expr(&field("_time"), &ev),
            EvalValue::Str("2026-01-01T00:00:00Z".to_string())
        );
    }

    #[test]
    fn field_ref_numeric() {
        let ev = event(&json!({"status": 200}));
        assert_eq!(eval_expr(&field("status"), &ev), EvalValue::Int(200));
    }

    // ── arithmetic ─────────────────────────────────────────────────

    #[test]
    fn add_ints() {
        let expr = binary(lit_int(2), BinaryOp::Add, lit_int(3));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(5));
    }

    #[test]
    fn add_int_float() {
        let expr = binary(lit_int(2), BinaryOp::Add, lit_float(1.5));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(3.5));
    }

    #[test]
    fn sub_ints() {
        let expr = binary(lit_int(10), BinaryOp::Sub, lit_int(3));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(7));
    }

    #[test]
    fn mul_ints() {
        let expr = binary(lit_int(4), BinaryOp::Mul, lit_int(5));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(20));
    }

    #[test]
    fn div_ints() {
        let expr = binary(lit_int(10), BinaryOp::Div, lit_int(3));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(3));
    }

    #[test]
    fn div_by_zero_int() {
        let expr = binary(lit_int(10), BinaryOp::Div, lit_int(0));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn div_floats() {
        let expr = binary(lit_float(10.0), BinaryOp::Div, lit_float(4.0));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(2.5));
    }

    #[test]
    fn div_by_zero_float() {
        let expr = binary(lit_float(10.0), BinaryOp::Div, lit_float(0.0));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn modulo_ints() {
        let expr = binary(lit_int(10), BinaryOp::Mod, lit_int(3));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(1));
    }

    #[test]
    fn modulo_by_zero() {
        let expr = binary(lit_int(10), BinaryOp::Mod, lit_int(0));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn arithmetic_null_propagation() {
        let expr = binary(lit_int(5), BinaryOp::Add, lit_null());
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn arithmetic_with_field() {
        let ev = event(&json!({"duration": 100}));
        let expr = binary(field("duration"), BinaryOp::Mul, lit_int(1000));
        assert_eq!(eval_expr(&expr, &ev), EvalValue::Int(100_000));
    }

    // ── comparison ─────────────────────────────────────────────────

    #[test]
    fn eq_ints() {
        let expr = binary(lit_int(5), BinaryOp::Eq, lit_int(5));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn eq_int_float_cross() {
        let expr = binary(lit_int(5), BinaryOp::Eq, lit_float(5.0));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn ne_ints() {
        let expr = binary(lit_int(5), BinaryOp::Ne, lit_int(3));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn gt_ints() {
        let expr = binary(lit_int(5), BinaryOp::Gt, lit_int(3));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn gt_false() {
        let expr = binary(lit_int(3), BinaryOp::Gt, lit_int(5));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn gte_equal() {
        let expr = binary(lit_int(5), BinaryOp::Gte, lit_int(5));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn lt_ints() {
        let expr = binary(lit_int(3), BinaryOp::Lt, lit_int(5));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn lte_equal() {
        let expr = binary(lit_int(5), BinaryOp::Lte, lit_int(5));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn cmp_strings() {
        let expr = binary(lit_str("apple"), BinaryOp::Lt, lit_str("banana"));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn cmp_null_propagation() {
        let expr = binary(lit_int(5), BinaryOp::Gt, lit_null());
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    // ── logical ops ────────────────────────────────────────────────

    #[test]
    fn and_true_true() {
        let expr = binary(lit_bool(true), BinaryOp::And, lit_bool(true));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn and_true_false() {
        let expr = binary(lit_bool(true), BinaryOp::And, lit_bool(false));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn and_false_null() {
        // false AND null = false (SQL short-circuit)
        let expr = binary(lit_bool(false), BinaryOp::And, lit_null());
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn and_null_true() {
        let expr = binary(lit_null(), BinaryOp::And, lit_bool(true));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn or_true_null() {
        let expr = binary(lit_bool(true), BinaryOp::Or, lit_null());
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn or_false_false() {
        let expr = binary(lit_bool(false), BinaryOp::Or, lit_bool(false));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn or_null_false() {
        let expr = binary(lit_null(), BinaryOp::Or, lit_bool(false));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    // ── matches ────────────────────────────────────────────────────

    #[test]
    fn matches_regex() {
        let expr = binary(
            lit_str("prod-web-01"),
            BinaryOp::Matches,
            lit_str("prod-.*"),
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn matches_no_match() {
        let expr = binary(
            lit_str("staging-01"),
            BinaryOp::Matches,
            lit_str("^prod-.*"),
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    // ── like / ilike ────────────────────────────────────────────────

    #[test]
    fn like_percent_wildcard() {
        let expr = binary(lit_str("prod-web-01"), BinaryOp::Like, lit_str("prod-%"));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn like_no_match() {
        let expr = binary(lit_str("staging-01"), BinaryOp::Like, lit_str("prod-%"));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn like_underscore_wildcard() {
        let expr = binary(lit_str("a1"), BinaryOp::Like, lit_str("a_"));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn like_case_sensitive() {
        let expr = binary(lit_str("PROD-01"), BinaryOp::Like, lit_str("prod-%"));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn ilike_case_insensitive() {
        let expr = binary(lit_str("PROD-01"), BinaryOp::ILike, lit_str("prod-%"));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn like_null_propagation() {
        let expr = binary(lit_null(), BinaryOp::Like, lit_str("prod-%"));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    // ── unary ──────────────────────────────────────────────────────

    #[test]
    fn not_true() {
        let expr = unary(UnaryOp::Not, lit_bool(true));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn not_false() {
        let expr = unary(UnaryOp::Not, lit_bool(false));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn not_null() {
        let expr = unary(UnaryOp::Not, lit_null());
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn neg_int() {
        let expr = unary(UnaryOp::Neg, lit_int(42));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(-42));
    }

    #[test]
    fn neg_float() {
        let expr = unary(UnaryOp::Neg, lit_float(1.5));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(-1.5));
    }

    // ── in list ────────────────────────────────────────────────────

    #[test]
    fn in_list_found() {
        let expr = in_list(lit_int(200), vec![lit_int(200), lit_int(301), lit_int(404)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn in_list_not_found() {
        let expr = in_list(lit_int(500), vec![lit_int(200), lit_int(301)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn in_list_null_target() {
        let expr = in_list(lit_null(), vec![lit_int(200)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    // ── scalar functions: string ───────────────────────────────────

    #[test]
    fn fn_lower() {
        let expr = call("lower", vec![lit_str("HELLO")]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("hello".to_string())
        );
    }

    #[test]
    fn fn_upper() {
        let expr = call("upper", vec![lit_str("hello")]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("HELLO".to_string())
        );
    }

    #[test]
    fn fn_length() {
        let expr = call("length", vec![lit_str("hello")]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(5));
    }

    #[test]
    fn fn_len_alias() {
        let expr = call("len", vec![lit_str("hi")]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(2));
    }

    #[test]
    fn fn_trim() {
        let expr = call("trim", vec![lit_str("  hello  ")]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("hello".to_string())
        );
    }

    #[test]
    fn fn_ltrim() {
        let expr = call("ltrim", vec![lit_str("  hello  ")]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("hello  ".to_string())
        );
    }

    #[test]
    fn fn_rtrim() {
        let expr = call("rtrim", vec![lit_str("  hello  ")]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("  hello".to_string())
        );
    }

    #[test]
    fn fn_replace() {
        let expr = call(
            "replace",
            vec![lit_str("foo bar foo"), lit_str("foo"), lit_str("baz")],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("baz bar baz".to_string())
        );
    }

    #[test]
    fn fn_substr_two_args() {
        let expr = call("substr", vec![lit_str("hello world"), lit_int(7)]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("world".to_string())
        );
    }

    #[test]
    fn fn_substr_three_args() {
        let expr = call(
            "substr",
            vec![lit_str("hello world"), lit_int(1), lit_int(5)],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("hello".to_string())
        );
    }

    #[test]
    fn fn_substr_null() {
        let expr = call("substr", vec![lit_null(), lit_int(1)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    // ── scalar functions: numeric ──────────────────────────────────

    #[test]
    fn fn_abs_positive() {
        let expr = call("abs", vec![lit_int(-42)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(42));
    }

    #[test]
    fn fn_abs_float() {
        let expr = call("abs", vec![lit_float(-1.5)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(1.5));
    }

    #[test]
    fn fn_ceil() {
        let expr = call("ceil", vec![lit_float(1.2)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(2));
    }

    #[test]
    fn fn_ceiling_alias() {
        let expr = call("ceiling", vec![lit_float(1.2)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(2));
    }

    #[test]
    fn fn_floor() {
        let expr = call("floor", vec![lit_float(1.8)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(1));
    }

    #[test]
    fn fn_round_no_precision() {
        let expr = call("round", vec![lit_float(1.6)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(2));
    }

    #[test]
    fn fn_round_with_precision() {
        let expr = call("round", vec![lit_float(1.456), lit_int(2)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(1.46));
    }

    #[test]
    fn fn_round_int_passthrough() {
        let expr = call("round", vec![lit_int(42)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(42));
    }

    // ── scalar functions: conditional ──────────────────────────────

    #[test]
    fn fn_if_true() {
        let expr = call("if", vec![lit_bool(true), lit_str("yes"), lit_str("no")]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("yes".to_string())
        );
    }

    #[test]
    fn fn_if_false() {
        let expr = call("if", vec![lit_bool(false), lit_str("yes"), lit_str("no")]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("no".to_string())
        );
    }

    #[test]
    fn fn_coalesce_first_non_null() {
        let expr = call("coalesce", vec![lit_null(), lit_int(42), lit_int(99)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(42));
    }

    #[test]
    fn fn_coalesce_all_null() {
        let expr = call("coalesce", vec![lit_null(), lit_null()]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn fn_isnull_null() {
        let expr = call("isnull", vec![lit_null()]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn fn_isnull_not_null() {
        let expr = call("isnull", vec![lit_int(5)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn fn_isnotnull() {
        let expr = call("isnotnull", vec![lit_str("hi")]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn fn_typeof_int() {
        let expr = call("typeof", vec![lit_int(5)]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("INTEGER".to_string())
        );
    }

    #[test]
    fn fn_typeof_string() {
        let expr = call("typeof", vec![lit_str("hi")]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("VARCHAR".to_string())
        );
    }

    #[test]
    fn fn_typeof_null() {
        let expr = call("typeof", vec![lit_null()]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("NULL".to_string())
        );
    }

    #[test]
    fn fn_now_returns_string() {
        let expr = call("now", vec![]);
        let result = eval_expr(&expr, &empty_event());
        assert!(matches!(result, EvalValue::Str(_)));
    }

    // ── EvalValue conversions ──────────────────────────────────────

    #[test]
    fn eval_value_to_json_roundtrip() {
        let cases = vec![
            (EvalValue::Null, Value::Null),
            (EvalValue::Bool(true), Value::Bool(true)),
            (EvalValue::Int(42), json!(42)),
            (EvalValue::Float(1.5), json!(1.5)),
            (EvalValue::Str("hi".to_string()), json!("hi")),
        ];
        for (eval_val, expected_json) in cases {
            assert_eq!(Value::from(eval_val), expected_json);
        }
    }

    #[test]
    fn json_to_eval_value() {
        let cases = vec![
            (json!(null), EvalValue::Null),
            (json!(true), EvalValue::Bool(true)),
            (json!(42), EvalValue::Int(42)),
            (json!(1.5), EvalValue::Float(1.5)),
            (json!("hi"), EvalValue::Str("hi".to_string())),
        ];
        for (json_val, expected_eval) in cases {
            assert_eq!(EvalValue::from(&json_val), expected_eval);
        }
    }

    // ── truthiness ─────────────────────────────────────────────────

    #[test]
    fn truthiness() {
        assert!(!EvalValue::Null.is_truthy());
        assert!(!EvalValue::Bool(false).is_truthy());
        assert!(EvalValue::Bool(true).is_truthy());
        assert!(!EvalValue::Int(0).is_truthy());
        assert!(EvalValue::Int(1).is_truthy());
        assert!(!EvalValue::Str(String::new()).is_truthy());
        assert!(EvalValue::Str("x".to_string()).is_truthy());
    }

    // ── complex expressions (integration-style) ────────────────────

    #[test]
    fn complex_where_condition() {
        // status >= 400 and status < 500
        let ev = event(&json!({"status": 404}));
        let expr = binary(
            binary(field("status"), BinaryOp::Gte, lit_int(400)),
            BinaryOp::And,
            binary(field("status"), BinaryOp::Lt, lit_int(500)),
        );
        assert_eq!(eval_expr(&expr, &ev), EvalValue::Bool(true));
    }

    #[test]
    fn complex_let_expression() {
        // duration * 1000 + 50
        let ev = event(&json!({"duration": 2}));
        let expr = binary(
            binary(field("duration"), BinaryOp::Mul, lit_int(1000)),
            BinaryOp::Add,
            lit_int(50),
        );
        assert_eq!(eval_expr(&expr, &ev), EvalValue::Int(2050));
    }

    #[test]
    fn nested_function_call() {
        // upper(lower("HeLLo"))
        let expr = call("upper", vec![call("lower", vec![lit_str("HeLLo")])]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("HELLO".to_string())
        );
    }

    #[test]
    fn if_with_field_condition() {
        // if(isnotnull(message), length(message), 0)
        let ev = event(&json!({"message": "hello world"}));
        let expr = call(
            "if",
            vec![
                call("isnotnull", vec![field("message")]),
                call("length", vec![field("message")]),
                lit_int(0),
            ],
        );
        assert_eq!(eval_expr(&expr, &ev), EvalValue::Int(11));
    }

    #[test]
    fn if_with_null_field() {
        let ev = event(&json!({"host": "web-1"}));
        let expr = call(
            "if",
            vec![
                call("isnotnull", vec![field("message")]),
                call("length", vec![field("message")]),
                lit_int(0),
            ],
        );
        assert_eq!(eval_expr(&expr, &ev), EvalValue::Int(0));
    }
}
