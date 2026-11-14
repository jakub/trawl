// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Advice on a valid query: a note that names the query its author
//! probably meant (ADR-0068).
//!
//! Advice never gates. It is not a [`crate::parser::ParseError`], and
//! nothing about it changes whether a query is valid, runs, or what it
//! returns. Every surface calls [`advise`] on the tree it parsed and draws
//! the findings as advice, never as errors.
//!
//! Two search-stage shapes read like field filters and are text searches:
//!
//! - [`AdviceKind::NegatedFieldShape`]: `-service=coredns`, a negated text
//!   search for the text `service=coredns`.
//! - [`AdviceKind::SearchStageIn`]: `event_type in (a, b)`, text searches
//!   for `event_type` and `in` beside a bracketed search group.
//!
//! The walk reads only the search stage and holds the nodes still to visit
//! in a list on the heap, as [`crate::parser`]'s depth check does, so a
//! deep tree costs heap, not stack. It never parses, formats, clones,
//! debug-prints or compares the tree: the derived `Clone`, `Debug` and
//! `PartialEq` recurse.

use std::ops::Range;

use crate::ast::{Query, SearchToken, Spanned, TextSearch, TimeClause};
use crate::parser::suggest::{backtick_dsl_field, is_bare_field_name, quote_dsl_field};

/// The most findings one call returns. Findings past it are counted in
/// [`AdviceReport::omitted`] and their copy is never built.
pub const MAX_ADVICE_FINDINGS: usize = 8;

/// Which shape a finding describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdviceKind {
    /// A negated text term shaped like a field filter: `-service=coredns`.
    NegatedFieldShape,
    /// A field-shaped word, then `in`, `not in` or `NOT in`, then a
    /// bracketed group, in one search-stage AND sequence.
    SearchStageIn,
}

/// One finding: the shape, where it is, and fixed copy that names the
/// field only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advice {
    pub kind: AdviceKind,
    /// A byte span into the text the query was parsed from.
    pub span: Range<usize>,
    pub message: String,
    pub hint: String,
}

/// The findings for one query, in source order, and how many were left
/// out past [`MAX_ADVICE_FINDINGS`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdviceReport {
    pub findings: Vec<Advice>,
    pub omitted: usize,
}

const MSG_SEARCH_STAGE_IN: &str = "in the search stage, in (…) is not a list test: the words \
     before the brackets are text searches, and the brackets hold a search group";

/// The comparison operators a field filter reads, longest first, so `<=`
/// wins over `<` at the same byte.
const OPERATORS: [&str; 6] = [">=", "<=", "!=", ">", "<", "="];

/// The advice for `query`, a tree a parse admitted, whose spans index the
/// text it was parsed from.
///
/// Findings come in source order. At most [`MAX_ADVICE_FINDINGS`] are
/// kept; the rest are only counted.
#[must_use]
pub fn advise(query: &Query) -> AdviceReport {
    let search = &query.search;
    let mut report = AdviceReport::default();
    // Pre-order over nested spans visits findings in source order: a run's
    // finding starts at its field word, before anything in the group the
    // run ends with.
    let mut work: Vec<Work<'_>> = search
        .groups
        .iter()
        .rev()
        .map(|tokens| Work::Seq(tokens, 0))
        .collect();
    while let Some(item) = work.pop() {
        match item {
            Work::Seq(tokens, index) => {
                if index >= tokens.len() {
                    continue;
                }
                // A matched run consumes its keyword prefix, so `F not in (`
                // is not read again as `not in (`, but its group is still
                // visited.
                let next = match in_run(tokens, index, &search.hoisted) {
                    Some(run) => {
                        report.push(run.finding());
                        run.group
                    }
                    None => index,
                };
                work.push(Work::Seq(tokens, next + 1));
                work.push(Work::Token(&tokens[next]));
            }
            Work::Token(token) => match &token.node {
                SearchToken::TextSearch(TextSearch {
                    term,
                    negated: true,
                }) => {
                    if let Some((name, op)) = field_shape(term) {
                        report.push(Pending::Negated {
                            span: token.span.clone(),
                            name,
                            op,
                        });
                    }
                }
                SearchToken::Not(inner) => work.push(Work::Token(inner)),
                SearchToken::Group(branches) => {
                    work.extend(branches.iter().rev().map(|tokens| Work::Seq(tokens, 0)));
                }
                SearchToken::TextSearch(_)
                | SearchToken::FieldFilter(_)
                | SearchToken::TimeFilter(_)
                | SearchToken::QuotedSearch(_)
                | SearchToken::EarliestFilter(_)
                | SearchToken::LatestFilter(_) => {}
            },
        }
    }
    report
}

/// A node still to visit: one AND sequence from an index on, or one token.
enum Work<'a> {
    Seq(&'a [Spanned<SearchToken>], usize),
    Token(&'a Spanned<SearchToken>),
}

/// A finding before its copy is built.
enum Pending<'a> {
    Negated {
        span: Range<usize>,
        name: &'a str,
        op: &'static str,
    },
    In {
        span: Range<usize>,
        name: &'a str,
        negated: bool,
    },
}

impl AdviceReport {
    fn push(&mut self, pending: Pending<'_>) {
        if self.findings.len() < MAX_ADVICE_FINDINGS {
            self.findings.push(pending.build());
        } else {
            self.omitted += 1;
        }
    }
}

