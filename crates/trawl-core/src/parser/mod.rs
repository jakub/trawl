// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! DSL parser for trawl's query language.
//!
//! Transforms a query string like `service=nginx _severity=error last=2h | stats count() by host`
//! into a structured AST representation.
//!
//! The parser is built in layers:
//! 1. **Primitives** — numbers, strings, identifiers, operators, durations
//! 2. **Search tokens** — field filters, text search, time filters, quoted phrases
//! 3. **Search stage** — whitespace-separated tokens (implicit AND)
//! 4. **Expressions** — recursive expression parser with operator precedence
//! 5. **Pipe stages** — stats, where, sort, limit, table

pub(crate) mod comment;
pub(crate) mod expr;
pub(crate) mod pipe;
pub(crate) mod primitives;
pub mod scan;
pub(crate) mod search;
pub mod suggest;

use chumsky::prelude::*;

use crate::ast::Query;
use primitives::{ParserExtra, ParserInput};
pub use primitives::{RegexCut, first_cut};

/// An error produced by the parser, with source location and context.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub span: std::ops::Range<usize>,
    pub label: Option<String>,
    /// Optional contextual suggestion (e.g. "did you mean 'stats'?").
    pub hint: Option<String>,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}..{}] {}",
            self.span.start, self.span.end, self.message
        )?;
        if let Some(label) = &self.label {
            write!(f, " (while parsing {label})")?;
        }
        if let Some(hint) = &self.hint {
            write!(f, " ({hint})")?;
        }
        Ok(())
    }
}

/// Maximum query length in bytes. Prevents denial-of-service via pathological parser input.
///
/// `GET /api/v1/capabilities` advertises it as `max_query_bytes`.
pub const MAX_QUERY_LEN: usize = 65_536;

/// The most globs and regexes one query may hold, summed over every filter
/// and list element, every `matches` with a literal pattern and every
/// `extract` regex. Exact values do not count.
///
/// `MAX_QUERY_LEN` does not bound this: `f=/a/,/b/,…` spends four bytes a
/// regex, so 64 KiB of list compiles thousands of patterns in each lane.
/// The parser compiles a query's regexes only once the query is under it.
pub const MAX_QUERY_PATTERNS: usize = 256;

/// The most diagnostics one parse reports, however many the grammar
/// emitted.
///
/// A single query can carry unboundedly many independent violations
/// (`f=#a,#b,#c,…` emits one per element) and every one of them renders a
/// hint quoting text from the input, so the reported bytes grow as the
/// product of the two. The cap is applied before rendering, so the
/// quadratic work is never done rather than merely never returned; the
/// server serializes every detail it is handed, so this is the bound the
/// wire sees too.
///
/// Eight, not one: a parse error list is a work list, and chumsky orders
/// it by position, so the first few are the ones a user reads.
///
/// The cap is not a plain truncation, because the list is not purely
/// positional: the emitted diagnostics come first, and the failure that
/// ended the parse is appended last. Plain truncation drops that one, and
/// it is the error the query cannot succeed without fixing (an
/// eight-element `f=#a,…` list would hide `unknown command 'bogus'`). So
/// the report is the first `MAX_REPORTED_ERRORS - 1` plus that final one,
/// whose message is annotated with how many were left out
/// ([`omitted_suffix`]).
const MAX_REPORTED_ERRORS: usize = 8;

/// How the annotation on the last reported error reads, so the count the
/// cap swallowed is never silent. `{n}` diagnostics between the reported
/// prefix and the terminal error were not rendered.
fn omitted_suffix(n: usize) -> String {
    let plural = if n == 1 { "error" } else { "errors" };
    format!(" ({n} earlier {plural} omitted)")
}

/// Parse a trawl DSL query string into a structured AST.
///
/// # Errors
///
/// Returns a list of parse errors if the input is not valid trawl DSL.
pub fn parse(input: &str) -> Result<Query, Vec<ParseError>> {
    if input.len() > MAX_QUERY_LEN {
        return Err(vec![ParseError {
            message: format!(
                "query too long ({} bytes, max {MAX_QUERY_LEN})",
                input.len()
            ),
            span: 0..input.len(),
            label: None,
            hint: None,
        }]);
    }

    // Comments are a grammar production (ADR-0014), so the raw input goes
    // straight to the parser and error spans index the real text.
    let parser = query_parser();
    let mut regexes = primitives::RegexSites::default();
    let result = parser.parse_with_state(input, &mut regexes);

    // The grammar checks regex syntax and compiles nothing. Compiling
    // waits for a clean parse under the pattern cap, so neither a long
    // list nor a parse that already failed spends any compile work.
    let query = result.into_result().map_err(|errors| {
        let mut reported = report_errors(&errors, input);
        name_pipe_cut_runs(&mut reported, &regexes.pipe_cut_runs);
        reported
    })?;
    if let Some(error) = pattern_cap_error(&query) {
        return Err(vec![error]);
    }
    let errors = regex_compile_errors(&regexes.sites);
    if errors.is_empty() {
        Ok(query)
    } else {
        Err(errors)
    }
}

/// Name each `/…/` run a pipe stage cut out of a search value, beside the
/// first reported error that falls inside it (ADR-0061 ruling 1).
///
/// The run was a regex under the earlier rule, so `msg=/err | warn/`
/// fails as an unknown command `warn` whose real cause is the `|` the
/// author meant as alternation. The diagnostic goes beside that error,
/// never in place of it: an unrelated mistake can sit inside the run too
/// (`path=/tmp* | wher x / 2`), and its own error is the one to fix. A run
/// no error falls inside belongs to a query that failed for some other
/// reason, so it adds nothing.
fn name_pipe_cut_runs(errors: &mut Vec<ParseError>, runs: &[std::ops::Range<usize>]) {
    let mut named: Vec<&std::ops::Range<usize>> = Vec::new();
    for run in runs {
        if named.contains(&run) {
            continue;
        }
        let Some(at) = errors.iter().position(|e| run.contains(&e.span.start)) else {
            continue;
        };
        named.push(run);
        let message = primitives::MSG_REGEX_PIPE_STAGE;
        errors.insert(
            at,
            ParseError {
                message: message.to_string(),
                span: run.clone(),
                label: None,
                hint: comment::hint_for(message, None),
            },
        );
    }
}

/// Compile the regexes a clean parse recognised, in text order, and
/// report those that do not compile, capped as [`report_errors`] caps a
/// parse's list. It runs after [`pattern_cap_error`], so it compiles at
/// most [`MAX_QUERY_PATTERNS`] regexes, each bounded by
/// [`primitives::MAX_REGEX_COMPILED_BYTES`].
fn regex_compile_errors(sites: &[primitives::RegexSite]) -> Vec<ParseError> {
    let mut errors: Vec<ParseError> = sites
        .iter()
        .filter_map(|site| {
            let message = primitives::regex_compile_error(&site.body)?;
            Some(ParseError {
                hint: comment::hint_for(&message, None),
                message,
                span: site.span.clone(),
                label: None,
            })
        })
        .collect();
    if errors.len() > MAX_REPORTED_ERRORS {
        let omitted = errors.len() - MAX_REPORTED_ERRORS;
        let mut last = errors.pop().expect("the list is longer than the cap");
        errors.truncate(MAX_REPORTED_ERRORS - 1);
        last.message.push_str(&omitted_suffix(omitted));
        errors.push(last);
    }
    errors
}

/// The [`MAX_QUERY_PATTERNS`] check, over a query that already parsed.
///
/// It reads the AST, so no grammar alternative can fail and fall through
/// to another reading. The span is the token, `matches` pattern or
/// `extract` stage that holds the first pattern past the cap, walked in
/// text order.
fn pattern_cap_error(query: &Query) -> Option<ParseError> {
    use crate::ast::{ExtractMode, PipeStage};

    let mut tally = PatternTally::default();
    for group in &query.search.groups {
        for token in group {
            tally.walk(Visit::Token(token));
        }
    }
    for stage in &query.pipeline {
        match &stage.node {
            PipeStage::Where(w) => tally.walk(Visit::Expr(&w.condition)),
            PipeStage::Let(l) => {
                for (_, e) in &l.assignments {
                    tally.walk(Visit::Expr(e));
                }
            }
            PipeStage::Stats(s) => tally.aggs(&s.aggregations),
            PipeStage::Timechart(t) => tally.aggs(&t.aggregations),
            PipeStage::EventStats(s) => tally.aggs(&s.aggregations),
            PipeStage::Pivot(p) => tally.aggs(std::slice::from_ref(&p.aggregation)),
            PipeStage::Extract(x) if matches!(x.mode, ExtractMode::Regex(_)) => {
                tally.add(1, &stage.span);
            }
            _ => {}
        }
    }

    let span = tally.first_over?;
    Some(ParseError {
        message: format!(
            "too many patterns ({} globs and regexes, max {MAX_QUERY_PATTERNS})",
            tally.count
        ),
        span,
        label: None,
        hint: None,
    })
}

