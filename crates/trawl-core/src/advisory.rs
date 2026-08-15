// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `level=error` advisory (ADR-0013 §7): pure semantics plus a
//! shape-triggered notice.
//!
//! `level=error` means what it says — a verbatim comparison on the
//! sender's own field. But that is exactly the shape an analyst types
//! when they mean severity, and after the alias deletion it silently
//! answers a different question. So when the AST shows a bare `level`
//! compared against a value the severity vocabulary RECOGNIZES, the
//! response carries a non-blocking advisory pointing at `_severity`.
//!
//! Two decisions make this honest rather than a nag:
//!
//! - it triggers on **query shape**, never on catalog state. A pin-existence
//!   check would go quiet on exactly the mixed fleet where one sender has
//!   a legitimate `level` pin and another means severity — the case the
//!   notice exists for.
//! - it triggers on a **recognized token only**. `level=gold` is somebody's
//!   loot tier and gets nothing; `_severity=error` is already right and
//!   gets nothing.
//!
//! Never blocks. The query executes with pure semantics either way.

use crate::ast::{Expr, FilterValue, LiteralValue, PipeStage, Query, SearchToken, Spanned};

/// The bare field name whose severity-shaped comparisons are advised on.
const ADVISED_FIELD: &str = "level";

/// The one sentence the advisory carries. Fixed text: it names the shape,
/// the reason, and the remedy, and it does not quote the user's query
/// (which the telemetry boundary keeps out of responses anyway).
pub const LEVEL_ADVISORY: &str = "'level' is an ordinary field: this filtered the sender's own value, not \
     severity. For the severity ladder use `_severity` (e.g. `_severity>=error`).";

/// Whether a value names a point on the severity ladder — the band token
/// table plus the `OTel` exact short names, the same vocabulary the
/// `SEVERITY` pin binds.
fn is_severity_token(value: &str) -> bool {
    crate::severity::number_for_token(value).is_some()
        || crate::severity::number_for_exact(value).is_some()
}

/// Whether `query` contains a bare `level` compared against a recognized
/// severity token, in either stage.
///
/// Search stage: a field filter on `level` whose literal (or any IN-list
/// element) is a token. Pipeline: a `where`/`let` binary comparison with
/// `level` on either side and a string literal on the other.
#[must_use]
pub fn advises_severity(query: &Query) -> bool {
    let search_hit = query.search.groups.iter().flatten().any(search_token_hit);
    search_hit || query.pipeline.iter().any(|s| stage_hit(&s.node))
}

fn search_token_hit(token: &Spanned<SearchToken>) -> bool {
    match &token.node {
        SearchToken::FieldFilter(ff) if ff.field == ADVISED_FIELD => match &ff.value {
            FilterValue::Literal(v) => is_severity_token(v),
            FilterValue::List(vs) => vs.iter().any(|v| is_severity_token(v)),
        },
        SearchToken::Not(inner) => search_token_hit(inner),
        SearchToken::Group(groups) => groups.iter().flatten().any(search_token_hit),
        _ => false,
    }
}

fn stage_hit(stage: &PipeStage) -> bool {
    match stage {
        PipeStage::Where(w) => expr_hit(&w.condition),
        PipeStage::Let(l) => l.assignments.iter().any(|(_, e)| expr_hit(e)),
        _ => false,
    }
}

fn expr_hit(expr: &Spanned<Expr>) -> bool {
    match &expr.node {
        Expr::Binary { lhs, op, rhs } => {
            if is_comparison(*op) && comparison_hit(lhs, rhs) {
                return true;
            }
            expr_hit(lhs) || expr_hit(rhs)
        }
        Expr::Unary { operand, .. } => expr_hit(operand),
        Expr::FunctionCall { args, .. } => args.iter().any(expr_hit),
        Expr::InList { expr: target, list } => {
            if matches!(&target.node, Expr::FieldRef(name) if name == ADVISED_FIELD)
                && list.iter().any(is_token_literal)
            {
                return true;
            }
            expr_hit(target) || list.iter().any(expr_hit)
        }
        Expr::Literal(_) | Expr::FieldRef(_) => false,
    }
}

const fn is_comparison(op: crate::ast::BinaryOp) -> bool {
    use crate::ast::BinaryOp as B;
    matches!(op, B::Eq | B::Ne | B::Gt | B::Gte | B::Lt | B::Lte)
}

/// Either operand order: `level == "error"` and `"error" == level` are
/// the same confusion.
fn comparison_hit(lhs: &Spanned<Expr>, rhs: &Spanned<Expr>) -> bool {
    let pair = |field: &Spanned<Expr>, other: &Spanned<Expr>| {
        matches!(&field.node, Expr::FieldRef(name) if name == ADVISED_FIELD)
            && is_token_literal(other)
    };
    pair(lhs, rhs) || pair(rhs, lhs)
}

fn is_token_literal(expr: &Spanned<Expr>) -> bool {
    matches!(&expr.node, Expr::Literal(LiteralValue::String(s)) if is_severity_token(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advises(dsl: &str) -> bool {
        advises_severity(&crate::parser::parse(dsl).expect("parses"))
    }

    #[test]
    fn fires_on_a_bare_level_against_a_recognized_token() {
        for dsl in [
            "level=error",
            "level=ERROR",
            "level!=warn",
            "level>=warn",
            "level=info,error",
            "level=error2",
            "service=nginx level=error last=1h",
            "NOT level=error",
            r#"* | where level == "warn""#,
            r#"* | where "warn" == level"#,
            r#"* | where service == "nginx" and level != "error""#,
            r#"* | where level in ("error", "fatal")"#,
            r#"* | let hot = level == "error""#,
        ] {
            assert!(advises(dsl), "{dsl} must advise");
        }
    }

    #[test]
    fn stays_quiet_on_everything_else() {
        for dsl in [
            // Somebody's loot tier, not a severity.
            "level=gold",
            "level=3",
            r#"* | where level == "gold""#,
            // Already right.
            "_severity=error",
            "_severity>=warn",
            r#"* | where _severity == "error""#,
            // The word appears, but not as a `level` comparison.
            "error",
            "message=/error/",
            "* | stats count() by level",
            "* | table level",
            r#"* | where lower(level) == "error""#,
            "* | rename level as lvl",
        ] {
            assert!(!advises(dsl), "{dsl} must stay quiet");
        }
    }
}
