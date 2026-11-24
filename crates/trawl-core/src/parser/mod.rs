// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! DSL parser for trawl's query language.

pub(crate) mod comment;
mod depth;
pub(crate) mod expr;
pub(crate) mod pipe;
pub(crate) mod primitives;
pub mod scan;
pub(crate) mod search;
pub mod suggest;

use chumsky::prelude::*;

use crate::ast::Query;
use crate::cancel::{Cancelled, Stop};
use crate::pattern::{PatternError, PatternKind, Preparer, price};
#[cfg(test)]
pub(crate) use depth::fixtures as depth_fixtures;
use primitives::{ParserExtra, ParserInput};
pub use primitives::{RegexCut, first_cut};

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub span: std::ops::Range<usize>,
    pub label: Option<String>,
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

/// `GET /api/v1/capabilities` advertises it as `max_query_bytes`.
pub const MAX_QUERY_LEN: usize = 65_536;

/// How many levels deep one expression, or a query's search, may be: the
/// most levels on any path from its root down to a leaf, as `context.md`'s
/// **Tree depth** counts them (ADR-0024). It is not the nesting bound
/// `expr::MAX_EXPR_DEPTH`, which counts only parentheses, call arguments
/// and `in` lists while the grammar runs.
pub const MAX_TREE_DEPTH: usize = 64;

/// How many levels the search grammar may recurse through, as a count of
/// the raw text, before the grammar reads the text at all. The count may
/// exceed a search's real depth, never fall short of it (`depth::prescan`
/// says where).
pub const MAX_SEARCH_PRESCAN: usize = 512;

pub const MAX_QUERY_PATTERNS: usize = 256;

/// The most diagnostics one parse reports. The cap is not a plain
/// truncation: the failure that ended the parse is appended last, and the
/// report is the first `MAX_REPORTED_ERRORS - 1` plus that final one.
const MAX_REPORTED_ERRORS: usize = 8;

fn omitted_suffix(n: usize) -> String {
    let plural = if n == 1 { "error" } else { "errors" };
    format!(" ({n} earlier {plural} omitted)")
}

pub fn parse(input: &str) -> Result<Query, Vec<ParseError>> {
    parse_cancellable(input, &Stop::never())
        .unwrap_or_else(|Cancelled| unreachable!("a stop nobody can request never cancels"))
}

/// [`parse`] without its last step: every refusal `parse` makes before it
/// builds a pattern, and no build. A query this admits can still be
/// refused by `parse`, when one of its patterns compiles past
/// [`crate::pattern::MAX_REGEX_COMPILED_BYTES`], or a regex body's
/// batch-engine view past [`crate::pattern::MAX_BATCH_COMPILED_BYTES`]:
/// only a build measures either.
pub fn parse_unbuilt(input: &str) -> Result<Query, Vec<ParseError>> {
    parse_unbuilt_cancellable(input, &Stop::never())
        .unwrap_or_else(|Cancelled| unreachable!("a stop nobody can request never cancels"))
}

/// [`parse`], reading `stop` between its steps (ADR-0063). A parse whose
/// stop is requested ends with [`Cancelled`] at its next check, never with
/// `Ok` and never with a diagnostic.
pub fn parse_cancellable(
    input: &str,
    stop: &Stop,
) -> Result<Result<Query, Vec<ParseError>>, Cancelled> {
    #[cfg(any(test, feature = "test-support"))]
    count_read(input, ParseKind::Built);
    let parsed = match admit(input, stop)? {
        Ok((query, sites)) => {
            let errors = regex_compile_errors(&sites, stop)?;
            if errors.is_empty() {
                Ok(query)
            } else {
                Err(errors)
            }
        }
        Err(errors) => Err(errors),
    };
    // The stop may have come while the last pattern built.
    checkpoint(stop)?;
    Ok(parsed)
}

pub fn parse_unbuilt_cancellable(
    input: &str,
    stop: &Stop,
) -> Result<Result<Query, Vec<ParseError>>, Cancelled> {
    #[cfg(any(test, feature = "test-support"))]
    count_read(input, ParseKind::Unbuilt);
    Ok(admit(input, stop)?.map(|(query, _)| query))
}

fn checkpoint(stop: &Stop) -> Result<(), Cancelled> {
    #[cfg(test)]
    tests::pass_point(stop);
    stop.check()
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(any(test, feature = "test-support"))]
fn count_read(input: &str, kind: ParseKind) {
    PARSES.with(|n| n.set(n.get() + 1));
    watch::record(input, kind);
}

/// How many queries [`parse`] and [`parse_unbuilt`] have read on this
/// thread. Take the difference across the work: other work on the same
/// thread counts too.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn thread_parses() -> usize {
    PARSES.with(std::cell::Cell::get)
}

#[cfg(any(test, feature = "test-support"))]
pub use watch::{ParseKind, ParseLog};

#[cfg(any(test, feature = "test-support"))]
mod watch {
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread::ThreadId;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ParseKind {
        Built,
        Unbuilt,
    }

    static LOGS: Mutex<Vec<Arc<Watched>>> = Mutex::new(Vec::new());

    #[derive(Debug)]
    struct Watched {
        marker: String,
        parses: Mutex<Vec<(ThreadId, ParseKind)>>,
    }

    /// Each test watches a marker of its own, so tests that run in parallel
    /// never see each other's parses. It records until it drops.
    #[derive(Debug)]
    pub struct ParseLog(Arc<Watched>);

    impl ParseLog {
        #[must_use]
        pub fn watch(marker: &str) -> Self {
            assert!(!marker.is_empty(), "a parse log watches a marker");
            let watched = Arc::new(Watched {
                marker: marker.to_owned(),
                parses: Mutex::new(Vec::new()),
            });
            logs().push(Arc::clone(&watched));
            Self(watched)
        }