impl Pending<'_> {
    fn build(self) -> Advice {
        match self {
            Self::Negated { span, name, op } => Advice {
                kind: AdviceKind::NegatedFieldShape,
                span,
                message: format!(
                    "-{name}{op}… is a text search, not a field filter: it drops events whose \
                     message or _raw contain that text"
                ),
                hint: negated_hint(name, op),
            },
            Self::In {
                span,
                name,
                negated,
            } => {
                let field = filter_field(name);
                // The expression grammar has no `not in` operator: `not` is a
                // prefix over the whole comparison.
                let hint = if negated {
                    format!(
                        "to exclude a list, write {field}!=a,b; in (…) is expression syntax, as \
                         in | where not {field} in (\"a\", \"b\")"
                    )
                } else {
                    format!(
                        "to match a list, write {field}=a,b; in (…) is expression syntax, as in \
                         | where {field} in (\"a\", \"b\")"
                    )
                };
                Advice {
                    kind: AdviceKind::SearchStageIn,
                    span,
                    message: MSG_SEARCH_STAGE_IN.to_owned(),
                    hint,
                }
            }
        }
    }
}

/// The hint for `-name<op>…`. The quoted silencing form keeps the name as
/// written: there it is text, not a field reference.
fn negated_hint(name: &str, op: &str) -> String {
    let under_not = not_field(name);
    let silence = format!("to keep the text search and silence this, write NOT \"{name}{op}…\"");
    if op == "=" {
        let field = filter_field(name);
        format!(
            "to filter the field, write {field}!=… (keeps events with no {name}) or NOT \
             {under_not}=… (drops them too); {silence}"
        )
    } else {
        format!("to filter the field, write NOT {under_not}{op}…; {silence}")
    }
}

/// `name` spelled as a field filter's field.
fn filter_field(name: &str) -> String {
    // A bare field name is ASCII and never empty, so the renderer always
    // spells it.
    quote_dsl_field(name).unwrap_or_else(|| name.to_owned())
}

/// `name` spelled as the field of a filter directly under `NOT`, which
/// negates the next token only when it opens with an identifier
/// character, a quote, `-`, `(` or a backtick: a bare `@`-name there
/// would leave `NOT` a text search (see [`backtick_dsl_field`]).
fn not_field(name: &str) -> String {
    let field = filter_field(name);
    if field.starts_with('@') {
        backtick_dsl_field(name).unwrap_or(field)
    } else {
        field
    }
}

/// The field name and operator of a term shaped `<field><op><value>`:
/// split at the first byte of `<>!=`, where the longest operator must
/// match, with a bare field name before it and at least one byte after.
fn field_shape(term: &str) -> Option<(&str, &'static str)> {
    let at = term.find(['<', '>', '!', '='])?;
    let (name, rest) = term.split_at(at);
    let op = OPERATORS.into_iter().find(|op| rest.starts_with(op))?;
    (rest.len() > op.len() && is_bare_field_name(name)).then_some((name, op))
}

/// A matched `F in (`, `F not in (` or `F NOT in (` run.
struct InRun<'a> {
    field: &'a Spanned<SearchToken>,
    name: &'a str,
    negated: bool,
    /// The index of the run's group in its sequence.
    group: usize,
    group_start: usize,
}

impl<'a> InRun<'a> {
    fn finding(&self) -> Pending<'a> {
        Pending::In {
            span: self.field.span.start..self.group_start + 1,
            name: self.name,
            negated: self.negated,
        }
    }
}

/// The run that starts at `tokens[index]`, if one does.
///
/// A time clause hoisted out from between the field word and the group
/// cancels the match: the tokens look adjacent only because the clause
/// is gone.
fn in_run<'a>(
    tokens: &'a [Spanned<SearchToken>],
    index: usize,
    hoisted: &[Spanned<TimeClause>],
) -> Option<InRun<'a>> {
    let field = &tokens[index];
    let name = positive_term(&field.node).filter(|name| is_bare_field_name(name))?;
    let next = tokens.get(index + 1)?;
    let (negated, group) = if positive_term(&next.node).is_some_and(is_in) {
        (false, index + 2)
    } else if positive_term(&next.node).is_some_and(|word| word.eq_ignore_ascii_case("not"))
        && tokens
            .get(index + 2)
            .and_then(|token| positive_term(&token.node))
            .is_some_and(is_in)
    {
        (true, index + 3)
    } else if let SearchToken::Not(inner) = &next.node
        && positive_term(&inner.node).is_some_and(is_in)
    {
        (true, index + 2)
    } else {
        return None;
    };
    let group_token = tokens.get(group)?;
    if !matches!(group_token.node, SearchToken::Group(_)) {
        return None;
    }
    let group_start = group_token.span.start;
    let first = hoisted.partition_point(|clause| clause.span.start < field.span.start);
    if hoisted
        .get(first)
        .is_some_and(|clause| clause.span.start < group_start)
    {
        return None;
    }
    Some(InRun {
        field,
        name,
        negated,
        group,
        group_start,
    })
}

/// The term of a positive text search.
fn positive_term(token: &SearchToken) -> Option<&str> {
    match token {
        SearchToken::TextSearch(TextSearch {
            term,
            negated: false,
        }) => Some(term),
        _ => None,
    }
}

fn is_in(word: &str) -> bool {
    word == "in" || word == "IN"
}
