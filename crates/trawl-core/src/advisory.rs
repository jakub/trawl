// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The retired-alias advisories (ADR-0013 §7): pure semantics plus a
//! shape-triggered notice.
//!
//! `level=error` means what it says — a verbatim comparison on the
//! sender's own field. But that is exactly the shape an analyst types
//! when they mean severity, and after the alias deletion it silently
//! answers a different question. The same is true of `timestamp` and
//! `@timestamp`, which used to resolve to the `_time` column and now name
//! whatever the sender happened to send. So when the AST shows one of
//! those retired spellings, the response carries a non-blocking advisory
//! pointing at the contract slot that replaced it.
//!
//! Silence here is expensive in a specific way: a name no sender writes is
//! not an error, it is an empty result. A pre-cutover saved query, report
//! or dashboard panel written against the old spellings would otherwise
//! come back with zero rows and zero columns and say nothing at all —
//! which is exactly what the deleted "`level` outside a comparison is an
//! error" class used to prevent.
//!
//! Three decisions make this honest rather than a nag:
//!
//! - it triggers on **query shape**, never on catalog state. A pin-existence
//!   check would go quiet on exactly the mixed fleet where one sender has
//!   a legitimate `level` pin and another means severity — the case the
//!   notice exists for.
//! - it triggers on **every position the name can appear in** — filters,
//!   `where`/`let`, group-by, sort, projections, `rename` sources — because
//!   the confusion is in the name, not in the stage. `_severity=error` and
//!   `_time` are already right and get nothing.
//! - a `level` COMPARISON can prove the analyst means their own field, and
//!   then it is quiet: `level=gold` is somebody's loot tier. Only a
//!   comparison carries a literal to read intent from, so that exemption
//!   stops there — a legitimate `| stats count() by level` pays one
//!   advisory line, which is the price of the empty pre-cutover query
//!   saying so.
//!
//! Never blocks. The query executes with pure semantics either way.

use crate::ast::{Expr, FilterValue, LiteralValue, PipeStage, Query, SearchToken, Spanned};
use crate::schema;

/// The bare field name whose uses are advised on.
const ADVISED_FIELD: &str = "level";

/// The one sentence the `level` advisory carries. Fixed text: it names the
/// shape, the reason, and the remedy, and it does not quote the user's
/// query (which the telemetry boundary keeps out of responses anyway).
pub const LEVEL_ADVISORY: &str = "'level' is an ordinary field now, not a severity alias: this reads the sender's own \
     value, and matches nothing where no sender writes it. For the severity ladder use \
     `_severity` (e.g. `_severity>=error`).";

/// The same sentence for the retired time aliases.
pub const TIME_ADVISORY: &str = "'timestamp'/'@timestamp' are ordinary fields now, not aliases for the event time: they read \
     the sender's own value, and match nothing where no sender writes it. For the canonical event \
     time use `_time`.";

/// Whether a value names a point on the severity ladder — the band token
/// table plus the `OTel` exact short names, the same vocabulary the
/// `SEVERITY` pin binds.
fn is_severity_token(value: &str) -> bool {
    crate::severity::number_for_token(value).is_some()
        || crate::severity::number_for_exact(value).is_some()
}

/// Whether `name` is a retired time alias.
///
/// Derived from the `_time` derivation's source list rather than restated
/// beside it, minus the contract slot itself: `_time` is the right
/// spelling, so naming it is never confusing. Ingest still READS these
/// (ADR-0013 §2) — it just stores them verbatim, under their own names.
fn is_time_alias(name: &str) -> bool {
    schema::TIME_ALIASES
        .iter()
        .any(|alias| !schema::is_reserved_name(alias) && alias.eq_ignore_ascii_case(name))
}

/// The non-blocking advisories `query`'s shape earns, in a fixed order.
///
/// Empty for the overwhelming majority of queries, which is what keeps a
/// healthy response byte-identical to one from a build without the field.
#[must_use]
pub fn notices(query: &Query) -> Vec<&'static str> {
    // One walk over every BINDING position, shared with the degraded-field
    // notice: which stages can name a field is a question with one answer.
    let bound = crate::field_refs::referenced_fields(query);
    let mut out = Vec::new();
    if bound.contains(ADVISED_FIELD) && !denies_severity_intent(query) {
        out.push(LEVEL_ADVISORY);
    }
    if bound.iter().any(|name| is_time_alias(name)) {
        out.push(TIME_ADVISORY);
    }
    out
}

/// Whether the query PROVES the analyst means their own `level` field: a
/// comparison against a literal the severity vocabulary does not recognize.
///
/// Anywhere in the query is enough. Somebody who wrote `level=gold` once
/// knows whose field it is, and re-advising them on the rest of the same
/// query is the nag this exemption exists to prevent.
fn denies_severity_intent(query: &Query) -> bool {
    query
        .search
        .groups
        .iter()
        .flatten()
        .any(search_token_denial)
        || query.pipeline.iter().any(|s| stage_denial(&s.node))
}

fn search_token_denial(token: &Spanned<SearchToken>) -> bool {
    match &token.node {
        SearchToken::FieldFilter(ff) if is_advised_name(&ff.field) => match &ff.value {
            FilterValue::Literal(v) => !is_severity_token(v),
            FilterValue::List(vs) => !vs.iter().any(|v| is_severity_token(v)),
        },
        SearchToken::Not(inner) => search_token_denial(inner),
        SearchToken::Group(groups) => groups.iter().flatten().any(search_token_denial),
        _ => false,
    }
}