        #[must_use]
        pub fn parses(&self) -> Vec<(ThreadId, ParseKind)> {
            self.0
                .parses
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl Drop for ParseLog {
        fn drop(&mut self) {
            logs().retain(|watched| !Arc::ptr_eq(watched, &self.0));
        }
    }

    fn logs() -> std::sync::MutexGuard<'static, Vec<Arc<Watched>>> {
        LOGS.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn record(input: &str, kind: ParseKind) {
        let thread = std::thread::current().id();
        for log in logs().iter().filter(|log| input.contains(&log.marker)) {
            log.parses
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((thread, kind));
        }
    }
}

type Admitted = Result<Result<(Query, Vec<primitives::RegexSite>), Vec<ParseError>>, Cancelled>;

fn admit(input: &str, stop: &Stop) -> Admitted {
    checkpoint(stop)?;
    if input.len() > MAX_QUERY_LEN {
        return Ok(Err(vec![ParseError {
            message: format!(
                "query too long ({} bytes, max {MAX_QUERY_LEN})",
                input.len()
            ),
            span: 0..input.len(),
            label: None,
            hint: None,
        }]));
    }
    // One linear pass, so it needs no cancellation point of its own.
    if let Some(error) = depth::prescan_error(input) {
        return Ok(Err(vec![error]));
    }

    let parser = query_parser();
    let mut regexes = primitives::RegexSites::default();
    let result = parser.parse_with_state(input, &mut regexes);
    checkpoint(stop)?;

    let query = match result.into_result() {
        Ok(query) => query,
        Err(errors) => {
            let mut reported = report_errors(&errors, input);
            name_pipe_cut_runs(&mut reported, &regexes.pipe_cut_runs);
            return Ok(Err(reported));
        }
    };
    if let Some(error) = depth::tree_depth_error(&query) {
        return Ok(Err(vec![error]));
    }
    let over_cap = pattern_cap_error(&query);
    checkpoint(stop)?;
    if let Some(error) = over_cap {
        return Ok(Err(vec![error]));
    }
    let over_budget = translation_budget_error(&regexes.sites);
    checkpoint(stop)?;
    if let Some(error) = over_budget {
        return Ok(Err(vec![error]));
    }
    Ok(Ok((query, regexes.sites)))
}

/// Name each `/…/` run a pipe stage cut out of a search value, beside the
/// first reported error that falls inside it (ADR-0061 ruling 1), never in
/// place of it: an unrelated mistake can sit inside the run too
/// (`path=/tmp* | wher x / 2`), and its own error is the one to fix.
fn name_pipe_cut_runs(errors: &mut Vec<ParseError>, runs: &[std::ops::Range<usize>]) {
    let mut named: Vec<&std::ops::Range<usize>> = Vec::new();
    for run in runs {
        if errors.len() >= MAX_REPORTED_ERRORS {
            return;
        }
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

fn translation_budget_error(sites: &[primitives::RegexSite]) -> Option<ParseError> {
    let limit = price::MAX_TRANSLATION_UNITS;
    let mut units: u64 = 0;
    let mut first_over = None;
    for site in sites {
        units = units.saturating_add(site.units);
        if units > limit && first_over.is_none() {
            first_over = Some(site.span.clone());
        }
    }
    let message = PatternError::Translation { units, limit }.to_string();
    Some(ParseError {
        hint: comment::hint_for(&message, None),
        message,
        span: first_over?,
        label: None,
    })
}

/// One ledger serves every site, so a pattern written twice builds once
/// and both sites report its error. After the first pattern that compiles
/// too large the ledger builds nothing new, and a site it did not build
/// reports nothing: the query is refused either way, and the work stays
/// one failed build.
fn regex_compile_errors(
    sites: &[primitives::RegexSite],
    stop: &Stop,
) -> Result<Vec<ParseError>, Cancelled> {
    let mut checker = Preparer::check().with_stop(stop.clone());
    let mut errors = Vec::new();
    for site in sites {
        checkpoint(stop)?;
        let error = match checker.check(site.kind, &site.text) {
            Ok(()) | Err(PatternError::Stopped) => continue,
            Err(PatternError::Cancelled) => return Err(Cancelled),
            Err(error) => error,
        };
        let message = primitives::regex_error_message(&error);
        errors.push(ParseError {
            hint: comment::hint_for(&message, None),
            message,
            span: site.span.clone(),
            label: None,
        });
    }
    if errors.len() > MAX_REPORTED_ERRORS {
        let omitted = errors.len() - MAX_REPORTED_ERRORS;
        let mut last = errors.pop().expect("the list is longer than the cap");
        errors.truncate(MAX_REPORTED_ERRORS - 1);
        last.message.push_str(&omitted_suffix(omitted));
        errors.push(last);
    }
    Ok(errors)
}

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
            PipeStage::Extract(x) => {
                if let ExtractMode::Regex(body) = &x.mode {
                    tally.add(extract_charge(body), &stage.span);
                }
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

fn extract_charge(body: &str) -> usize {
    // The grammar refused a body that is not regex syntax, so this one
    // parses; reading its groups builds nothing.
    crate::pattern::price::captures(body).map_or(1, |groups| groups.len().max(1))
}

enum Visit<'q> {
    Token(&'q crate::ast::Spanned<crate::ast::SearchToken>),
    Expr(&'q crate::ast::Spanned<crate::ast::Expr>),
    Pattern(&'q std::ops::Range<usize>),
}

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
        use crate::ast::Expr;

        match &expr.node {
            Expr::Binary { lhs, op, rhs } => {
                // A pattern operand is a regex the query builds, whether
                // `matches` spells it `/re/` or `"re"`. A clean parse holds
                // no other kind ([`crate::pattern::literal_operand`]).
                if PatternKind::of_operator(*op).is_some() {
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

fn rich_to_parse_error(e: &Rich<'_, char>, input: &str) -> ParseError {
    use chumsky::error::RichReason;

    let span = e.span();
    let label = e.contexts().next().map(|(l, _)| l.to_string());

    if let RichReason::Custom(raw) = e.reason() {
        let (msg, exact) = comment::split_payload(raw);
        return ParseError {
            message: comment::message_for(msg, exact),
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

fn find_pipe_command_word(input: &str, offset: usize) -> Option<String> {
    if !input.is_char_boundary(offset) {
        return None;
    }

    let before = &input[..offset];
    let word_start = before
        .rfind(|c: char| !c.is_alphanumeric() && c != '_')
        .map_or(0, |pos| {
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

fn preceded_by_whitespace(input: &str, offset: usize) -> bool {
    comment::opens_comment_after(input[..offset].chars().next_back())
}

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

fn comment_diagnostic(input: &str, offset: usize) -> Option<CommentDiagnostic> {
    if offset >= input.len() {
        return None;
    }
    let rest = &input[offset..];

    if rest.starts_with(comment::OPENER) && !preceded_by_whitespace(input, offset) {
        return Some(CommentDiagnostic::new(
            comment::MSG_OPENER_IN_TOKEN,
            offset,
            comment::OPENER.len_utf8(),
        ));
    }

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

    #[test]
    fn pipe_cut_names_stay_within_the_error_cap() {
        let error = |at: usize| ParseError {
            message: format!("error at {at}"),
            span: at..at + 1,
            label: None,
            hint: None,
        };
        let runs = [0..4, 10..14];

        let mut full: Vec<ParseError> = (0..MAX_REPORTED_ERRORS).map(|i| error(i * 10)).collect();
        name_pipe_cut_runs(&mut full, &runs);
        assert_eq!(full.len(), MAX_REPORTED_ERRORS);
        assert!(
            full.iter()
                .all(|e| e.message != primitives::MSG_REGEX_PIPE_STAGE)
        );

        let mut short: Vec<ParseError> = (0..MAX_REPORTED_ERRORS - 1)
            .map(|i| error(i * 10))
            .collect();
        name_pipe_cut_runs(&mut short, &runs);
        assert_eq!(short.len(), MAX_REPORTED_ERRORS);
        assert_eq!(short[0].message, primitives::MSG_REGEX_PIPE_STAGE);
        assert_eq!(short[0].span, 0..4);
    }
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
        assert!(msg.contains('['));
        assert!(msg.contains(']'));
    }

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

    #[test]
    fn test_hash_comment_at_start_of_input() {
        let query = parse("# this is a comment\nservice=nginx").unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
    }

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
        let result = parse("# comment\n| badstage");
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors[0].span.start >= 10, // after "# comment\n"
            "span should point into original input, got: {:?}",
            errors[0].span
        );
    }

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
        let query = parse("| table `a#b`").expect("parses");
        match &query.pipeline[0].node {
            PipeStage::Table(t) => assert_eq!(t.fields, vec!["a#b".to_string()]),
            other => panic!("expected Table, got {other:?}"),
        }
        let query = parse("`a b`=x # trailing").expect("parses");
        assert_eq!(query.search.groups[0].len(), 1);
        match &query.search.groups[0][0].node {
            SearchToken::FieldFilter(ff) => assert_eq!(ff.field, "a b"),
            other => panic!("expected FieldFilter, got {other:?}"),
        }
    }

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

        assert!(parse("cmd=*`* # secret note\n`req id`=1").is_err());
    }

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

    #[test]
    fn a_backtick_after_an_operator_opens_a_name() {
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

        let query = parse("host=/a+b`c/ # | bad_stage").expect("the comment is a comment");
        assert_eq!(
            query.pipeline.len(),
            0,
            "the comment must not reach the parser"
        );
        assert_eq!(query.search.groups[0].len(), 1);
    }

    #[test]
    fn a_negated_backtick_is_a_parse_error() {
        for dsl in ["-`http-status`=500", "-`a b`", "error -`x", "-`"] {
            assert!(parse(dsl).is_err(), "{dsl} must be a loud parse error");
        }
        let query = parse("NOT `http-status`=500").expect("NOT negates a quoted name");
        assert_eq!(query.search.groups[0].len(), 1);
        assert!(matches!(
            &query.search.groups[0][0].node,
            SearchToken::Not(_)
        ));
        let query = parse("-debug").expect("parses");
        assert_eq!(
            query.search.groups[0][0].node,
            SearchToken::TextSearch(TextSearch {
                term: "debug".to_string(),
                negated: true,
            })
        );
    }

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

    /// `cargo test`'s own threads get 8 MiB by default, so a suite that
    /// merely calls `parse` would keep passing while the server
    /// overflowed. So the corpus runs in a thread sized like the one that
    /// matters, and the test asserts it finishes rather than dies.
    #[test]
    fn the_pathological_corpus_parses_within_a_tokio_workers_stack() {
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
            // the deepest nesting the search pre-scan lets the grammar read
            format!(
                "* | where {}1{}",
                "( ".repeat(MAX_SEARCH_PRESCAN),
                ") ".repeat(MAX_SEARCH_PRESCAN)
            ),
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

    #[test]
    fn abandoned_nesting_does_not_spend_the_depth_budget() {
        let n = crate::parser::expr::MAX_EXPR_DEPTH;
        // Each `(x)` opens and leaves a level; hundreds of them in
        // sequence must not add up against the nesting that follows. They
        // sit side by side in one list, so the tree stays two levels deep.
        let sequential = format!(
            "* | where a in ({}) and {}1{}",
            vec!["(1)"; 500].join(", "),
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

    fn grammar_tree(dsl: &str) -> Query {
        let mut regexes = primitives::RegexSites::default();
        query_parser()
            .parse_with_state(dsl, &mut regexes)
            .into_result()
            .unwrap_or_else(|errors| panic!("{dsl:.80}: the grammar refused it: {errors:?}"))
    }

    fn true_depth(query: &Query) -> usize {
        use crate::ast::{AggExpr, Expr, PipeStage, Spanned};

        fn expr(e: &Expr) -> usize {
            let deepest = |children: &mut dyn Iterator<Item = &Spanned<Expr>>| {
                1 + children.map(|c| expr(&c.node)).max().unwrap_or(0)
            };
            match e {
                Expr::Binary { lhs, op: _, rhs } => deepest(&mut [&**lhs, &**rhs].into_iter()),
                Expr::Unary { op: _, operand } => deepest(&mut std::iter::once(&**operand)),
                Expr::FunctionCall { name: _, args } => deepest(&mut args.iter()),
                Expr::InList { expr, list } => {
                    deepest(&mut std::iter::once(&**expr).chain(list.iter()))
                }
                Expr::FieldRef(_) | Expr::Literal(_) => 0,
            }
        }
        fn token(t: &SearchToken) -> usize {
            match t {
                SearchToken::Not(inner) => 1 + token(&inner.node),
                SearchToken::Group(groups) => {
                    1 + groups
                        .iter()
                        .flatten()
                        .map(|t| token(&t.node))
                        .max()
                        .unwrap_or(0)
                }
                SearchToken::FieldFilter(_)
                | SearchToken::TextSearch(_)
                | SearchToken::TimeFilter(_)
                | SearchToken::QuotedSearch(_)
                | SearchToken::EarliestFilter(_)
                | SearchToken::LatestFilter(_) => 0,
            }
        }
        fn args(aggs: &[AggExpr]) -> Vec<&Spanned<Expr>> {
            aggs.iter().flat_map(|a| &a.args).collect()
        }
        let roots = query.pipeline.iter().flat_map(|stage| match &stage.node {
            PipeStage::Where(w) => vec![&w.condition],
            PipeStage::Let(l) => l.assignments.iter().map(|(_, v)| v).collect(),
            PipeStage::Stats(s) => args(&s.aggregations),
            PipeStage::EventStats(s) => args(&s.aggregations),
            PipeStage::Timechart(t) => args(&t.aggregations),
            PipeStage::Pivot(p) => args(std::slice::from_ref(&p.aggregation)),
            _ => Vec::new(),
        });
        query
            .search
            .groups
            .iter()
            .flatten()
            .map(|t| token(&t.node))
            .chain(roots.map(|root| expr(&root.node)))
            .max()
            .unwrap_or(0)
    }

    fn depth_refusal(kind: depth_fixtures::Kind) -> (&'static str, &'static str) {
        match kind {
            depth_fixtures::Kind::Search => (
                "search is deeper than 64 levels",
                "each NOT and each pair of brackets is a level: use fewer nested brackets or negations",
            ),
            depth_fixtures::Kind::Expression => (
                "expression is deeper than 64 levels",
                "each operator, not, minus sign, call and in list is a level: write equality tests on one field as field in (…), and split a long and across several | where stages",
            ),
        }
    }

    #[test]
    fn tree_depth_limit_admits_64_and_refuses_65() {
        assert_eq!(MAX_TREE_DEPTH, 64);
        let at_limit = depth_fixtures::shapes(MAX_TREE_DEPTH);
        let over = depth_fixtures::shapes(MAX_TREE_DEPTH + 1);
        assert_eq!(at_limit.len(), 19);
        for (at, over) in at_limit.iter().zip(&over) {
            let name = at.name;
            assert_eq!(
                true_depth(&grammar_tree(&at.text)),
                64,
                "{name}: {}",
                at.text
            );
            assert_eq!(
                true_depth(&grammar_tree(&over.text)),
                65,
                "{name}: {}",
                over.text
            );

            if let Err(errors) = parse(&at.text) {
                panic!("{name}: 64 levels is the limit, not past it: {errors:?}");
            }
            let errors = parse(&over.text).expect_err(name);
            assert_eq!(errors.len(), 1, "{name}: {errors:?}");
            let (message, hint) = depth_refusal(over.kind);
            assert_eq!(errors[0].message, message, "{name}");
            assert_eq!(errors[0].hint.as_deref(), Some(hint), "{name}");
            assert_eq!(errors[0].label, None, "{name}");
            assert_eq!(&over.text[errors[0].span.clone()], over.refused, "{name}");
        }
    }

    #[test]
    fn every_parse_door_enforces_tree_depth() {
        use depth_fixtures::{Kind, chain, comparisons};

        let over = MAX_TREE_DEPTH + 1;
        let search = format!("{}a=1{}", "(".repeat(over), " )".repeat(over));
        let expression = format!("* | where {}", comparisons(over, "or"));
        let too_many = format!("f={}", vec!["/a/"; MAX_QUERY_PATTERNS + 1].join(","));
        let too_costly = format!("(?i){}", r"[\x00-\x{10FFFF}]".repeat(15));
        for (dsl, kind) in [(&search, Kind::Search), (&expression, Kind::Expression)] {
            let label = format!("{dsl:.40}");
            let refusal = parse(dsl).expect_err(&label);
            assert_eq!(refusal.len(), 1, "{label}: {refusal:?}");
            assert_eq!(refusal[0].message, depth_refusal(kind).0, "{label}");
            assert_eq!(parse_unbuilt(dsl).as_ref().err(), Some(&refusal), "{label}");
            for door in [parse_cancellable, parse_unbuilt_cancellable] {
                let (refused, points) = tripped(None, || door(dsl, &Stop::new()));
                assert_eq!(refused.ok().and_then(Result::err), Some(refusal.clone()));
                let (_, grammar_error) = tripped(None, || door("a | where", &Stop::new()));
                let (_, over_cap) = tripped(None, || door(&too_many, &Stop::new()));
                assert_eq!(points, grammar_error, "{label}");
                assert_eq!(over_cap, points + 1, "{label}");
            }
        }

        let first = |dsl: &str| {
            let errors = parse(dsl).expect_err(dsl);
            assert_eq!(errors.len(), 1, "{dsl:.40}: {errors:?}");
            errors[0].clone()
        };
        let deep_where = format!(" | where {}", comparisons(over, "or"));
        let deep_expr = first(&format!("{too_many}{deep_where}"));
        assert_eq!(deep_expr.message, depth_refusal(Kind::Expression).0);
        let deep_expr = first(&format!("f=/{too_costly}/{deep_where}"));
        assert_eq!(deep_expr.message, depth_refusal(Kind::Expression).0);
        let deep_search = first(&format!("{too_many} {search}"));
        assert_eq!(deep_search.message, depth_refusal(Kind::Search).0);
        let grammar = first(&format!("*{deep_where} | where"));
        assert!(!grammar.message.contains("deeper than"), "{grammar:?}");

        let both = format!("{search}{deep_where}");
        // a bracket group's span takes the whitespace after it
        assert_eq!(&both[first(&both).span], format!("{search} "));
        let two = format!("* | let x = {}, y = {} | where a > 0", chain(66), chain(67));
        let error = first(&two);
        assert_eq!(&two[error.span], chain(66));
    }

    #[test]
    fn tree_depth_is_not_width_or_text() {
        let over = MAX_TREE_DEPTH + 1;
        let brackets = "(".repeat(over);
        // A capture group is a regex position, and a pattern holds 64, so
        // the regex bodies nest groups that capture nothing.
        let groups = "(?:".repeat(over);
        let closes = ")".repeat(over);
        let nots = "not ".repeat(over);
        let big_nots = "NOT ".repeat(over);
        let elements: Vec<String> = (0..5_000).map(|i| i.to_string()).collect();
        let cases = [
            (format!("* | where a in ({})", elements.join(", ")), 1),
            (
                format!("* | where coalesce({}) > 0", vec!["a"; 5_000].join(", ")),
                2,
            ),
            (format!(r#"msg="{brackets}{nots}{big_nots}""#), 0),
            (format!(r#""{brackets} {big_nots}""#), 0),
            (format!(r#"* | where m == "{brackets}{nots}{big_nots}""#), 1),
            (format!("`{brackets}{nots}{big_nots}x`=1"), 0),
            (format!("* | where `{brackets}{nots}x` == 1"), 1),
            (
                format!("a=1 # {brackets}{big_nots}\n| where b == 1 # {brackets}{nots}"),
                1,
            ),
            (format!("msg=/{groups}a{closes}/"), 0),
            (format!("* | where m matches /{groups}a{closes}/"), 1),
        ];
        for (dsl, depth) in &cases {
            let label = format!("{dsl:.60}");
            if let Err(errors) = parse(dsl) {
                panic!("{label}: {errors:?}");
            }
            assert_eq!(true_depth(&grammar_tree(dsl)), *depth, "{label}");
        }
    }

    /// The count of cancellation points is thread-local, so the doors are
    /// tripped on the small thread itself.
    #[test]
    fn a_parse_cancelled_after_the_grammar_drops_a_deep_tree() {
        const TOKIO_WORKER_STACK: usize = 2 * 1024 * 1024;

        let deep = [
            (
                "search",
                format!(
                    "{}a=1{}",
                    "(".repeat(MAX_SEARCH_PRESCAN),
                    ")".repeat(MAX_SEARCH_PRESCAN)
                ),
            ),
            (
                "expression",
                format!("* | where {} > 0", vec!["1"; 32_000].join("+")),
            ),
        ];
        std::thread::Builder::new()
            .stack_size(TOKIO_WORKER_STACK)
            .spawn(move || {
                for (case, dsl) in &deep {
                    assert!(dsl.len() <= MAX_QUERY_LEN, "{case}: {}", dsl.len());
                    for door in [parse_cancellable, parse_unbuilt_cancellable] {
                        // point 0 is the start, point 1 follows the grammar
                        let (parsed, points) = tripped(Some(1), || door(dsl, &Stop::new()));
                        assert!(matches!(parsed, Err(Cancelled)), "{case}");
                        assert_eq!(points, 2, "{case}");
                    }
                }
            })
            .expect("spawn")
            .join()
            .expect("a cancelled parse must drop its tree on a 2 MiB stack");
    }

    fn prescan_count(text: &str) -> usize {
        depth::prescan(text, usize::MAX).expect("no bound to pass")
    }

    #[test]
    fn the_search_prescan_admits_512_and_refuses_513() {
        type Nest = fn(usize) -> String;

        assert_eq!(MAX_SEARCH_PRESCAN, 512);
        let shapes: [(&str, Nest); 3] = [
            ("brackets", |n| {
                format!("{}a=1{}", "(".repeat(n), " )".repeat(n))
            }),
            ("NOT run", |n| format!("{}a=1", "NOT ".repeat(n))),
            ("NOT and brackets", |n| {
                format!(
                    "{}{}a=1{}",
                    if n % 2 == 1 { "NOT " } else { "" },
                    "NOT (".repeat(n / 2),
                    " )".repeat(n / 2)
                )
            }),
        ];
        let (message, hint) = depth_refusal(depth_fixtures::Kind::Search);
        let unbuilt = |dsl: &str| tripped(None, || parse_unbuilt_cancellable(dsl, &Stop::new()));
        let (_, grammar_points) = unbuilt("a | where");
        let (_, length_points) = unbuilt(&"x".repeat(MAX_QUERY_LEN + 1));
        assert_eq!(grammar_points, length_points + 1);
        for (name, shape) in shapes {
            let at = shape(MAX_SEARCH_PRESCAN);
            assert_eq!(prescan_count(&at), MAX_SEARCH_PRESCAN, "{name}");
            assert_eq!(true_depth(&grammar_tree(&at)), MAX_SEARCH_PRESCAN, "{name}");
            let (parsed, points) = unbuilt(&at);
            let errors = parsed.expect("not cancelled").expect_err(name);
            assert_eq!(points, grammar_points, "{name}: the grammar ran");
            assert_eq!(errors.len(), 1, "{name}: {errors:?}");
            assert_eq!(errors[0].message, message, "{name}");
            // the tree depth check spans the top-level token
            assert_eq!(errors[0].span, 0..at.len(), "{name}");

            let over = shape(MAX_SEARCH_PRESCAN + 1);
            assert_eq!(prescan_count(&over), MAX_SEARCH_PRESCAN + 1, "{name}");
            let (parsed, points) = unbuilt(&over);
            let errors = parsed.expect("not cancelled").expect_err(name);
            assert_eq!(points, length_points, "{name}: the grammar never ran");
            assert_eq!(parse(&over).as_ref().err(), Some(&errors), "{name}");
            assert_eq!(errors.len(), 1, "{name}: {errors:?}");
            assert_eq!(errors[0].message, message, "{name}");
            assert_eq!(errors[0].hint.as_deref(), Some(hint), "{name}");
            assert_eq!(errors[0].label, None, "{name}");
            let span = errors[0].span.clone();
            assert!(
                ["(", "NOT"].contains(&&over[span.clone()]),
                "{name}: {span:?}"
            );
            assert_eq!(
                prescan_count(&over[..span.start]),
                MAX_SEARCH_PRESCAN,
                "{name}: the span is where the count passes the bound"
            );
        }
    }

    #[test]
    fn a_search_the_prescan_refuses_builds_no_tree() {
        let deep = format!("{}a=1{}", "(".repeat(30_000), ")".repeat(30_000));
        assert!(deep.len() <= MAX_QUERY_LEN, "{}", deep.len());
        let too_long = "x".repeat(MAX_QUERY_LEN + 1);
        for door in [parse_cancellable, parse_unbuilt_cancellable] {
            let (refused, points) = tripped(None, || door(&deep, &Stop::new()));
            let errors = refused.expect("not cancelled").expect_err("refused");
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert_eq!(errors[0].message, "search is deeper than 64 levels");
            assert_eq!(errors[0].span, MAX_SEARCH_PRESCAN..MAX_SEARCH_PRESCAN + 1);
            let (_, length_points) = tripped(None, || door(&too_long, &Stop::new()));
            assert_eq!(points, length_points);
        }
    }

    #[test]
    fn the_prescan_counts_at_least_what_the_search_nests() {
        let units = [
            ("quoted value", r#"( ")" "#),
            ("stray quote in a value", r#"( a=x"y ")" "#),
            ("backticked name", "( `)`=1 "),
            ("regex body", r"( a=/\)/ "),
            ("comment", "( # )\n"),
            ("comment after NOT", "NOT # )\n("),
            ("bracket in a comment after NOT", "NOT # (a)\n("),
        ];
        for (name, unit) in units {
            let nest = |n: usize| format!("{}a=1{}", unit.repeat(n), " )".repeat(n));
            let text = nest(100);
            let depth = true_depth(&grammar_tree(&text));
            assert!(depth >= 100, "{name}: {depth}");
            let count = prescan_count(&text);
            assert!(count >= depth, "{name}: counted {count} of {depth}");

            let over = nest(MAX_SEARCH_PRESCAN + 1);
            let (parsed, points) = tripped(None, || parse_unbuilt_cancellable(&over, &Stop::new()));
            let errors = parsed.expect("not cancelled").expect_err(name);
            assert_eq!(points, 1, "{name}: the grammar never ran");
            assert_eq!(
                errors[0].message,
                depth_refusal(depth_fixtures::Kind::Search).0,
                "{name}"
            );
        }
    }

    #[test]
    fn the_prescan_counts_by_its_rules() {
        for (text, count) in [
            ("(a) (b) (c)", 1),
            ("((a) (b))", 2),
            ("NOT a NOT b NOT c", 1),
            ("NOT NOT a", 2),
            ("NOT(NOT(a))", 4),
            ("NOT (a) NOT (b)", 2),
            // a no-break space is layout to the grammar too
            ("NOT\u{a0}(a) b", 2),
            ("NOTICE (a) NOT_A NOT1", 1),
            ("* | stats count() by a | where (b) > 1", 1),
            // a `NOT` needs no word boundary before it
            ("xNOT (a)", 2),
            // brackets and `NOT`s count in quotes and comments
            (r#"msg="(((NOT a""#, 4),
            ("a=1 # (((\n", 3),
            // a group that holds a `"`, backtick, `/` or `#` stays open
            (r#"(a="x") (b="y")"#, 2),
            ("* | where (a / 2) > (b)", 2),
            ("(a # c\n) (b)", 2),
            // so do the `NOT`s before a comment
            ("NOT # c\n(a) (b)", 2),
        ] {
            assert_eq!(prescan_count(text), count, "{text:?}");
        }
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

    #[test]
    fn the_pattern_count_walks_a_deep_tree_within_a_tokio_workers_stack() {
        use depth_fixtures::{flat_chain, spliced};

        const TOKIO_WORKER_STACK: usize = 2 * 1024 * 1024;
        const TERMS: usize = 32_000;

        // Each tree drops on the small stack as soon as it is counted.
        let counted = std::thread::Builder::new()
            .stack_size(TOKIO_WORKER_STACK)
            .spawn(|| {
                [
                    format!("*{} | where DEEP", " | head 1".repeat(128)),
                    "* | where DEEP matches /a/".to_owned(),
                    "* | let x = DEEP".to_owned(),
                    "* | stats sum(DEEP)".to_owned(),
                ]
                .iter()
                .map(|template| pattern_cap_error(&spliced(template, flat_chain(TERMS))))
                .collect::<Vec<_>>()
            })
            .expect("spawn")
            .join()
            .expect("the pattern count and the drop must not overflow a 2 MiB stack");
        assert_eq!(counted, [None, None, None, None]);
    }

    /// The chains are 50 terms long, under [`MAX_TREE_DEPTH`], so the pattern
    /// that passes the cap sits in the middle of the sixth.
    #[test]
    fn pattern_cap_spans_the_first_pattern_past_it_in_a_chain() {
        let terms: Vec<String> = (0..MAX_QUERY_PATTERNS + 5)
            .map(|i| format!("a matches /r{i}/"))
            .collect();
        let stages: Vec<String> = terms.chunks(50).map(|chain| chain.join(" or ")).collect();
        let over = format!("* | where {}", stages.join(" | where "));
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
        let last = over.rsplit(' ').next().unwrap();
        assert_eq!(&over[error.span.clone()], last);
    }

    #[test]
    fn pattern_cap_sums_one_list_and_separate_filters() {
        let list: Vec<String> = (0..MAX_QUERY_PATTERNS).map(|i| format!("/r{i}/")).collect();
        let one_list = format!("f={}", list.join(","));
        assert!(parse(&one_list).is_ok());
        assert!(pattern_cap_refusal(&format!("{one_list},z*")).is_some());
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
            "| where host like \"a%\"",
            "| where host ilike \"a%\"",
            "| extract \"(?P<ip>\\d+)\"",
        ] {
            assert!(parse(&format!("{one_short} {tail}")).is_ok(), "{tail}");
            let over = format!("{one_short} {tail} | where msg matches /b/");
            let error = pattern_cap_refusal(&over).expect(tail);
            assert_eq!(&over[error.span], "/b/", "{tail}");
        }
        let errors =
            parse(&format!("{one_short} f2=z* | where a matches b")).expect_err("computed");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].message, crate::pattern::MSG_PATTERN_NOT_LITERAL);
    }

    fn quoted(body: &str) -> String {
        body.replace('\\', r"\\").replace('"', "\\\"")
    }

    fn at(dsl: &str, span: &std::ops::Range<usize>) -> String {
        let text = &dsl[span.clone()];
        if text.len() > 40 {
            format!("{}…", &text[..text.floor_char_boundary(40)])
        } else {
            text.to_string()
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn every_regex_body_is_validated() {
        use crate::pattern::{MAX_REGEX_LEN, MSG_REGEX_TOO_LARGE};
        type Operand = fn(&str) -> (String, String);

        type Site = fn(&str) -> (String, String);
        let regex_sites: [(&str, Site); 5] = [
            ("search value", |b| (format!("msg=/{b}/"), format!("/{b}/"))),
            ("list element", |b| {
                (format!("msg=a,/{b}/"), format!("/{b}/"))
            }),
            ("matches /re/", |b| {
                (format!("* | where msg matches /{b}/"), format!("/{b}/"))
            }),
            ("matches \"re\"", |b| {
                let q = format!("\"{}\"", quoted(b));
                (format!("* | where msg matches {q}"), q)
            }),
            ("extract", |b| {
                let q = format!("\"{b}\"");
                (format!("* | extract {q} from msg"), q)
            }),
        ];
        // Exactly the limit, in few positions: an empty group is four
        // bytes that match nothing, so the body stays inside the repeat
        // budget too. A body of 1024 literals would not (`repeat_budget`).
        let at_limit = format!("a{}bcd", "(?:)".repeat(255));
        assert_eq!(at_limit.len(), MAX_REGEX_LEN);
        let over_limit = "a".repeat(MAX_REGEX_LEN + 1);

        for (site, spell) in regex_sites {
            for (body, want) in [
                ("[", "invalid regex:".to_string()),
                (
                    over_limit.as_str(),
                    format!("regex pattern too long (1025 bytes, max {MAX_REGEX_LEN})"),
                ),
                (r"\w{64}", MSG_REGEX_TOO_LARGE.to_string()),
            ] {
                let (dsl, covered) = spell(body);
                let errors = parse(&dsl).expect_err(site);
                assert_eq!(errors.len(), 1, "{site} {body:.20}: {errors:?}");
                assert!(
                    errors[0].message.starts_with(&want),
                    "{site} {body:.20}: {errors:?}"
                );
                assert_eq!(
                    at(&dsl, &errors[0].span),
                    at(&covered, &(0..covered.len())),
                    "{site} {body:.20}"
                );
            }
            let (dsl, _) = spell(&at_limit);
            assert!(parse(&dsl).is_ok(), "{site}: {:?}", parse(&dsl));
        }

        // The limit counts the body as the query decodes it: `\"` is one
        // byte of a quoted `matches` string, so 1025 written bytes are 1024
        // decoded ones. An `extract` string decodes nothing.
        let decoded_at_limit = format!("\"{}bcd", "(?:)".repeat(255));
        assert_eq!(decoded_at_limit.len(), MAX_REGEX_LEN);
        let dsl = format!("* | where msg matches \"{}\"", quoted(&decoded_at_limit));
        assert_eq!(
            dsl.len(),
            "* | where msg matches \"\"".len() + MAX_REGEX_LEN + 1
        );
        assert!(parse(&dsl).is_ok(), "{:?}", parse(&dsl));

        // A glob, `like` and `ilike` operand is capped at the same 1024
        // bytes, before the repeat budget. The regex each becomes is
        // longer, and is not capped again. Every character of a `like`
        // operand is a position, and so is each anchor of the `^…$` it
        // becomes, so 256 four-byte characters pass the length cap and the
        // repeat budget refuses them: 62 are the most a `like` operand
        // holds. A glob class is one position however many members it
        // lists, so a glob reaches the length cap.
        let operand_sites: [(&str, &str, Operand); 3] = [
            ("glob", "glob", |t| (format!("host={t}"), t.to_string())),
            ("like", "like", |t| {
                let q = format!("\"{}\"", quoted(t));
                (format!("* | where host like {q}"), q)
            }),
            ("ilike", "ilike", |t| {
                let q = format!("\"{}\"", quoted(t));
                (format!("* | where host ilike {q}"), q)
            }),
        ];
        let four_bytes = "\u{1F600}".repeat(MAX_REGEX_LEN / 4);
        let glob_at_limit = format!("*[{}]", "a".repeat(MAX_REGEX_LEN - 3));
        let most_like = "\u{1F600}".repeat(62);
        for (site, noun, spell) in operand_sites {
            let at_limit = if site == "glob" {
                glob_at_limit.clone()
            } else {
                four_bytes.clone()
            };
            assert_eq!(at_limit.len(), MAX_REGEX_LEN, "{site}");
            let (dsl, _) = spell(&at_limit);
            if site == "glob" {
                assert!(parse(&dsl).is_ok(), "{site}: {:?}", parse(&dsl));
            } else {
                let errors = parse(&dsl).expect_err(site);
                assert_eq!(
                    errors[0].message, "pattern repeats too much (258 positions, max 64)",
                    "{site}"
                );
                let (dsl, _) = spell(&most_like);
                assert!(parse(&dsl).is_ok(), "{site}: {:?}", parse(&dsl));
            }

            let over = format!("{at_limit}*");
            let (dsl, covered) = spell(&over);
            let errors = parse(&dsl).expect_err(site);
            assert_eq!(errors.len(), 1, "{site}: {errors:?}");
            assert_eq!(
                errors[0].message,
                format!("{noun} pattern too long (1025 bytes, max {MAX_REGEX_LEN})"),
                "{site}"
            );
            assert_eq!(at(&dsl, &errors[0].span), at(&covered, &(0..covered.len())));
        }
    }

    #[test]
    fn a_computed_pattern_operand_is_refused_with_a_hint() {
        for (dsl, operand) in [
            (r#"* | where message matches lower("a")"#, r#"lower("a")"#),
            ("* | where message matches field", "field"),
            ("* | where host like field", "field"),
            (
                r#"* | where host ilike concat("a", "%")"#,
                r#"concat("a", "%")"#,
            ),
            (r#"* | let m = host matches "a" + "b""#, r#""a" + "b""#),
            ("* | stats count(host like 1)", "1"),
            ("* | where not (host matches null)", "null"),
        ] {
            let errors = parse(dsl).expect_err(dsl);
            assert_eq!(errors.len(), 1, "{dsl}: {errors:?}");
            assert_eq!(
                errors[0].message,
                crate::pattern::MSG_PATTERN_NOT_LITERAL,
                "{dsl}"
            );
            assert_eq!(&dsl[errors[0].span.clone()], operand, "{dsl}");
            assert_eq!(
                errors[0].hint.as_deref(),
                Some(comment::HINT_PATTERN_NOT_LITERAL),
                "{dsl}"
            );
        }
        for dsl in [
            "* | where message matches /a/",
            r#"* | where message matches "a""#,
            r#"* | where message matches ("a")"#,
            r#"* | where host like "a%" and host ilike "%B""#,
        ] {
            assert!(parse(dsl).is_ok(), "{dsl}");
        }
    }

    fn parse_counting_builds(input: &str) -> (Result<Query, Vec<ParseError>>, usize) {
        let before = crate::pattern::thread_builds();
        let result = parse(input);
        (result, crate::pattern::thread_builds() - before)
    }

    /// `\w{64}` compiles past the size limit, and `\B(?:\pL?){15}\PL` past
    /// the batch engine's.
    #[test]
    fn parse_unbuilt_refuses_what_parse_refuses_before_it_builds() {
        let unbuilt = |input: &str| {
            let before = crate::pattern::thread_builds();
            let result = parse_unbuilt(input);
            assert_eq!(crate::pattern::thread_builds(), before, "{input}");
            result
        };
        let too_many = format!("f={}", vec!["/a/"; MAX_QUERY_PATTERNS + 1].join(","));
        let too_costly = format!("(?i){}", r"[\x00-\x{10FFFF}]".repeat(15));
        for input in [
            "a | where",
            "f=/[/",
            &format!("f=/{}/", "a".repeat(1025)),
            r"f=/\d{1000}/",
            "* | where host like status",
            &too_many,
            &format!("f=/{too_costly}/"),
        ] {
            let refused = parse(input).expect_err(input);
            assert_eq!(unbuilt(input).expect_err(input), refused, "{input}");
        }

        for (input, message) in [
            (r"f=/\w{64}/", crate::pattern::MSG_REGEX_TOO_LARGE),
            (
                r"f=/\B(?:\pL?){15}\PL/",
                crate::pattern::MSG_BATCH_TOO_LARGE,
            ),
        ] {
            let (built, builds) = parse_counting_builds(input);
            assert_eq!(builds, 1, "{input}");
            assert_eq!(built.expect_err("too large")[0].message, message);
            assert!(unbuilt(input).is_ok(), "{input}");
        }

        for input in [r#"msg=/err|warn/ | where host like "web%""#, "x"] {
            assert_eq!(unbuilt(input), parse(input), "{input}");
        }
    }

    #[test]
    fn thread_parses_counts_each_query_read_once() {
        let before = thread_parses();
        assert!(parse("a=1 | head 2").is_ok());
        assert!(parse("a | where").is_err());
        assert!(parse_unbuilt("f=/a/").is_ok());
        assert!(parse(&"x".repeat(MAX_QUERY_LEN + 1)).is_err());
        assert_eq!(thread_parses() - before, 4);
    }

    thread_local! {
        static POINTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        /// The point, counted from 0, at which a parse on this thread
        /// requests its own stop.
        static TRIP_AT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    }

    pub(super) fn pass_point(stop: &Stop) {
        let point = POINTS.get();
        POINTS.set(point + 1);
        if TRIP_AT.get() == Some(point) {
            stop.request();
        }
    }

    /// `run`, with the stop of the parse in it requested at cancellation
    /// point `trip` (from 0), or at none, and how many points it passed.
    fn tripped<T>(trip: Option<usize>, run: impl FnOnce() -> T) -> (T, usize) {
        POINTS.set(0);
        TRIP_AT.set(trip);
        let out = run();
        TRIP_AT.set(None);
        (out, POINTS.get())
    }

    fn every_ending() -> Vec<String> {
        let too_costly = format!("(?i){}", r"[\x00-\x{10FFFF}]".repeat(15));
        vec![
            "a=1 | head 2".to_owned(),
            r#"f=/a/ g=/b/ | where m matches "c""#.to_owned(),
            "x".repeat(MAX_QUERY_LEN + 1),
            "a | where".to_owned(),
            format!("f={}", vec!["/a/"; MAX_QUERY_PATTERNS + 1].join(",")),
            format!("f=/{too_costly}/"),
            r"f=/\w{64}/".to_owned(),
            format!(
                "* | where {}",
                depth_fixtures::comparisons(MAX_TREE_DEPTH + 1, "or")
            ),
            format!("{}a=1", "(".repeat(MAX_SEARCH_PRESCAN + 1)),
        ]
    }

    #[test]
    fn a_cancelled_parse_is_never_ok_and_never_a_diagnostic() {
        let ends = every_ending();
        assert!(
            parse(&ends[5]).expect_err("over the translation budget")[0]
                .message
                .starts_with(crate::pattern::MSG_TRANSLATION_BUDGET)
        );
        for input in &ends {
            let label = format!("{input:.40}");
            assert_eq!(
                parse_cancellable(input, &Stop::never()),
                Ok(parse(input)),
                "{label}"
            );
            assert_eq!(
                parse_unbuilt_cancellable(input, &Stop::new()),
                Ok(parse_unbuilt(input)),
                "{label}"
            );

            let requested = Stop::new();
            requested.request();
            let before = crate::pattern::thread_builds();
            assert_eq!(
                parse_cancellable(input, &requested),
                Err(Cancelled),
                "{label}"
            );
            assert_eq!(
                parse_unbuilt_cancellable(input, &requested),
                Err(Cancelled),
                "{label}"
            );
            assert_eq!(crate::pattern::thread_builds(), before, "{label}");

            for door in [parse_cancellable, parse_unbuilt_cancellable] {
                let (_, points) = tripped(None, || door(input, &Stop::new()));
                assert!(points > 0, "{label}");
                for trip in 0..points {
                    let (parsed, _) = tripped(Some(trip), || door(input, &Stop::new()));
                    assert_eq!(parsed, Err(Cancelled), "{label} at point {trip}");
                }
            }
        }
    }

    #[test]
    fn a_parse_stops_between_its_pattern_builds() {
        let input = r"f=/a/ g=/b/ | where m matches /c/";
        let built = |trip| {
            let before = crate::pattern::thread_builds();
            let (parsed, points) = tripped(trip, || parse_cancellable(input, &Stop::new()));
            (
                parsed.map(|_| ()),
                points,
                crate::pattern::thread_builds() - before,
            )
        };
        let (parsed, points, builds) = built(None);
        assert_eq!((parsed, builds), (Ok(()), 3));
        // the start, the grammar, the pattern cap, the translation
        // budget, one before each build and one after the last
        assert_eq!(points, 8);
        let first_build = 4;
        for trip in 0..points {
            let (parsed, _, builds) = built(Some(trip));
            assert_eq!(parsed, Err(Cancelled), "at point {trip}");
            assert_eq!(
                builds,
                trip.saturating_sub(first_build).min(3),
                "at point {trip}"
            );
        }
    }

    #[test]
    fn a_parse_log_records_the_thread_and_kind_of_each_marked_parse() {
        let marker = "parselog_a1f3";
        let other = ParseLog::watch("parselog_b7c9");
        let log = ParseLog::watch(marker);
        let here = std::thread::current().id();
        assert!(parse(&format!("service={marker}")).is_ok());
        assert!(parse_unbuilt(&format!("service={marker} | head 1")).is_ok());
        assert!(parse("service=unmarked").is_ok());
        let stop = Stop::new();
        stop.request();
        assert_eq!(
            parse_cancellable(&format!("{marker} | where"), &stop),
            Err(Cancelled)
        );
        let there = std::thread::spawn(move || {
            assert!(parse(&format!("{marker} | where")).is_err());
            std::thread::current().id()
        })
        .join()
        .expect("the parse thread ends");
        assert_ne!(here, there);
        assert_eq!(
            log.parses(),
            vec![
                (here, ParseKind::Built),
                (here, ParseKind::Unbuilt),
                (here, ParseKind::Built),
                (there, ParseKind::Built),
            ]
        );
        assert!(other.parses().is_empty());
    }

    #[test]
    fn a_query_over_the_pattern_cap_compiles_no_regex() {
        for (label, element, n) in [
            ("tiny", "/a/", 16_383),
            ("large", r"/\w{64}/", MAX_QUERY_PATTERNS + 1),
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

    #[test]
    fn a_parse_that_failed_compiles_no_regex() {
        let list = vec![r"/\w{64}/"; 300].join(",");
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

    #[test]
    fn a_query_under_the_cap_compiles_each_regex_once() {
        let (result, builds) = parse_counting_builds("f=/a/,b,/c/ g=/d/ | where m matches /e/");
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(builds, 4);

        let element = r"/\p{Nope}/";
        let input = format!("f={}", vec![element; MAX_QUERY_PATTERNS].join(","));
        let (result, builds) = parse_counting_builds(&input);
        let errors = result.expect_err("no regex compiles");
        assert_eq!(builds, 1);
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

    #[test]
    fn a_regex_too_large_once_compiled_is_a_parse_error() {
        for (dsl, span) in [
            (r"msg=/\w{64}/", 4..12),
            (r"msg=a,/\w{64}/", 6..14),
            (r"* | where msg matches /\w{64}/", 22..30),
        ] {
            let errors = parse(dsl).expect_err(dsl);
            assert_eq!(errors.len(), 1, "{dsl}: {errors:?}");
            assert_eq!(
                errors[0].message,
                crate::pattern::MSG_REGEX_TOO_LARGE,
                "{dsl}"
            );
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

    /// Spans are bytes, so a multibyte prefix moves them by its length.
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
        // separates it from a negated term.
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