/// One node the [`PatternTally`] has still to visit.
enum Visit<'q> {
    Token(&'q crate::ast::Spanned<crate::ast::SearchToken>),
    Expr(&'q crate::ast::Spanned<crate::ast::Expr>),
    /// A literal `matches` pattern, counted when the walk reaches it.
    Pattern(&'q std::ops::Range<usize>),
}

/// The pattern count, walked with an explicit stack rather than by
/// recursion. `MAX_EXPR_DEPTH` bounds nesting, not tree depth: `1+1+…`
/// folds to a left-leaning tree as deep as the chain is long, and a
/// recursive walk of a 64 KiB chain overflows a tokio worker's stack.
#[derive(Default)]
struct PatternTally<'q> {
    count: usize,
    first_over: Option<std::ops::Range<usize>>,
    /// Nodes still to visit, the next one last. Children are pushed last
    /// first, so they come off in text order.
    pending: Vec<Visit<'q>>,
}

impl<'q> PatternTally<'q> {
    fn add(&mut self, n: usize, span: &std::ops::Range<usize>) {
        self.count = self.count.saturating_add(n);
        if self.count > MAX_QUERY_PATTERNS && self.first_over.is_none() {
            self.first_over = Some(span.clone());
        }
    }

    /// Count `root` and everything under it, in text order.
    fn walk(&mut self, root: Visit<'q>) {
        self.pending.push(root);
        while let Some(visit) = self.pending.pop() {
            match visit {
                Visit::Token(token) => self.token(token),
                Visit::Expr(expr) => self.expr(expr),
                Visit::Pattern(span) => self.add(1, span),
            }
        }
    }

    fn token(&mut self, token: &'q crate::ast::Spanned<crate::ast::SearchToken>) {
        use crate::ast::{FilterValue, Scalar, SearchToken};

        let is_pattern = |s: &Scalar| !matches!(s, Scalar::Exact(_));
        match &token.node {
            SearchToken::FieldFilter(f) => {
                let n = match &f.value {
                    FilterValue::Single(s) => usize::from(is_pattern(s)),
                    FilterValue::List(items) => items.iter().filter(|s| is_pattern(s)).count(),
                };
                if n > 0 {
                    self.add(n, &token.span);
                }
            }
            SearchToken::Not(inner) => self.pending.push(Visit::Token(inner)),
            SearchToken::Group(groups) => self
                .pending
                .extend(groups.iter().flatten().rev().map(Visit::Token)),
            _ => {}
        }
    }

    fn aggs(&mut self, aggs: &'q [crate::ast::AggExpr]) {
        for arg in aggs.iter().flat_map(|a| &a.args) {
            self.walk(Visit::Expr(arg));
        }
    }

    fn expr(&mut self, expr: &'q crate::ast::Spanned<crate::ast::Expr>) {
        use crate::ast::{BinaryOp, Expr, LiteralValue};

        match &expr.node {
            Expr::Binary { lhs, op, rhs } => {
                // A literal `matches` pattern is a regex the query
                // compiles, whether written `/re/` or `"re"`.
                if *op == BinaryOp::Matches
                    && matches!(rhs.node, Expr::Literal(LiteralValue::String(_)))
                {
                    self.pending.push(Visit::Pattern(&rhs.span));
                } else {
                    self.pending.push(Visit::Expr(rhs));
                }
                self.pending.push(Visit::Expr(lhs));
            }
            Expr::Unary { operand, .. } => self.pending.push(Visit::Expr(operand)),
            Expr::FunctionCall { args, .. } => {
                self.pending.extend(args.iter().rev().map(Visit::Expr));
            }
            Expr::InList { expr, list } => {
                self.pending.extend(list.iter().rev().map(Visit::Expr));
                self.pending.push(Visit::Expr(expr));
            }
            Expr::FieldRef(_) | Expr::Literal(_) => {}
        }
    }
}

/// Render at most [`MAX_REPORTED_ERRORS`] diagnostics from one parse,
/// keeping the terminal failure whatever the emitted list does to the
/// budget. Rendering is done only for the errors reported, so a
/// pathological query costs a constant amount of work, not a product.
fn report_errors(errors: &[Rich<'_, char>], input: &str) -> Vec<ParseError> {
    if errors.len() <= MAX_REPORTED_ERRORS {
        return errors
            .iter()
            .map(|e| rich_to_parse_error(e, input))
            .collect();
    }

    let omitted = errors.len() - MAX_REPORTED_ERRORS;
    let mut reported: Vec<ParseError> = errors
        .iter()
        .take(MAX_REPORTED_ERRORS - 1)
        .map(|e| rich_to_parse_error(e, input))
        .collect();
    let mut last = rich_to_parse_error(
        errors.last().expect("the list is longer than the cap"),
        input,
    );
    last.message.push_str(&omitted_suffix(omitted));
    reported.push(last);
    reported
}

/// Convert a chumsky `Rich` error into our `ParseError` with a human-friendly message.
fn rich_to_parse_error(e: &Rich<'_, char>, input: &str) -> ParseError {
    use chumsky::error::RichReason;

    let span = e.span();
    let label = e.contexts().next().map(|(l, _)| l.to_string());

    // custom errors (e.g. overflow, invalid regex) carry their own message
    if let RichReason::Custom(raw) = e.reason() {
        // A comment diagnostic carries the exact token its production
        // held, past the message it renders (`comment::payload`); every
        // other custom error is its message and nothing else.
        let (msg, exact) = comment::split_payload(raw);
        return ParseError {
            message: msg.to_string(),
            hint: comment::hint_for(msg, exact),
            span: span.start..span.end,
            label,
        };
    }

    let offset = span.start;

    // A comment opener the grammar could not admit (ADR-0014 rulings 2
    // and 3). This runs before the generic enrichment because it owns its
    // own span: chumsky reports where the production died, which for
    // `| co#unt()` is the start of the stage word and for `// note` is one
    // slash of two — neither of which is the byte the user has to change.
    if let Some(d) = comment_diagnostic(input, offset) {
        return ParseError {
            message: d.message,
            span: d.span,
            label,
            hint: d.hint,
        };
    }

    let found = if offset >= input.len() {
        "end of input".to_string()
    } else {
        let ch = &input[offset..];
        let end = ch.char_indices().nth(1).map_or(ch.len(), |(idx, _)| idx);
        format!("'{}'", &ch[..end])
    };

    let expected: Vec<String> = e
        .expected()
        .map(|exp| match exp {
            chumsky::error::RichPattern::Token(c) => format!("'{}'", **c),
            chumsky::error::RichPattern::Label(l) => l.to_string(),
            chumsky::error::RichPattern::Identifier(id) => format!("`{id}`"),
            chumsky::error::RichPattern::Any => "any token".to_string(),
            chumsky::error::RichPattern::SomethingElse => "something else".to_string(),
            chumsky::error::RichPattern::EndOfInput => "end of input".to_string(),
            _ => "unknown".to_string(),
        })
        .collect();

    let (message, hint) = enrich_error(input, offset, &found, &expected, label.as_deref());

    ParseError {
        message,
        span: span.start..span.end,
        label,
        hint,
    }
}

/// Find the pipe-command word around the error offset, if the error is in
/// a position that looks like a pipe stage command (i.e. the word follows a `|`).
///
/// Returns `Some(word)` if the word is not a known pipe stage (i.e. it's a typo),
/// or `None` if the error isn't in a pipe-command position or the word is valid.
fn find_pipe_command_word(input: &str, offset: usize) -> Option<String> {
    // Guard against byte offsets that land inside a multi-byte character.
    if !input.is_char_boundary(offset) {
        return None;
    }

    // Find the start of the word containing `offset` by scanning backward.
    let before = &input[..offset];
    let word_start = before
        .rfind(|c: char| !c.is_alphanumeric() && c != '_')
        .map_or(0, |pos| {
            // Advance past the matched character (which may be multi-byte).
            let c = input[pos..].chars().next().unwrap_or(' ');
            pos + c.len_utf8()
        });

    let candidate: String = input[word_start..]
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();

    if candidate.is_empty() {
        return None;
    }

    let prefix = input[..word_start].trim_end();
    if !prefix.ends_with('|') {
        return None;
    }

    if suggest::KNOWN_PIPE_STAGES.contains(&candidate.as_str()) {
        return None;
    }

    Some(candidate)
}

/// Whether `offset` sits at the start of the input or directly after an
/// ASCII whitespace character — the two positions the grammar admits a
/// comment at, asked of the one predicate that owns the question
/// ([`comment::opens_comment_after`], which [`comment::ws`] and
/// [`scan`] also go through).
fn preceded_by_whitespace(input: &str, offset: usize) -> bool {
    comment::opens_comment_after(input[..offset].chars().next_back())
}

/// A comment diagnostic, with the span of the byte the user must change.
struct CommentDiagnostic {
    message: String,
    hint: Option<String>,
    span: std::ops::Range<usize>,
}

impl CommentDiagnostic {
    /// No `exact`: this is the position-blind net, and a hint it minted a
    /// spelling for would be guessing at both the token's bounds and the
    /// position's escape (`comment::hint_for`).
    fn new(msg: &str, at: usize, len: usize) -> Self {
        Self {
            message: msg.to_string(),
            hint: comment::hint_for(msg, None),
            span: at..at + len,
        }
    }
}

/// The generic net for a comment opener the grammar could not admit, at
/// every position the two validators (the bare value and the search-stage
/// bare word) do not cover — a stage name, an expression, a sort key. It
/// exists so a `#` never surfaces as "found '#', expected …".
fn comment_diagnostic(input: &str, offset: usize) -> Option<CommentDiagnostic> {
    if offset >= input.len() {
        return None;
    }
    let rest = &input[offset..];

    // A `#` right where the parser died, and not at a boundary the
    // grammar would have admitted a comment at (ADR-0014 ruling 2).
    if rest.starts_with(comment::OPENER) && !preceded_by_whitespace(input, offset) {
        return Some(CommentDiagnostic::new(
            comment::MSG_OPENER_IN_TOKEN,
            offset,
            comment::OPENER.len_utf8(),
        ));
    }

    // …and the same net for `//`, which is not a comment opener, at the
    // one place it could have been one: a token boundary (ADR-0014
    // ruling 3). The span covers both slashes, since underlining one of
    // the two points at nothing the user can act on.
    if comment::starts_with_slashes(rest) && preceded_by_whitespace(input, offset) {
        return Some(CommentDiagnostic::new(
            comment::MSG_SLASHES_NOT_A_COMMENT,
            offset,
            comment::SLASHES.len(),
        ));
    }

    // A `#` inside a pipe stage name. chumsky reports the start of the
    // word (the stage word ends at the `#`), which the unknown-command
    // rule below then renders as `unknown command 'co'` plus a suggestion
    // for a typo the user did not make.
    let at = comment::opener_in_command_word(input, offset)?;
    Some(CommentDiagnostic::new(
        comment::MSG_OPENER_IN_COMMAND,
        at,
        comment::OPENER.len_utf8(),
    ))
}

/// Produce an enriched (message, hint) pair based on error context.
fn enrich_error(
    input: &str,
    offset: usize,
    found: &str,
    expected: &[String],
    label: Option<&str>,
) -> (String, Option<String>) {
    let expects_end_quote = expected.iter().any(|e| e == "'\"'");
    let expects_close_paren = expected.iter().any(|e| e == "')'");
    let at_end = offset >= input.len();

    // Unknown pipe stage: detected by scanning the input for a `|` before
    // the error position and extracting the word that follows it. chumsky's
    // choice() combinator may report character-level errors from the branch
    // that consumed the most input (e.g. "staats" partially matches "stats"),
    // resulting in no context labels — just a single-char expected token.
    // We recover the full word by scanning backward from the error offset.
    if let Some(ref word) = find_pipe_command_word(input, offset) {
        let hint = suggest::suggest_pipe_stage(word).map(|s| format!("did you mean '{s}'?"));
        return (format!("unknown command '{word}'"), hint);
    }

    if at_end && expects_end_quote {
        return (
            "unterminated string literal".to_string(),
            Some("add a closing '\"' to complete the string".to_string()),
        );
    }

    if at_end && expects_close_paren {
        return (
            "unmatched opening parenthesis".to_string(),
            Some("add a closing ')' to match the opening '('".to_string()),
        );
    }

    if label == Some("stats stage") && expected.iter().any(|e| e == "aggregation expression") {
        return (
            format!("found {found}, expected aggregation expression"),
            Some("stats requires at least one aggregation, e.g. stats count()".to_string()),
        );
    }

    let message = if expected.is_empty() {
        format!("unexpected {found}")
    } else {
        format!("found {found}, expected {}", expected.join(" or "))
    };
    (message, None)
}

/// Build the top-level query parser: search stage, then pipeline, then EOF.
fn query_parser<'src>() -> impl Parser<'src, ParserInput<'src>, Query, ParserExtra<'src>> {
    comment::leading_ws()
        .ignore_then(search::search_stage())
        .then(pipe::pipeline())
        .then_ignore(comment::ws())
        .then_ignore(end())
        .map(|(search, pipeline)| Query { search, pipeline })
        .labelled("query")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::*;

    #[test]
    fn test_simple_field_filter() {
        let query = parse("service=nginx").unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
        assert_eq!(query.pipeline.len(), 0);
    }

    #[test]
    fn test_multi_token_search() {
        let query = parse("service=nginx _severity=error last=2h").unwrap();
        // Time filter hoisted, 2 tokens remain in group.
        assert_eq!(query.search.groups[0].len(), 2);
        assert!(query.search.time_filter.is_some());
    }

    #[test]
    fn test_quoted_search() {
        let query = parse(r#""connection refused""#).unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
        assert_eq!(
            query.search.groups[0][0].node,
            SearchToken::QuotedSearch(QuotedSearch {
                phrase: "connection refused".to_string(),
            })
        );
    }

    #[test]
    fn test_negated_text_search() {
        let query = parse("-debug service=nginx").unwrap();
        assert_eq!(query.search.groups[0].len(), 2);
        assert_eq!(
            query.search.groups[0][0].node,
            SearchToken::TextSearch(TextSearch {
                term: "debug".to_string(),
                negated: true,
            })
        );
    }

    #[test]
    fn test_search_with_stats() {
        let query = parse("service=nginx last=1h | stats count() by host").unwrap();
        // Time filter hoisted, 1 token remains in group.
        assert_eq!(query.search.groups[0].len(), 1);
        assert!(query.search.time_filter.is_some());
        assert_eq!(query.pipeline.len(), 1);
        assert!(matches!(query.pipeline[0].node, PipeStage::Stats(_)));
    }

    #[test]
    fn test_full_pipeline() {
        let query = parse("service=nginx | stats count() by host | where count > 10 | sort -count")
            .unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
        assert_eq!(query.pipeline.len(), 3);
        assert!(matches!(query.pipeline[0].node, PipeStage::Stats(_)));
        assert!(matches!(query.pipeline[1].node, PipeStage::Where(_)));
        assert!(matches!(query.pipeline[2].node, PipeStage::Sort(_)));
    }

    #[test]
    fn test_stats_avg_table() {
        let query =
            parse("service=nginx | stats avg(duration) by status | table status, avg_duration")
                .unwrap();
        assert_eq!(query.pipeline.len(), 2);
        match &query.pipeline[0].node {
            PipeStage::Stats(stats) => {
                assert_eq!(stats.aggregations[0].function, "avg");
                assert_eq!(stats.group_by, vec!["status".to_string()]);
            }
            other => panic!("expected Stats, got {other:?}"),
        }
        match &query.pipeline[1].node {
            PipeStage::Table(table) => {
                assert_eq!(
                    table.fields,
                    vec!["status".to_string(), "avg_duration".to_string()]
                );
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }

    #[test]
    fn test_complex_query() {
        let query =
            parse("status>=400 last=24h | stats count() by host, uri | sort -count | limit 20")
                .unwrap();
        // Time filter hoisted, 1 token remains in group.
        assert_eq!(query.search.groups[0].len(), 1);
        assert!(query.search.time_filter.is_some());
        assert_eq!(query.pipeline.len(), 3);
        assert!(matches!(query.pipeline[0].node, PipeStage::Stats(_)));
        assert!(matches!(query.pipeline[1].node, PipeStage::Sort(_)));
        assert!(matches!(query.pipeline[2].node, PipeStage::Limit(_)));
    }

    #[test]
    fn test_parse_error() {
        let result = parse("| | invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_query() {
        let query = parse("").unwrap();
        assert!(query.search.groups.is_empty());
        assert_eq!(query.pipeline.len(), 0);
    }

    // --- error case tests ---

    #[test]
    fn test_error_invalid_pipe_stage() {
        let result = parse("service=nginx | bogus");
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(!errors.is_empty());
    }

    #[test]
    fn test_error_missing_stats_agg() {
        let result = parse("service=nginx | stats");
        assert!(result.is_err());
    }

    #[test]
    fn test_error_unclosed_paren() {
        let result = parse("service=nginx | where (count > 10");
        assert!(result.is_err());
    }

    #[test]
    fn test_error_unclosed_quote() {
        let result = parse(r#""unterminated string"#);
        assert!(result.is_err());
    }

    #[test]
    fn test_error_message_has_span() {
        let result = parse("| badstage");
        assert!(result.is_err());
        let errors = result.unwrap_err();
        for error in &errors {
            assert!(error.span.start <= error.span.end);
        }
    }

    #[test]
    fn test_error_integer_overflow_no_panic() {
        // overflowing integers must produce a parse error, not a panic
        let result = parse("x | where count > 99999999999999999999999");
        assert!(result.is_err());
    }

    #[test]
    fn test_query_length_limit_exceeded() {
        let long_query = "a".repeat(65_537);
        let result = parse(&long_query);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors[0].message.contains("query too long"));
    }

    #[test]
    fn test_query_at_length_limit_not_rejected_for_length() {
        // exactly at limit — should not fail with "query too long"
        // (may fail for syntax reasons, which is fine)
        let query = "a".repeat(65_536);
        let result = parse(&query);
        if let Err(errors) = &result {
            assert!(
                !errors.iter().any(|e| e.message.contains("query too long")),
                "should not be rejected for length at exact limit"
            );
        }
    }

    #[test]
    fn test_error_display() {
        let result = parse("| badstage");
        let errors = result.unwrap_err();
        let msg = errors[0].to_string();
        // should contain span info and the error description
        assert!(msg.contains('['));
        assert!(msg.contains(']'));
    }

    // --- enriched error message tests ---

    #[test]
    fn test_error_unknown_command_with_suggestion() {
        let result = parse("| staats count() by host");
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors[0].message.contains("unknown command"),
            "message should say unknown command, got: {}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("staats"),
            "message should contain the typo, got: {}",
            errors[0].message
        );
        assert_eq!(
            errors[0].hint.as_deref(),
            Some("did you mean 'stats'?"),
            "hint should suggest 'stats'"
        );
    }

    #[test]
    fn test_error_unknown_command_no_suggestion() {
        let result = parse("| zzzzzzz");
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors[0].message.contains("unknown command"),
            "message should say unknown command, got: {}",
            errors[0].message
        );
        assert!(
            errors[0].hint.is_none(),
            "hint should be None for unrecognizable command"
        );
    }

    #[test]
    fn test_error_unterminated_string() {
        let result = parse(r#""unterminated string"#);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors[0].message.contains("unterminated string"),
            "should detect unterminated string, got: {}",
            errors[0].message
        );
        assert!(errors[0].hint.is_some(), "should provide a hint");
    }

    #[test]
    fn test_error_unmatched_paren() {
        let result = parse("| where (count > 10");
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors[0].message.contains("unmatched opening parenthesis"),
            "should detect unmatched paren, got: {}",
            errors[0].message
        );
        assert!(errors[0].hint.is_some(), "should provide a hint");
    }

    #[test]
    fn test_error_display_with_hint() {
        let result = parse("| staats count()");
        let errors = result.unwrap_err();
        let msg = errors[0].to_string();
        assert!(
            msg.contains("did you mean"),
            "display should include hint, got: {msg}"
        );
    }

    #[test]
    fn test_error_whre_suggests_where() {
        let result = parse("| whre count > 10");
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert_eq!(errors[0].hint.as_deref(), Some("did you mean 'where'?"));
    }

    // --- comment tests ---

    #[test]
    fn test_hash_comment_at_start_of_input() {
        let query = parse("# this is a comment\nservice=nginx").unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
    }

    /// `//` is not a comment opener (ADR-0014 ruling 3), and the refusal
    /// is loud at the one place it could have been one: a bare term at a
    /// token boundary. Silently reading it as two AND-ed text terms would
    /// narrow the match set to nothing.
    #[test]
    fn test_double_slash_is_no_longer_a_comment() {
        let errors = parse("service=nginx // filter by service\n| stats count()").unwrap_err();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].message, comment::MSG_SLASHES_NOT_A_COMMENT);
        assert_eq!(errors[0].span, 14..16);
    }

    #[test]
    fn test_comment_inside_string_preserved() {
        let query = parse(r#""hello # world""#).unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
        assert_eq!(
            query.search.groups[0][0].node,
            SearchToken::QuotedSearch(QuotedSearch {
                phrase: "hello # world".to_string(),
            })
        );
    }

    /// A `//` in a value is ordinary data and needs no quoting, the
    /// everyday half of ADR-0014 ruling 3.
    #[test]
    fn test_double_slash_in_a_value_is_data() {
        for (dsl, field, value) in [
            ("url=https://example.com/x", "url", "https://example.com/x"),
            ("path=/api//v1", "path", "/api//v1"),
            ("url=//cdn.example.com/x", "url", "//cdn.example.com/x"),
        ] {
            let query = parse(dsl).unwrap_or_else(|e| panic!("{dsl} must parse: {e:?}"));
            assert_eq!(query.search.groups[0].len(), 1, "{dsl}");
            assert_eq!(
                query.search.groups[0][0].node,
                SearchToken::FieldFilter(FieldFilter {
                    field: field.to_string(),
                    op: FilterOp::Eq,
                    value: FilterValue::Single(Scalar::Exact(value.to_string())),
                }),
                "{dsl}"
            );
        }
        // …and a sibling filter on the same line survives.
        let query = parse("referrer=https://a.b/c status=200").expect("both filters");
        assert_eq!(query.search.groups[0].len(), 2);
    }

    #[test]
    fn test_double_slash_inside_string_preserved() {
        let query = parse(r#""http://example.com""#).unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
        assert_eq!(
            query.search.groups[0][0].node,
            SearchToken::QuotedSearch(QuotedSearch {
                phrase: "http://example.com".to_string(),
            })
        );
    }

    #[test]
    fn test_multiline_comments() {
        let query =
            parse("# find errors\nservice=nginx\n# only recent\nlast=1h | stats count() by host")
                .unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
        assert!(query.search.time_filter.is_some());
        assert_eq!(query.pipeline.len(), 1);
    }

    #[test]
    fn test_comment_at_end_of_input() {
        let query = parse("service=nginx # trailing").unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
    }

    #[test]
    fn test_only_comments() {
        let query = parse("# just a comment").unwrap();
        assert!(query.search.groups.is_empty());
        assert_eq!(query.pipeline.len(), 0);
    }

    #[test]
    fn test_only_comments_multiline() {
        let query = parse("# line 1\n# line 2\n# line 3").unwrap();
        assert!(query.search.groups.is_empty());
        assert_eq!(query.pipeline.len(), 0);
    }

    #[test]
    fn test_comment_error_spans_match_original() {
        // error should point to the original input position
        let result = parse("# comment\n| badstage");
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors[0].span.start >= 10, // after "# comment\n"
            "span should point into original input, got: {:?}",
            errors[0].span
        );
    }

    // ── backtick escape (ADR-0013 ruling 7) ─────────────────────────────

    /// A backticked name is a quoted context, so a `#` or `//` inside it
    /// is part of the name: the comment production is unreachable from
    /// inside [`primitives::quoted_name`].
    #[test]
    fn backticked_names_carry_comment_openers_verbatim() {
        for (dsl, want) in [("| table `a#b`", "a#b"), ("| table `a//b`", "a//b")] {
            let query = parse(dsl).unwrap_or_else(|e| panic!("{dsl} must parse: {e:?}"));
            match &query.pipeline[0].node {
                PipeStage::Table(t) => assert_eq!(t.fields, vec![want.to_string()], "{dsl}"),
                other => panic!("expected Table, got {other:?}"),
            }
        }
        let query = parse("| table `a//b`, host").expect("parses");
        match &query.pipeline[0].node {
            PipeStage::Table(t) => {
                assert_eq!(t.fields, vec!["a//b".to_string(), "host".to_string()]);
            }
            other => panic!("expected Table, got {other:?}"),
        }
        // …and the name reaches the AST intact.
        let query = parse("| table `a#b`").expect("parses");
        match &query.pipeline[0].node {
            PipeStage::Table(t) => assert_eq!(t.fields, vec!["a#b".to_string()]),
            other => panic!("expected Table, got {other:?}"),
        }
        // A comment after a closed backtick is still a comment.
        let query = parse("`a b`=x # trailing").expect("parses");
        assert_eq!(query.search.groups[0].len(), 1);
        match &query.search.groups[0][0].node {
            SearchToken::FieldFilter(ff) => assert_eq!(ff.field, "a b"),
            other => panic!("expected FieldFilter, got {other:?}"),
        }
    }

    /// A stray backtick inside a regex literal must not protect a later
    /// comment: the comment's own words would parse as extra AND-ed
    /// text-search terms and silently narrow the match set. A regex body
    /// is a production, not a scanner state, so a partner backtick later
    /// on cannot open a pseudo-name across the comment either.
    #[test]
    fn stray_backtick_does_not_shield_later_comments() {
        let query = parse("message=/back`tick/ # comment").expect("parses");
        assert_eq!(query.search.groups[0].len(), 1);

        // …and with a legitimate backticked name after the comment, whose
        // opening tick must not pair with the stray one before it.
        let input = "message=/a`b/ # secret note\n`req id`=1";
        let query = parse(input).unwrap_or_else(|e| panic!("{input:?} parses: {e:?}"));
        assert_eq!(
            query.search.groups[0].len(),
            2,
            "{input:?}: only the two filters, never the comment's words"
        );

        // The glob shape reaches the same invariant from the other side:
        // a bare value cannot absorb a tick, so the query is a loud parse
        // error rather than a comment quietly read as part of a value.
        assert!(parse("cmd=*`* # secret note\n`req id`=1").is_err());
    }

    /// No unquoted position absorbs a backtick (ADR-0013 ruling 7), so a
    /// value that runs into one is a loud parse error rather than a
    /// quietly different query.
    #[test]
    fn a_value_never_absorbs_a_backtick_shielded_comment() {
        for dsl in [
            "host=a+`b#c` # outside",
            "host=a*`b#c` # outside",
            "host=a%`b#c` # outside",
            "host=a-`b#c` # outside",
            // the plain typo, then the shapes whose comment words must
            // not become AND-ed search terms
            "service=`my service`",
            "host=`a # b` more",
            "status=200,`a # b` more",
        ] {
            assert!(parse(dsl).is_err(), "{dsl} must be a loud parse error");
        }

        // A value that genuinely contains a tick is double-quoted, and the
        // comment beside it is still a comment.
        let query = parse(r#"host="a+`b" # outside"#).expect("the quoted form parses");
        assert_eq!(query.search.groups[0].len(), 1);
        assert_eq!(
            query.search.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "host".to_string(),
                op: FilterOp::Eq,
                value: FilterValue::Single(Scalar::Exact("a+`b".to_string())),
            })
        );
    }

    /// A backticked name is a field reference in every field position,
    /// including directly after an operator — a descending sort key or
    /// arithmetic. It carries comment openers verbatim, because a quoted
    /// name has no whitespace-skip site inside it for the comment
    /// production to reach.
    #[test]
    fn a_backtick_after_an_operator_opens_a_name() {
        // the descending sort key, precisely: the name arrives whole
        for (dsl, want) in [
            ("* | sort -`a#b`", "a#b"),
            ("* | sort -`http://x`", "http://x"),
        ] {
            let query = parse(dsl).unwrap_or_else(|e| panic!("{dsl} must parse: {e:?}"));
            match &query.pipeline[0].node {
                PipeStage::Sort(s) => {
                    assert_eq!(s.fields[0].field, want);
                    assert_eq!(s.fields[0].direction, SortDirection::Desc);
                }
                other => panic!("expected Sort, got {other:?}"),
            }
        }

        // …and every other operator an expression can put in front of one
        for dsl in [
            "* | let x = 1+`a#b`",
            "* | let x = 2*`http://x`",
            "* | let x = `a#b`%2",
            "* | let x = 1-`a#b`",
            "* | where `a#b`>1",
            "* | stats count() by `a#b`",
        ] {
            assert!(parse(dsl).is_ok(), "{dsl} must parse");
        }

        // …and a tick inside a regex is ordinary body content, with the
        // trailing comment still a comment.
        let query = parse("host=/a+b`c/ # | bad_stage").expect("the comment is a comment");
        assert_eq!(
            query.pipeline.len(),
            0,
            "the comment must not reach the parser"
        );
        assert_eq!(query.search.groups[0].len(), 1);
    }

    /// A negated quoted name has no production in the search grammar, so
    /// it is a loud parse error. Reading it as text would search for the
    /// literal ticks (matching nothing), and dropping the `-` to a term of
    /// its own would silently discard the negation.
    #[test]
    fn a_negated_backtick_is_a_parse_error() {
        for dsl in ["-`http-status`=500", "-`a b`", "error -`x", "-`"] {
            assert!(parse(dsl).is_err(), "{dsl} must be a loud parse error");
        }
        // `NOT` is the spelling that negates a field filter, and it works.
        let query = parse("NOT `http-status`=500").expect("NOT negates a quoted name");
        assert_eq!(query.search.groups[0].len(), 1);
        assert!(matches!(
            &query.search.groups[0][0].node,
            SearchToken::Not(_)
        ));
        // An ordinary negated term is untouched.
        let query = parse("-debug").expect("parses");
        assert_eq!(
            query.search.groups[0][0].node,
            SearchToken::TextSearch(TextSearch {
                term: "debug".to_string(),
                negated: true,
            })
        );
    }

    /// An adversarial backtick run stays cheap: no pass over the text
    /// re-scans to end of input from a backtick byte, so these shapes
    /// terminate with an answer instead of going quadratic.
    #[test]
    fn a_backtick_run_parses_without_blowing_up() {
        let n = 65_536;
        for body in [
            "a``".repeat(n / 3),
            format!("{} x", "`".repeat(n - 2)),
            " ``a".repeat(n / 4),
        ] {
            let _ = parse(&body);
        }
    }

    /// The pathological corpus parses inside a 2 MiB stack, the size of a
    /// tokio worker thread, which is where trawld runs the parser.
    ///
    /// `cargo test`'s own threads get 8 MiB by default, so a suite that
    /// merely calls `parse` would keep passing while the server
    /// overflowed. A frame is not cheap either: every padding site is a
    /// project-owned combinator ([`comment::Spaced::spaced`]), which
    /// deepens the parser's nested type and with it the per-frame stack
    /// cost. So the corpus runs in a thread sized like the one that
    /// matters, and the test asserts it finishes rather than dies.
    ///
    /// Nesting is in the corpus at both sides of the grammar's depth
    /// bound: the deepest expression the DSL accepts must fit this stack
    /// (that is the bound's whole claim), and one far past it must reach
    /// a parse error rather than a recursion. What a level costs varies
    /// by orders of magnitude between builds, so it is the bound rather
    /// than any measurement that makes the claim true everywhere.
    #[test]
    fn the_pathological_corpus_parses_within_a_tokio_workers_stack() {
        /// A tokio worker thread's default stack.
        const TOKIO_WORKER_STACK: usize = 2 * 1024 * 1024;

        let deepest_legal = crate::parser::expr::MAX_EXPR_DEPTH;
        let corpus = vec![
            format!("*{}", "| head 1 ".repeat(2000)),
            format!("* | where a =={}1", " ".repeat(100_000)),
            "a=1 ".repeat(10_000),
            format!("a=1{}b=2", " ".repeat(60_000)),
            format!(
                "* | where {}1{}",
                "( ".repeat(deepest_legal),
                ") ".repeat(deepest_legal)
            ),
            format!(
                "* | where {}1{}",
                "abs(".repeat(deepest_legal),
                ")".repeat(deepest_legal)
            ),
            format!("* | where {}1{}", "( ".repeat(4000), ") ".repeat(4000)),
        ];

        std::thread::Builder::new()
            .stack_size(TOKIO_WORKER_STACK)
            .spawn(move || {
                for body in &corpus {
                    // the answer is not the point — reaching one is
                    let _ = parse(body);
                }
            })
            .expect("spawn")
            .join()
            .expect("the parser must not overflow a 2 MiB stack");
    }

    /// Expression nesting is bounded by the grammar, and the refusal is
    /// a parse error that names the bound — never a truncated query and
    /// never a stack overflow: the depth of a `where` clause is
    /// client-chosen, and `MAX_QUERY_LEN` alone lets `a(a(a(…` spell
    /// twenty thousand levels inside 64 KiB.
    ///
    /// All three recursion sites are covered — parentheses, a function
    /// call's arguments, an `in (…)` list — because each is one descent
    /// through the whole precedence chain.
    #[test]
    fn expression_nesting_past_the_bound_is_a_loud_parse_error() {
        let n = crate::parser::expr::MAX_EXPR_DEPTH;
        let message = format!("expression nests deeper than {n} levels");

        for (label, legal, over) in [
            (
                "parens",
                format!("* | where {}1{}", "(".repeat(n), ")".repeat(n)),
                format!("* | where {}1{}", "(".repeat(n + 1), ")".repeat(n + 1)),
            ),
            (
                "calls",
                format!("* | where {}1{}", "abs(".repeat(n), ")".repeat(n)),
                format!("* | where {}1{}", "abs(".repeat(n + 1), ")".repeat(n + 1)),
            ),
            (
                "in-list",
                // the list itself is one level, so it nests n-1 parens
                format!(
                    "* | where x in ({}1{})",
                    "(".repeat(n - 1),
                    ")".repeat(n - 1)
                ),
                format!("* | where x in ({}1{})", "(".repeat(n), ")".repeat(n)),
            ),
        ] {
            assert!(
                parse(&legal).is_ok(),
                "{label}: {n} levels is the bound, not past it"
            );
            let Err(errors) = parse(&over) else {
                panic!("{label}: nesting past the bound must be refused");
            };
            assert!(
                errors.iter().any(|e| e.message.contains(&message)),
                "{label}: refusal must name the bound, got {errors:?}"
            );
        }
    }

    /// A backtracked nesting level is given back: the depth counter is
    /// restored however chumsky leaves the level, so a query that tries
    /// (and abandons) many nested alternatives still parses its own
    /// legal nesting afterwards. A leak here would refuse valid queries
    /// only after a long enough prefix — the worst kind of bug to find.
    #[test]
    fn abandoned_nesting_does_not_spend_the_depth_budget() {
        let n = crate::parser::expr::MAX_EXPR_DEPTH;
        // Each `(x)` opens and leaves a level; hundreds of them in
        // sequence must not add up against the nesting that follows.
        let sequential = format!(
            "* | where {} and {}1{}",
            "(1) + ".repeat(500).trim_end_matches("+ "),
            "(".repeat(n),
            ")".repeat(n)
        );
        assert!(
            parse(&sequential).is_ok(),
            "sequential nesting must not accumulate: {sequential:.80}…"
        );

        // The counter is thread-local and a tokio worker is reused
        // across requests, so the claim that actually matters in
        // production: a parse that fails while holding nesting levels
        // (aborted mid-expression, or refused at the depth bound) must
        // not poison the next parse on the same thread.
        let abandoned_midway = format!("* | where {}1 +", "(".repeat(n - 1));
        assert!(parse(&abandoned_midway).is_err(), "prefix input must fail");
        let over_the_bound = format!("* | where {}1{}", "(".repeat(200), ")".repeat(200));
        assert!(
            parse(&over_the_bound).is_err(),
            "over-bound input must fail"
        );
        let legal_max = format!("* | where {}1{}", "(".repeat(n), ")".repeat(n));
        assert!(
            parse(&legal_max).is_ok(),
            "a failed parse must not spend this thread's depth budget"
        );
    }

    fn pattern_cap_refusal(input: &str) -> Option<ParseError> {
        match parse(input) {
            Ok(_) => None,
            Err(errors) => {
                assert_eq!(errors.len(), 1, "{input:.80}: {errors:?}");
                assert!(
                    errors[0].message.starts_with("too many patterns"),
                    "{input:.80}: {errors:?}"
                );
                Some(errors[0].clone())
            }
        }
    }

    /// `n` patterns spread over separate filters, list elements, globs and
    /// regexes, so every way of writing one counts toward the same total.
    fn mixed_patterns(n: usize) -> String {
        let mut tokens = Vec::new();
        let mut left = n;
        let mut i = 0;
        while left > 0 {
            let token = match (i % 3, left) {
                (0, _) => {
                    left -= 1;
                    format!("f{i}=/r{i}/")
                }
                (1, _) => {
                    left -= 1;
                    format!("f{i}!=g{i}*")
                }
                (_, 1) => {
                    left -= 1;
                    format!("f{i}=x{i}*,exact")
                }
                _ => {
                    left -= 2;
                    format!("f{i}=a{i}?,/b{i}/,exact")
                }
            };
            tokens.push(token);
            i += 1;
        }
        tokens.join(" ")
    }

    /// The pattern count walks the parsed query in a 2 MiB stack, the size
    /// of the tokio worker that parses it, however long a flat chain the
    /// query spells. The longest chain that fits `MAX_QUERY_LEN` is the
    /// corpus, in every position the walk enters an expression from.
    #[test]
    fn the_pattern_count_walks_a_flat_chain_within_a_tokio_workers_stack() {
        const TOKIO_WORKER_STACK: usize = 2 * 1024 * 1024;

        let chain = vec!["1"; 32_000].join("+");
        let corpus = [
            // 129 stages: the stage limit is the caller's check, after
            // `parse` returns
            format!("*{} | where {chain}", " | head 1".repeat(128)),
            format!("* | where {chain} matches /a/"),
            format!("* | let x = {chain}"),
            format!("* | stats sum({chain})"),
        ];
        let parsed = std::thread::Builder::new()
            .stack_size(TOKIO_WORKER_STACK)
            .spawn(move || corpus.iter().map(|body| parse(body)).collect::<Vec<_>>())
            .expect("spawn")
            .join()
            .expect("the pattern count must not overflow a 2 MiB stack");
        for result in &parsed {
            assert!(result.is_ok(), "{result:.80?}");
        }
        // Dropping a tree this deep recurses in its drop glue, which is
        // not the walk under test, so it gets a stack it fits in.
        std::thread::Builder::new()
            .stack_size(64 * TOKIO_WORKER_STACK)
            .spawn(move || drop(parsed))
            .expect("spawn")
            .join()
            .expect("drop");
    }

    /// The walk counts in text order inside a chain, so the span is the
    /// first `matches` pattern past the cap.
    #[test]
    fn pattern_cap_spans_the_first_pattern_past_it_in_a_chain() {
        let terms: Vec<String> = (0..=MAX_QUERY_PATTERNS)
            .map(|i| format!("a matches /r{i}/"))
            .collect();
        let over = format!("* | where {}", terms.join(" or "));
        let error = pattern_cap_refusal(&over).expect("one pattern over the cap");
        assert_eq!(&over[error.span], format!("/r{MAX_QUERY_PATTERNS}/"));
    }

    #[test]
    fn pattern_cap_admits_exactly_the_cap_across_filters_and_lists() {
        let at_cap = mixed_patterns(MAX_QUERY_PATTERNS);
        assert!(parse(&at_cap).is_ok(), "{at_cap:.80}");

        let over = mixed_patterns(MAX_QUERY_PATTERNS + 1);
        let error = pattern_cap_refusal(&over).expect("one pattern over the cap");
        assert_eq!(
            error.message,
            format!(
                "too many patterns ({} globs and regexes, max {MAX_QUERY_PATTERNS})",
                MAX_QUERY_PATTERNS + 1
            )
        );
        // The span is the token holding the first pattern past the cap.
        let last = over.rsplit(' ').next().unwrap();
        assert_eq!(&over[error.span.clone()], last);
    }

    #[test]
    fn pattern_cap_sums_one_list_and_separate_filters() {
        let list: Vec<String> = (0..MAX_QUERY_PATTERNS).map(|i| format!("/r{i}/")).collect();
        let one_list = format!("f={}", list.join(","));
        assert!(parse(&one_list).is_ok());
        assert!(pattern_cap_refusal(&format!("{one_list},z*")).is_some());
        // One more pattern in a separate filter, grouped and negated, still
        // counts toward the same total.
        assert!(pattern_cap_refusal(&format!("{one_list} (a OR NOT g=z*)")).is_some());
    }

    #[test]
    fn pattern_cap_ignores_exact_and_quoted_values() {
        let globs: Vec<String> = (0..MAX_QUERY_PATTERNS).map(|i| format!("g{i}*")).collect();
        let at_cap = format!("f={}", globs.join(","));
        let exact: Vec<String> = (0..1000).map(|i| format!("v{i}")).collect();
        let quoted: Vec<String> = (0..1000).map(|i| format!("\"q{i}*\"")).collect();
        for extra in [
            format!("e={}", exact.join(",")),
            format!("q={}", quoted.join(",")),
            "q=\"a*\" r=\"/x/\" host<\"*\"".to_string(),
        ] {
            let input = format!("{at_cap} {extra}");
            assert!(parse(&input).is_ok(), "{extra:.80}");
        }
    }

    #[test]
    fn pattern_cap_counts_pipeline_regexes() {
        let globs: Vec<String> = (0..MAX_QUERY_PATTERNS - 1)
            .map(|i| format!("g{i}*"))
            .collect();
        let one_short = format!("f={}", globs.join(","));
        for tail in [
            "| where host matches /a/",
            "| where host matches \"a\"",
            "| extract \"(?P<ip>\\d+)\"",
        ] {
            assert!(parse(&format!("{one_short} {tail}")).is_ok(), "{tail}");
            let over = format!("{one_short} {tail} | where msg matches /b/");
            let error = pattern_cap_refusal(&over).expect(tail);
            assert_eq!(&over[error.span], "/b/", "{tail}");
        }
        // `matches` against a field is not a literal pattern.
        assert!(parse(&format!("{one_short} f2=z* | where a matches b")).is_ok());
    }

    /// Parse `input` and count the regexes the parse compiled.
    fn parse_counting_builds(input: &str) -> (Result<Query, Vec<ParseError>>, usize) {
        primitives::REGEX_BUILDS.with(|n| n.set(0));
        let result = parse(input);
        (result, primitives::REGEX_BUILDS.with(std::cell::Cell::get))
    }

    /// No regex compiles before the pattern cap passes: a list of
    /// thousands of four-byte regexes, or of a few hundred that each
    /// compile large, is refused having compiled none.
    #[test]
    fn a_query_over_the_pattern_cap_compiles_no_regex() {
        for (label, element, n) in [
            ("tiny", "/a/", 16_383),
            ("large", r"/\w{100}/", MAX_QUERY_PATTERNS + 1),
        ] {
            let input = format!("f={}", vec![element; n].join(","));
            assert!(input.len() <= MAX_QUERY_LEN, "{label}");
            let (result, builds) = parse_counting_builds(&input);
            let errors = result.expect_err(label);
            assert!(
                errors[0].message.starts_with("too many patterns"),
                "{label}: {errors:?}"
            );
            assert_eq!(builds, 0, "{label}");
        }
    }

    /// A parse that already failed compiles nothing, however many regexes
    /// it read, whether the grammar stopped it or emitted an error and
    /// went on.
    #[test]
    fn a_parse_that_failed_compiles_no_regex() {
        let list = vec![r"/\w{100}/"; 300].join(",");
        for input in [
            format!("f={list} | bogus"),
            format!("f={list} g>x*"),
            format!("f={list} | where a matches /[/"),
        ] {
            let (result, builds) = parse_counting_builds(&input);
            assert!(result.is_err(), "{input:.40}…");
            assert_eq!(builds, 0, "{input:.40}…");
        }
    }

    /// A query under the cap compiles each regex once, in a filter and
    /// after `matches`, and reports those that do not compile as any
    /// parse reports its errors: in text order, capped, the count it
    /// left out annotated on the last.
    #[test]
    fn a_query_under_the_cap_compiles_each_regex_once() {
        let (result, builds) = parse_counting_builds("f=/a/,b,/c/ g=/d/ | where m matches /e/");
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(builds, 4);

        // An unknown class fails fast, so a debug build compiles 256 of
        // them in no time.
        let element = r"/\p{Nope}/";
        let input = format!("f={}", vec![element; MAX_QUERY_PATTERNS].join(","));
        let (result, builds) = parse_counting_builds(&input);
        let errors = result.expect_err("no regex compiles");
        assert_eq!(builds, MAX_QUERY_PATTERNS);
        assert_eq!(errors.len(), MAX_REPORTED_ERRORS);
        for (i, error) in errors[..MAX_REPORTED_ERRORS - 1].iter().enumerate() {
            assert!(error.message.starts_with("invalid regex:"), "{error:?}");
            let start = "f=".len() + i * (element.len() + 1);
            assert_eq!(error.span, start..start + element.len());
        }
        let last = &errors[MAX_REPORTED_ERRORS - 1];
        assert!(
            last.message
                .ends_with(&omitted_suffix(MAX_QUERY_PATTERNS - MAX_REPORTED_ERRORS)),
            "{last:?}"
        );
        assert_eq!(last.span.end, input.len());
    }

    /// A well-formed body that does not compile is refused after the
    /// parse, over its `/…/`: an unknown Unicode class is not a syntax
    /// error, so the grammar lets it through to the compile.
    #[test]
    fn a_well_formed_regex_that_does_not_compile_is_a_parse_error() {
        let errors = parse(r"msg=/\p{Nope}/").expect_err("unknown class");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.starts_with("invalid regex:"),
            "{errors:?}"
        );
        assert_eq!(errors[0].span, 4..14);
    }

    /// A regex over the compiled-size limit is a parse error over the
    /// whole `/…/`, in a filter and after `matches` alike, with a hint
    /// naming the usual cause.
    #[test]
    fn a_regex_too_large_once_compiled_is_a_parse_error() {
        for (dsl, span) in [
            (r"msg=/\w{100}/", 4..13),
            (r"msg=a,/\w{100}/", 6..15),
            (r"* | where msg matches /\w{100}/", 22..31),
        ] {
            let errors = parse(dsl).expect_err(dsl);
            assert_eq!(errors.len(), 1, "{dsl}: {errors:?}");
            assert_eq!(errors[0].message, primitives::MSG_REGEX_TOO_LARGE, "{dsl}");
            assert_eq!(errors[0].span, span, "{dsl}");
            assert!(
                errors[0]
                    .hint
                    .as_deref()
                    .is_some_and(|h| h.contains(r"\w+")),
                "{dsl}: {errors:?}"
            );
        }
    }

    /// The five admission errors of ADR-0061 (rulings 1-5), each with the
    /// span of the text to change and the hint its ruling names. Spans
    /// are bytes, so a multibyte prefix moves them by its length.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn new_admission_errors_carry_span_and_hint() {
        use comment::{
            HINT_NESTED_TIME_CLAUSE, HINT_NOT_DASH_BEFORE_BACKTICK, HINT_OR_ONLY_GROUP,
            HINT_QUOTE_OPENS_BARE_VALUE, HINT_REGEX_LINE_BREAK, HINT_REGEX_PIPE_STAGE,
        };
        use primitives::{MSG_QUOTE_OPENS_BARE_VALUE, MSG_REGEX_LINE_BREAK, MSG_REGEX_PIPE_STAGE};
        use search::{MSG_NESTED_TIME_CLAUSE, MSG_NOT_DASH_BEFORE_BACKTICK, MSG_OR_ONLY_GROUP};

        assert_eq!(HINT_REGEX_LINE_BREAK, r"write \n");
        assert_eq!(HINT_REGEX_PIPE_STAGE, r"drop the spaces or write \s");
        assert_eq!(
            HINT_NESTED_TIME_CLAUSE,
            "time clauses apply to the whole search; write it outside the group"
        );
        assert_eq!(HINT_QUOTE_OPENS_BARE_VALUE, "close the quote");

        // (query, message, span, hint): the query's one error
        let alone: [(&str, &str, std::ops::Range<usize>, &str); 22] = [
            // ruling 1: a raw line break, in a value and after `matches`
            (
                "msg=/a\nb/",
                MSG_REGEX_LINE_BREAK,
                4..9,
                HINT_REGEX_LINE_BREAK,
            ),
            (
                "\"é\" msg=/a\nb/",
                MSG_REGEX_LINE_BREAK,
                9..14,
                HINT_REGEX_LINE_BREAK,
            ),
            (
                "msg=/a\r\nb/ x",
                MSG_REGEX_LINE_BREAK,
                4..10,
                HINT_REGEX_LINE_BREAK,
            ),
            (
                "* | where m matches /a\nb/",
                MSG_REGEX_LINE_BREAK,
                20..25,
                HINT_REGEX_LINE_BREAK,
            ),
            // …and `|` after whitespace after `matches`, where nothing but
            // a regex can follow
            (
                "* | where m matches /a | b/",
                MSG_REGEX_PIPE_STAGE,
                20..27,
                HINT_REGEX_PIPE_STAGE,
            ),
            // ruling 2: a time clause inside parentheses or under NOT
            (
                "(last=1h)",
                MSG_NESTED_TIME_CLAUSE,
                1..8,
                HINT_NESTED_TIME_CLAUSE,
            ),
            (
                "NOT last=1h",
                MSG_NESTED_TIME_CLAUSE,
                4..11,
                HINT_NESTED_TIME_CLAUSE,
            ),
            (
                "NOT (last=1h)",
                MSG_NESTED_TIME_CLAUSE,
                5..12,
                HINT_NESTED_TIME_CLAUSE,
            ),
            (
                "NOT NOT last=1h",
                MSG_NESTED_TIME_CLAUSE,
                8..15,
                HINT_NESTED_TIME_CLAUSE,
            ),
            (
                "a (b OR earliest=\"2026-01-01T00:00:00Z\")",
                MSG_NESTED_TIME_CLAUSE,
                8..39,
                HINT_NESTED_TIME_CLAUSE,
            ),
            (
                "\"é\" NOT latest=\"x\"",
                MSG_NESTED_TIME_CLAUSE,
                9..19,
                HINT_NESTED_TIME_CLAUSE,
            ),
            // ruling 3: a bare value that opens with `"`, alone, in a list,
            // and under an ordered operator, which adds no error of its own
            (
                "host=\"*",
                MSG_QUOTE_OPENS_BARE_VALUE,
                5..7,
                HINT_QUOTE_OPENS_BARE_VALUE,
            ),
            (
                "f=a,\"b c",
                MSG_QUOTE_OPENS_BARE_VALUE,
                4..6,
                HINT_QUOTE_OPENS_BARE_VALUE,
            ),
            (
                "host<\"*",
                MSG_QUOTE_OPENS_BARE_VALUE,
                5..7,
                HINT_QUOTE_OPENS_BARE_VALUE,
            ),
            // ruling 4: a group of only OR separators, over the group alone
            ("(OR)", MSG_OR_ONLY_GROUP, 0..4, HINT_OR_ONLY_GROUP),
            (
                "a ( or OR ) b",
                MSG_OR_ONLY_GROUP,
                2..11,
                HINT_OR_ONLY_GROUP,
            ),
            (
                "\"é\" (OR) # c",
                MSG_OR_ONLY_GROUP,
                5..9,
                HINT_OR_ONLY_GROUP,
            ),
            // ruling 5: `NOT-` directly before a backtick
            (
                "NOT-`x`=1",
                MSG_NOT_DASH_BEFORE_BACKTICK,
                0..4,
                HINT_NOT_DASH_BEFORE_BACKTICK,
            ),
            (
                "a (NOT-`x`=1)",
                MSG_NOT_DASH_BEFORE_BACKTICK,
                3..7,
                HINT_NOT_DASH_BEFORE_BACKTICK,
            ),
            // …with non-ASCII layout between `NOT` and `-`, which the bare
            // word absorbs and the formatter cannot keep apart either
            (
                "NOT\u{a0}-`x`=1",
                MSG_NOT_DASH_BEFORE_BACKTICK,
                0..6,
                HINT_NOT_DASH_BEFORE_BACKTICK,
            ),
            (
                "NOT\u{2003}\u{a0}-`x`=1",
                MSG_NOT_DASH_BEFORE_BACKTICK,
                0..9,
                HINT_NOT_DASH_BEFORE_BACKTICK,
            ),
            (
                "a (NOT\u{a0}-`x`=1)",
                MSG_NOT_DASH_BEFORE_BACKTICK,
                3..9,
                HINT_NOT_DASH_BEFORE_BACKTICK,
            ),
        ];
        for (dsl, message, span, hint) in alone {
            let errors = parse(dsl).expect_err(dsl);
            assert_eq!(errors.len(), 1, "{dsl:?}: {errors:?}");
            assert_eq!(errors[0].message, message, "{dsl:?}");
            assert_eq!(errors[0].span, span, "{dsl:?}");
            assert_eq!(errors[0].hint.as_deref(), Some(hint), "{dsl:?}");
        }

        // A `|` after whitespace in a value is a new pipe stage, so the
        // run reads as a value and a stage. When the query then fails, the
        // run is named beside the error that falls inside it, never in
        // its place.
        for (dsl, span, natural) in [
            ("msg=/err | warn/", 4..16, "unknown command 'warn'"),
            ("msg=/a\nb | c/", 4..13, "unknown command 'c'"),
            ("path=/tmp* | wher x / 2", 5..21, "unknown command 'wher'"),
        ] {
            let errors = parse(dsl).expect_err(dsl);
            let messages: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
            assert_eq!(
                messages,
                [MSG_REGEX_PIPE_STAGE, natural],
                "{dsl:?}: {errors:?}"
            );
            assert_eq!(errors[0].span, span, "{dsl:?}");
            assert_eq!(
                errors[0].hint.as_deref(),
                Some(HINT_REGEX_PIPE_STAGE),
                "{dsl:?}"
            );
        }

        // Without a backtick after the `-`, layout after `NOT` still
        // separates it from a negated term, as it did before the guard.
        let not_dash_x = parse("NOT\u{a0}-x=1").expect("NOT nbsp -x=1");
        assert!(
            matches!(
                &not_dash_x.search.groups[0][0].node,
                crate::ast::SearchToken::Not(inner) if matches!(
                    &inner.node,
                    crate::ast::SearchToken::TextSearch(t) if t.term == "x=1" && t.negated
                )
            ),
            "{not_dash_x:?}"
        );

        // `-` before a backtick keeps its own refusal.
        let errors = parse("-`x`=1").expect_err("-`x`");
        assert!(
            errors
                .iter()
                .all(|e| e.message != MSG_NOT_DASH_BEFORE_BACKTICK),
            "{errors:?}"
        );
    }

    /// The legal neighbours of the five errors keep their readings.
    #[test]
    fn the_admission_errors_leave_their_neighbours_alone() {
        // a pipe stage after a slash-opening glob, on one line or two
        for dsl in [
            "path=/tmp* | where x / 2 == 1",
            "path=/tmp*\n| where x / 2 == 1",
        ] {
            let query = parse(dsl).unwrap_or_else(|e| panic!("{dsl:?}: {e:?}"));
            assert_eq!(
                query.search.groups[0][0].node,
                SearchToken::FieldFilter(FieldFilter {
                    field: "path".to_string(),
                    op: FilterOp::Eq,
                    value: FilterValue::Single(Scalar::Glob("/tmp*".to_string())),
                }),
                "{dsl:?}"
            );
            assert!(
                matches!(&query.pipeline[..], [stage] if matches!(stage.node, PipeStage::Where(_))),
                "{dsl:?}"
            );
        }
        // alternation without whitespace before the `|`
        let query = parse("msg=/err|warn/").expect("a regex");
        assert_eq!(
            query.search.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "msg".to_string(),
                op: FilterOp::Eq,
                value: FilterValue::Single(Scalar::Regex("err|warn".to_string())),
            })
        );
        // a top-level clause beside an OR keeps its slot
        let dsl = "a last=1h OR b";
        let query = parse(dsl).expect("a top-level clause");
        assert_eq!(query.search.groups.len(), 2);
        let slot = query.search.time_filter.expect("the clause is the window");
        assert_eq!(&dsl[slot.span], "last=1h");
        // a complete quoted value, and a quote later in a bare one
        assert!(parse(r#"host="*""#).is_ok());
        assert!(parse(r#"host=a"b"#).is_ok());
        // a group that holds a term beside its OR
        assert!(parse("(a OR)").is_ok());
        // `NOT-` before anything but a backtick is a word
        assert!(parse("NOT- `x`=1").is_ok());
        assert!(parse("NOT-x").is_ok());
    }

    /// A leading backtick that fails the quoted production is a loud
    /// parse error, never a quiet slide into text search: unterminated,
    /// empty, or holding a character that cannot render as itself. That
    /// covers control characters and the bidi and zero-width format
    /// characters `sanitize` neutralises, since a name that parses is
    /// echoed verbatim into notices and error messages.
    #[test]
    fn leading_backtick_that_is_not_a_name_is_a_parse_error() {
        for input in [
            "`unterminated",
            "``",
            "`a\nb`",
            "`foo` bar",
            "`a\u{202e}b`",
            "`a\u{200b}b`",
            "`a\u{00ad}b`",
        ] {
            assert!(
                parse(input).is_err(),
                "{input:?} must be a loud parse error, not text search"
            );
        }
    }
}