fn stage_denial(stage: &PipeStage) -> bool {
    match stage {
        PipeStage::Where(w) => expr_denial(&w.condition),
        PipeStage::Let(l) => l.assignments.iter().any(|(_, e)| expr_denial(e)),
        _ => false,
    }
}

fn expr_denial(expr: &Spanned<Expr>) -> bool {
    match &expr.node {
        Expr::Binary { lhs, op, rhs } => {
            if is_comparison(*op) && comparison_denial(lhs, rhs) {
                return true;
            }
            expr_denial(lhs) || expr_denial(rhs)
        }
        Expr::Unary { operand, .. } => expr_denial(operand),
        Expr::FunctionCall { args, .. } => args.iter().any(expr_denial),
        Expr::InList { expr: target, list } => {
            if is_advised_ref(target) && !list.iter().any(is_token_literal) {
                return true;
            }
            expr_denial(target) || list.iter().any(expr_denial)
        }
        Expr::Literal(_) | Expr::FieldRef(_) => false,
    }
}

const fn is_comparison(op: crate::ast::BinaryOp) -> bool {
    use crate::ast::BinaryOp as B;
    matches!(op, B::Eq | B::Ne | B::Gt | B::Gte | B::Lt | B::Lte)
}

/// Either operand order: `level == "gold"` and `"gold" == level` prove the
/// same thing.
fn comparison_denial(lhs: &Spanned<Expr>, rhs: &Spanned<Expr>) -> bool {
    let pair = |field: &Spanned<Expr>, other: &Spanned<Expr>| {
        is_advised_ref(field) && matches!(&other.node, Expr::Literal(l) if !is_token(l))
    };
    pair(lhs, rhs) || pair(rhs, lhs)
}

/// `DuckDB` folds identifiers over ASCII, so `Level` IS `level`
/// ([`schema::catalog_key`]) — allocation-free here.
fn is_advised_name(name: &str) -> bool {
    name.eq_ignore_ascii_case(ADVISED_FIELD)
}

fn is_advised_ref(expr: &Spanned<Expr>) -> bool {
    matches!(&expr.node, Expr::FieldRef(name) if is_advised_name(name))
}

fn is_token(literal: &LiteralValue) -> bool {
    matches!(literal, LiteralValue::String(s) if is_severity_token(s))
}

fn is_token_literal(expr: &Spanned<Expr>) -> bool {
    matches!(&expr.node, Expr::Literal(l) if is_token(l))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advises(dsl: &str) -> Vec<&'static str> {
        notices(&crate::parser::parse(dsl).expect("parses"))
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
            assert_eq!(advises(dsl), vec![LEVEL_ADVISORY], "{dsl} must advise");
        }
    }

    /// The shapes with no literal to read intent from. Every one of these
    /// was a hard emit error before the cutover and is a silent empty
    /// result after it, so the name itself is the trigger.
    #[test]
    fn fires_on_a_level_reference_with_no_literal_to_judge() {
        for dsl in [
            "* | stats count() by level",
            "* | table level",
            "* | fields level, message",
            "* | sort -level",
            "* | dedup level",
            "* | top 5 level",
            "* | rename level as lvl",
            "* | timechart span=5m count() by level",
            r#"* | where lower(level) == "error""#,
            "* | where level == severity",
            "* | let l = level",
            "* | table Level",
        ] {
            assert_eq!(advises(dsl), vec![LEVEL_ADVISORY], "{dsl} must advise");
        }
    }

    /// The retired time aliases: `_time` is the column, these are the
    /// sender's own fields now.
    #[test]
    fn fires_on_the_retired_time_aliases_in_every_position() {
        for dsl in [
            r#"timestamp>"2026-01-01""#,
            "* | sort -timestamp | head 5",
            "* | table timestamp, message",
            "* | stats count() by timestamp",
            "* | where @timestamp > 1",
            "* | table @timestamp",
            "* | rename timestamp as ts",
            "* | table TimeStamp",
        ] {
            assert_eq!(advises(dsl), vec![TIME_ADVISORY], "{dsl} must advise");
        }
    }

    #[test]
    fn both_advisories_ride_the_same_response() {
        assert_eq!(
            advises("level=error | sort -timestamp"),
            vec![LEVEL_ADVISORY, TIME_ADVISORY]
        );
    }

    #[test]
    fn stays_quiet_on_everything_else() {
        for dsl in [
            // Somebody's loot tier, not a severity — and the proof holds
            // for the rest of the query.
            "level=gold",
            "level=3",
            r#"* | where level == "gold""#,
            r#"* | where level in ("gold", "silver")"#,
            "level=gold | stats count() by level",
            // Already right.
            "_severity=error",
            "_severity>=warn",
            r#"* | where _severity == "error""#,
            "* | sort -_time",
            "* | table _time, message",
            // The word appears, but binds no field.
            "error",
            "message=/error/",
            "last=1h",
            "* | drop level",
        ] {
            assert!(advises(dsl).is_empty(), "{dsl} must stay quiet");
        }
    }
}
