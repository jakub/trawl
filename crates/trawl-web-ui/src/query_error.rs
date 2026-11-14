// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a query error says, and where it points (ADR-0039).
//!
//! Two owners, never crossed. The server owns whether a query ran and
//! why not: its details index the *effective query it received*, so an
//! excerpt quotes that text and nothing else. The local parser owns the
//! marks on the draft in the editor. Nothing here maps one text's spans
//! into the other.
//!
//! Everything below is text in, text out: the notice component renders a
//! [`NoticeModel`] as text nodes and the draft diagnostic line renders a
//! [`DraftDiagnostic`]. Spans are UTF-8 byte offsets; columns are
//! character counts. A caret is not placed by counting: its line repeats
//! the text before the span, hidden, so the browser lays it out exactly as
//! it laid out the quoted line, wide and combining glyphs and tabs alike.
//!
//! Only the wasm32 build consumes these helpers outside the tests.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::{ErrorDetail, ErrorSpan};
use trawl_core::text_layout::locate_span;

use crate::api_error::ApiError;

/// A span resolved against the text it indexes: the one line that holds
/// its start, and where on that line it sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// 1-based line number.
    pub line: usize,
    /// 1-based character column of the span's start on that line.
    pub column: usize,
    /// Whether the text has more than one line, which is when an excerpt
    /// names the line it shows.
    pub multi_line: bool,
    /// The line holding the span's start, without its line ending.
    pub line_text: String,
    /// The line's text before the span's start.
    pub before: String,
    /// How many characters of the line the span covers, at least one.
    pub width: usize,
}

/// Resolve a byte span against `text` with
/// [`trawl_core::text_layout::locate_span`], in character columns.
///
/// `None` when the span cannot index this text: reversed, past the end,
/// or off a character boundary at either end. The caller then keeps the
/// message and drops the excerpt; a caret under the wrong character is
/// worse than none.
///
/// A zero-width span, including one at the end of the text, gets one
/// caret where it sits. A span running past the end of its line is cut
/// there. `\r\n` is one line ending.
#[must_use]
pub fn locate(text: &str, span: &ErrorSpan) -> Option<Located> {
    let at = locate_span(text, span.start, span.end)?;
    let before = at.before(text);
    Some(Located {
        line: at.line,
        column: before.chars().count() + 1,
        multi_line: at.multi_line,
        line_text: at.line_text(text).to_owned(),
        before: before.to_owned(),
        width: at.covered(text).chars().count().max(1),
    })
}

/// A message and its hint as one sentence, the way the editor's lint
/// tooltip has always joined them.
#[must_use]
pub fn join_hint(message: &str, hint: Option<&str>) -> String {
    match hint {
        Some(hint) => format!("{message} — {hint}"),
        None => message.to_owned(),
    }
}

/// Which failure the notice reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lead {
    /// The server refused a snapshot query.
    Query,
    /// A live stream closed before it opened, and the local parser
    /// refuses the text it sent. Each sentence of this lead has to be
    /// true on its own: the stream did not start, and the query does
    /// have a syntax error. It does not claim the one caused the other.
    LiveSyntax,
}

/// The sent text at one span: the line, and the caret line under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excerpt {
    /// `line N: ` for a multi-line text, else empty.
    pub prefix: String,
    /// The quoted line.
    pub text: String,
    /// What the caret line holds before its carets: the prefix and the
    /// quoted line up to the span, rendered hidden so the carets start
    /// where the span's first glyph was laid out above them.
    pub pad: String,
    /// One `^` per character the span covers on that line (at least one).
    pub caret: String,
}

impl Excerpt {
    fn at(sent: &str, span: &ErrorSpan) -> Option<Self> {
        let located = locate(sent, span)?;
        let prefix = if located.multi_line {
            format!("line {}: ", located.line)
        } else {
            String::new()
        };
        let pad = format!("{prefix}{}", located.before);
        Some(Self {
            prefix,
            text: located.line_text,
            pad,
            caret: "^".repeat(located.width),
        })
    }
}

/// One detail as the notice shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// The detail's message with its hint. `None` for a lone detail,
    /// whose message is already the headline.
    pub message: Option<String>,
    /// The sent text under the detail's span; `None` when it has no span
    /// or the span does not index the sent text.
    pub excerpt: Option<Excerpt>,
}

/// Everything the query error notice renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeModel {
    /// The first line's verdict: what failed, in the notice's own words.
    pub verdict: String,
    /// What the first line says after the verdict and a `: `: the
    /// server's message or the error count. `None` when the verdict is
    /// the whole line. The notice shows a lone detail's message under its
    /// excerpt instead, when it has one.
    pub detail: Option<String>,
    /// One block per detail that has something to show, in the order the
    /// details arrived.
    pub blocks: Vec<Block>,
}

impl NoticeModel {
    /// Build the notice for `details` against `sent`, the exact text they
    /// index. `summary` is the envelope's message, the headline when no
    /// detail came with it.
    ///
    /// One detail puts its message (and hint) in the headline and shows
    /// only its excerpt below. More than one counts them in the headline
    /// and gives each its own message and excerpt, in server order. The
    /// parser's "(N earlier errors omitted)" rides on the last message
    /// and so renders as it arrived.
    #[must_use]
    pub fn build(lead: Lead, details: &[ErrorDetail], summary: &str, sent: &str) -> Self {
        let excerpt = |d: &ErrorDetail| d.span.as_ref().and_then(|s| Excerpt::at(sent, s));
        let joined = |d: &ErrorDetail| join_hint(&d.message, d.hint.as_deref());
        match details {
            [] => Self::single(lead, summary.to_owned(), Vec::new()),
            [only] => Self::single(
                lead,
                joined(only),
                excerpt(only)
                    .map(|e| Block {
                        message: None,
                        excerpt: Some(e),
                    })
                    .into_iter()
                    .collect(),
            ),
            many => {
                let (verdict, detail) = match lead {
                    Lead::Query => (
                        "Couldn't run the query".to_owned(),
                        Some(format!("{} errors", many.len())),
                    ),
                    Lead::LiveSyntax => (
                        format!(
                            "Couldn't start the live stream. The query has {} syntax errors.",
                            many.len()
                        ),
                        None,
                    ),
                };
                Self {
                    verdict,
                    detail,
                    blocks: many
                        .iter()
                        .map(|d| Block {
                            message: Some(joined(d)),
                            excerpt: excerpt(d),
                        })
                        .collect(),
                }
            }
        }
    }

    /// The notice whose first line carries one message after its verdict.
    fn single(lead: Lead, message: String, blocks: Vec<Block>) -> Self {
        let verdict = match lead {
            Lead::Query => "Couldn't run the query",
            Lead::LiveSyntax => "Couldn't start the live stream. The query has a syntax error",
        };
        Self {
            verdict: verdict.to_owned(),
            detail: Some(message),
            blocks,
        }
    }

    /// The first line as one text, as the notice lays it out.
    #[cfg(test)]
    fn headline(&self) -> String {
        match &self.detail {
            Some(detail) => format!("{}: {detail}", self.verdict),
            None => self.verdict.clone(),
        }
    }
}

/// The notice for a snapshot request that failed with `error` after
/// sending `sent`. `None` unless the server refused the text itself: every
/// other failure keeps its generic copy and its Retry (ADR-0039).
///
/// `sent` must be the effective query that request carried, never the
/// draft or the URL's current query, because the spans index it.
#[must_use]
pub fn refusal_notice(error: &ApiError, sent: &str) -> Option<NoticeModel> {
    let envelope = error.query_error()?;
    Some(NoticeModel::build(
        Lead::Query,
        &envelope.details,
        &envelope.message,
        sent,
    ))
}

/// The notice for a live stream that closed before it opened, when the
/// local parser also refuses `sent`, the exact text the stream was
/// opened with. `None` when that text parses: then nothing says the
/// query is at fault, and the generic copy with its Retry stands.
///
/// The browser cannot read why the stream closed (ADR-0039), so this
/// never claims the syntax error was the reason; its lead says two
/// things that are each true on their own.
#[must_use]
pub fn live_syntax_notice(sent: &str) -> Option<NoticeModel> {
    let errors = trawl_core::parser::parse(sent).err()?;
    Some(NoticeModel::build(
        Lead::LiveSyntax,
        &local_details(&errors),
        "",
        sent,
    ))
}

/// The local parser's errors in the wire's detail shape, so the live
/// syntax notice renders through the same [`NoticeModel`] as a server
/// refusal.
#[must_use]
pub fn local_details(errors: &[trawl_core::parser::ParseError]) -> Vec<ErrorDetail> {
    errors
        .iter()
        .map(|e| ErrorDetail {
            message: e.message.clone(),
            span: Some(ErrorSpan {
                start: e.span.start,
                end: e.span.end,
            }),
            label: e.label.clone(),
            hint: e.hint.clone(),
        })
        .collect()
}

/// What the draft diagnostic reports: errors that block the draft, or
/// advice on a draft that parses (ADR-0068). Errors take precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftKind {
    Error,
    Advice,
}

/// The draft diagnostic: the local parser's verdict on the draft, as
/// visible text under the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftDiagnostic {
    pub kind: DraftKind,
    /// The first error or advice note, in full.
    pub first: String,
    /// Every further one, behind the `+N more` disclosure.
    pub rest: Vec<String>,
    /// Advice notes the core left out past its cap. Always 0 for errors.
    pub omitted: usize,
}

/// The line that counts advice notes left out past the cap.
#[must_use]
pub fn omitted_advice_note(omitted: usize) -> String {
    if omitted == 1 {
        "and 1 more advice note not shown".to_owned()
    } else {
        format!("and {omitted} more advice notes not shown")
    }
}

/// `Line L:C — message — hint`, or the joined message alone when the span
/// cannot index `doc`.
fn draft_line(doc: &str, start: usize, end: usize, message: &str, hint: Option<&str>) -> String {
    let message = join_hint(message, hint);
    match locate(doc, &ErrorSpan { start, end }) {
        Some(at) => format!("Line {}:{} — {message}", at.line, at.column),
        None => message,
    }
}

/// Parse `doc` locally and describe each error as `Line L:C — message`,
/// with the hint joined on. A draft that parses gets its advice the same
/// way, if it has any. `None` when the draft parses with no advice.
#[must_use]
pub fn draft_diagnostic(doc: &str) -> Option<DraftDiagnostic> {
    let (kind, lines, omitted): (_, Vec<String>, _) = match trawl_core::parser::parse(doc) {
        Err(errors) => (
            DraftKind::Error,
            errors
                .iter()
                .map(|e| draft_line(doc, e.span.start, e.span.end, &e.message, e.hint.as_deref()))
                .collect(),
            0,
        ),
        Ok(query) => {
            let report = trawl_core::advice::advise(&query);
            (
                DraftKind::Advice,
                report
                    .findings
                    .iter()
                    .map(|a| draft_line(doc, a.span.start, a.span.end, &a.message, Some(&a.hint)))
                    .collect(),
                report.omitted,
            )
        }
    };
    let mut lines = lines.into_iter();
    let first = lines.next()?;
    Some(DraftDiagnostic {
        kind,
        first,
        rest: lines.collect(),
        omitted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "last=15m service=kubelet | stats count( by host";

    fn span(start: usize, end: usize) -> ErrorSpan {
        ErrorSpan { start, end }
    }

    fn detail(message: &str, at: Option<(usize, usize)>, hint: Option<&str>) -> ErrorDetail {
        ErrorDetail {
            message: message.to_owned(),
            span: at.map(|(s, e)| span(s, e)),
            label: None,
            hint: hint.map(str::to_owned),
        }
    }

    /// The issue's sample, as the server receives it: the caret sits under
    /// the `h` of `host`, the byte the parser reports.
    #[test]
    fn the_sample_caret_sits_under_the_h_of_host() {
        let errors = trawl_core::parser::parse(SAMPLE).expect_err("the sample does not parse");
        assert_eq!(errors[0].span.start, 43);
        assert!(errors[0].message.starts_with("found 'h', expected"));

        let at = locate(SAMPLE, &span(43, 44)).expect("the span indexes the text");
        assert_eq!(at.line, 1);
        assert_eq!(at.column, 44);
        assert!(!at.multi_line);
        assert_eq!(at.line_text, SAMPLE);
        assert_eq!(at.before, &SAMPLE[..43]);
        assert_eq!(at.width, 1);
    }

    /// Columns count characters, not bytes: `é` is two bytes and `🌊`
    /// four, and each is one column.
    #[test]
    fn columns_count_characters_not_bytes() {
        let text = "a=é b=🌊 x";
        let x = text.find('x').expect("x is in the text");
        assert_eq!(x, 12);
        let at = locate(text, &span(x, x + 1)).expect("x is on a boundary");
        assert_eq!(at.column, 9);
        assert_eq!(at.before, "a=é b=🌊 ");

        let wave = text.find('🌊').expect("the wave is in the text");
        let at = locate(text, &span(wave, wave + '🌊'.len_utf8())).expect("whole glyph");
        assert_eq!(at.column, 7);
        assert_eq!(at.before, "a=é b=");
        assert_eq!(at.width, 1);
    }

    /// A wide glyph before the span is one column but two cells on
    /// screen. The caret line pads with the glyph itself, so the browser
    /// gives the pad the width it gave the line above.
    #[test]
    fn a_wide_glyph_before_the_span_pads_with_itself() {
        let text = "last=15m service=雪 | stats count( by host";
        let h = text.rfind('h').expect("host is in the text");
        let excerpt = Excerpt::at(text, &span(h, h + 1)).expect("in bounds");
        assert_eq!(excerpt.pad, "last=15m service=雪 | stats count( by ");
        assert_eq!(excerpt.caret, "^");
        assert_eq!(locate(text, &span(h, h + 1)).expect("in bounds").column, 38);
    }

    /// A combining mark is a column of its own to `locate`, and no width
    /// at all on screen; the pad carries it on its base letter.
    #[test]
    fn a_combining_mark_before_the_span_pads_with_itself() {
        let text = "a=e\u{301} x";
        let x = text.find('x').expect("x is in the text");
        let at = locate(text, &span(x, x + 1)).expect("in bounds");
        assert_eq!(at.column, 6);
        let excerpt = Excerpt::at(text, &span(x, x + 1)).expect("in bounds");
        assert_eq!(excerpt.pad, "a=e\u{301} ");
        assert_eq!(excerpt.caret, "^");
    }

    #[test]
    fn a_span_that_cannot_index_the_text_is_none() {
        let text = "a=é b";
        // Past the end.
        assert_eq!(locate(text, &span(0, text.len() + 1)), None);
        assert_eq!(locate(text, &span(text.len() + 1, text.len() + 1)), None);
        // Reversed.
        assert_eq!(locate(text, &span(3, 2)), None);
        // Inside `é` (bytes 2..4), at either end.
        assert_eq!(locate(text, &span(3, 4)), None);
        assert_eq!(locate(text, &span(2, 3)), None);
    }

    #[test]
    fn a_zero_width_span_gets_one_caret() {
        let at = locate("abc def", &span(4, 4)).expect("in bounds");
        assert_eq!(at.before, "abc ");
        assert_eq!(at.width, 1);
        assert_eq!(at.column, 5);
    }

    #[test]
    fn a_span_at_the_end_of_the_text_sits_after_the_last_character() {
        let text = "service=kubelet | stats count(";
        let at = locate(text, &span(text.len(), text.len())).expect("EOF is in bounds");
        assert_eq!(at.before, text);
        assert_eq!(at.width, 1);
        assert_eq!(at.line_text, text);
    }

    /// A multi-line text shows the one line holding the span and names
    /// it; `\r\n` is one line ending and never reaches the excerpt.
    #[test]
    fn a_multi_line_text_shows_only_the_spanned_line_with_its_number() {
        let text = "service=kubelet\r\n| stats count( by host\r\n| head 5";
        let h = text.find("host").expect("host is in the text");
        let at = locate(text, &span(h, h + 4)).expect("in bounds");
        assert_eq!(at.line, 2);
        assert!(at.multi_line);
        assert_eq!(at.line_text, "| stats count( by host");
        assert_eq!(at.before, "| stats count( by ");
        assert_eq!(at.width, 4);

        let excerpt = Excerpt::at(text, &span(h, h + 4)).expect("in bounds");
        assert_eq!(excerpt.prefix, "line 2: ");
        assert_eq!(excerpt.text, "| stats count( by host");
        assert_eq!(excerpt.pad, "line 2: | stats count( by ");
        assert_eq!(excerpt.caret, "^^^^");

        // The first line of a multi-line text is named too, and loses its
        // `\r`; a span running into the line ending is cut at the line.
        let first = Excerpt::at(text, &span(8, 17)).expect("in bounds");
        assert_eq!(first.prefix, "line 1: ");
        assert_eq!(first.text, "service=kubelet");
        assert_eq!(first.pad, "line 1: service=");
        assert_eq!(first.caret, "^".repeat(7));
    }

    /// A span that starts on a line ending the excerpt drops sits just
    /// past the displayed line, never a column further for the `\r`.
    #[test]
    fn a_span_on_a_dropped_carriage_return_sits_at_the_end_of_the_displayed_line() {
        // The `\n` of a `\r\n`.
        let at = locate("x\r\nnext", &span(2, 3)).expect("in bounds");
        assert_eq!(at.line, 1);
        assert_eq!(at.line_text, "x");
        assert_eq!(at.column, 2);
        assert_eq!((at.before.as_str(), at.width), ("x", 1));

        // The `\r` itself.
        let at = locate("x\r\nnext", &span(1, 2)).expect("in bounds");
        assert_eq!(at.column, 2);
        assert_eq!((at.before.as_str(), at.width), ("x", 1));

        // A lone trailing `\r`, and the end of the text after it.
        for (start, end) in [(1, 2), (2, 2)] {
            let at = locate("x\r", &span(start, end)).expect("in bounds");
            assert_eq!(at.line_text, "x");
            assert_eq!(at.column, 2, "span {start}..{end}");
            assert_eq!(
                (at.before.as_str(), at.width),
                ("x", 1),
                "span {start}..{end}"
            );
        }
    }

    /// A tab before the span is in the pad as itself, so it reaches the
    /// same tab stop it reached in the line above.
    #[test]
    fn a_tab_before_the_span_pads_with_itself() {
        let text = "a=1\t| x";
        let excerpt = Excerpt::at(text, &span(6, 7)).expect("in bounds");
        assert_eq!(excerpt.pad, "a=1\t| ");
        assert_eq!(excerpt.caret, "^");
    }

    #[test]
    fn a_hint_joins_with_an_em_dash() {
        assert_eq!(join_hint("m", Some("h")), "m — h");
        assert_eq!(join_hint("m", None), "m");
    }

    #[test]
    fn no_detail_reads_the_summary() {
        let model =
            NoticeModel::build(Lead::Query, &[], "timechart on 'x' is not a timestamp", "q");
        assert_eq!(
            model.headline(),
            "Couldn't run the query: timechart on 'x' is not a timestamp"
        );
        assert!(model.blocks.is_empty());

        let model = NoticeModel::build(Lead::LiveSyntax, &[], "boom", "q");
        assert_eq!(
            model.headline(),
            "Couldn't start the live stream. The query has a syntax error: boom"
        );
        assert!(model.blocks.is_empty());
    }

    /// One detail: the headline carries the message, the one block only
    /// the excerpt, so nothing is said twice.
    #[test]
    fn one_detail_heads_the_notice_and_shows_its_excerpt() {
        let details = [detail("found 'h', expected ')'", Some((43, 44)), None)];
        let model = NoticeModel::build(Lead::Query, &details, "summary", SAMPLE);
        assert_eq!(
            model.headline(),
            "Couldn't run the query: found 'h', expected ')'"
        );
        assert_eq!(
            model.blocks,
            vec![Block {
                message: None,
                excerpt: Some(Excerpt {
                    prefix: String::new(),
                    text: SAMPLE.to_owned(),
                    pad: SAMPLE[..43].to_owned(),
                    caret: "^".to_owned(),
                }),
            }]
        );

        let model = NoticeModel::build(Lead::LiveSyntax, &details, "summary", SAMPLE);
        assert_eq!(
            model.headline(),
            "Couldn't start the live stream. The query has a syntax error: found 'h', expected ')'"
        );
        assert_eq!(model.blocks.len(), 1);
    }

    /// The first line splits into the notice's verdict and the server's
    /// words after it, so each can take its own tone; a live count is a
    /// verdict alone.
    #[test]
    fn the_first_line_splits_the_verdict_from_the_message() {
        let details = [detail("found 'h', expected ')'", Some((43, 44)), None)];
        let model = NoticeModel::build(Lead::Query, &details, "summary", SAMPLE);
        assert_eq!(model.verdict, "Couldn't run the query");
        assert_eq!(model.detail.as_deref(), Some("found 'h', expected ')'"));

        let model = NoticeModel::build(Lead::LiveSyntax, &details, "summary", SAMPLE);
        assert_eq!(
            model.verdict,
            "Couldn't start the live stream. The query has a syntax error"
        );
        assert_eq!(model.detail.as_deref(), Some("found 'h', expected ')'"));

        let two = [detail("first", None, None), detail("second", None, None)];
        let model = NoticeModel::build(Lead::Query, &two, "first", SAMPLE);
        assert_eq!(model.verdict, "Couldn't run the query");
        assert_eq!(model.detail.as_deref(), Some("2 errors"));
        let model = NoticeModel::build(Lead::LiveSyntax, &two, "first", SAMPLE);
        assert_eq!(model.detail, None);
    }

    /// A validation error's lone detail has no span: the headline joins
    /// the hint once and there is no block at all.
    #[test]
    fn a_spanless_detail_joins_its_hint_once_and_shows_no_excerpt() {
        let details = [detail(
            "unknown function: countt",
            None,
            Some("did you mean 'count'?"),
        )];
        let model = NoticeModel::build(
            Lead::Query,
            &details,
            "unknown function: countt (did you mean 'count'?)",
            "stats countt(x) by host",
        );
        assert_eq!(
            model.headline(),
            "Couldn't run the query: unknown function: countt — did you mean 'count'?"
        );
        assert_eq!(model.headline().matches("did you mean").count(), 1);
        assert!(model.blocks.is_empty());
    }

    /// Several details: the headline counts them and each gets its own
    /// message and excerpt, in server order. A span that does not index
    /// the sent text keeps its message and loses only the excerpt.
    #[test]
    fn several_details_are_counted_and_listed_in_order() {
        let details = [
            detail("first", Some((0, 4)), Some("try this")),
            detail("second", Some((500, 501)), None),
        ];
        let model = NoticeModel::build(Lead::Query, &details, "first", SAMPLE);
        assert_eq!(model.headline(), "Couldn't run the query: 2 errors");
        assert_eq!(model.blocks.len(), 2);
        assert_eq!(model.blocks[0].message.as_deref(), Some("first — try this"));
        assert_eq!(
            model.blocks[0]
                .excerpt
                .as_ref()
                .map(|e| (e.pad.as_str(), e.caret.as_str())),
            Some(("", "^^^^"))
        );
        assert_eq!(model.blocks[1].message.as_deref(), Some("second"));
        assert_eq!(model.blocks[1].excerpt, None);

        let model = NoticeModel::build(Lead::LiveSyntax, &details, "first", SAMPLE);
        assert_eq!(
            model.headline(),
            "Couldn't start the live stream. The query has 2 syntax errors."
        );
        assert_eq!(model.blocks.len(), 2);
    }

    /// The parser's cap note is part of the last message, and the notice
    /// renders it exactly as the server wrote it.
    #[test]
    fn the_omitted_errors_note_renders_verbatim() {
        let details = [
            detail("found '#', expected a value", Some((2, 3)), None),
            detail(
                "unknown command 'bogus' (3 earlier errors omitted)",
                Some((10, 15)),
                None,
            ),
        ];
        let model = NoticeModel::build(Lead::Query, &details, "x", SAMPLE);
        assert_eq!(
            model.blocks[1].message.as_deref(),
            Some("unknown command 'bogus' (3 earlier errors omitted)")
        );
    }

    #[test]
    fn local_details_keep_every_field_of_the_parse_error() {
        let errors = trawl_core::parser::parse(SAMPLE).expect_err("the sample does not parse");
        let details = local_details(&errors);
        assert_eq!(details.len(), errors.len());
        for (d, e) in details.iter().zip(&errors) {
            assert_eq!(d.message, e.message);
            let s = d.span.as_ref().expect("a local error always has a span");
            assert_eq!((s.start, s.end), (e.span.start, e.span.end));
            assert_eq!(d.label, e.label);
            assert_eq!(d.hint, e.hint);
        }
    }

    /// The draft as the editor holds it, with no range prepended: the
    /// `h` of `host` is column 35.
    #[test]
    fn the_draft_diagnostic_names_line_and_column() {
        let draft = "service=kubelet | stats count( by host";
        let diagnostic = draft_diagnostic(draft).expect("the draft does not parse");
        assert!(
            diagnostic
                .first
                .starts_with("Line 1:35 — found 'h', expected"),
            "{}",
            diagnostic.first
        );
        assert_eq!(
            draft_diagnostic("service=kubelet | stats count() by host"),
            None
        );
    }

    /// An ordered operator with a glob is a parse error whose span opens at
    /// the operator: the `<` of `host<a*` is column 5.
    #[test]
    fn the_draft_diagnostic_marks_an_ordered_glob() {
        let diagnostic = draft_diagnostic("host<a*").expect("the draft does not parse");
        assert!(
            diagnostic
                .first
                .starts_with("Line 1:5 — glob patterns only support `=` and `!=`"),
            "{}",
            diagnostic.first
        );
        assert!(diagnostic.rest.is_empty(), "{:?}", diagnostic.rest);
        assert_eq!(draft_diagnostic("host<\"a*\""), None);
    }

    #[test]
    fn the_draft_diagnostic_keeps_further_errors_for_the_disclosure() {
        let draft = "f=#a,#b";
        let errors = trawl_core::parser::parse(draft).expect_err("two bad elements");
        assert!(errors.len() >= 2, "{errors:?}");
        let diagnostic = draft_diagnostic(draft).expect("the draft does not parse");
        assert_eq!(diagnostic.rest.len(), errors.len() - 1);
        assert!(diagnostic.first.starts_with("Line 1:"));
        assert!(diagnostic.rest.iter().all(|r| r.starts_with("Line 1:")));
    }

    /// A draft that parses but reads like a field filter gets advice, not
    /// an error: the advice kind, the core's copy verbatim, located like an
    /// error.
    #[test]
    fn draft_advice_uses_the_advice_kind() {
        let draft = "\"10.244.0.83\" -service=coredns";
        let diagnostic = draft_diagnostic(draft).expect("the draft has advice");
        assert_eq!(diagnostic.kind, DraftKind::Advice);
        assert_eq!(diagnostic.omitted, 0);
        assert!(diagnostic.rest.is_empty(), "{:?}", diagnostic.rest);
        let advice = trawl_core::advice::advise(&trawl_core::parser::parse(draft).expect("parses"));
        assert_eq!(advice.findings[0].span, 14..30);
        assert_eq!(
            diagnostic.first,
            format!(
                "Line 1:15 — {} — {}",
                advice.findings[0].message, advice.findings[0].hint
            )
        );
        assert!(
            diagnostic.first.contains("-service=… is a text search"),
            "{}",
            diagnostic.first
        );

        let in_draft = "service=trawld event_type in (a, b)";
        let diagnostic = draft_diagnostic(in_draft).expect("the draft has advice");
        assert_eq!(diagnostic.kind, DraftKind::Advice);
        assert!(
            diagnostic
                .first
                .starts_with("Line 1:16 — in the search stage, in (…) is not a list test"),
            "{}",
            diagnostic.first
        );

        assert_eq!(draft_diagnostic("\"10.244.0.83\" service!=coredns"), None);
    }

    /// A draft with an error shows only its errors, never advice, even when
    /// the rest of it holds an advice shape.
    #[test]
    fn draft_errors_suppress_advice() {
        let draft = "-service=coredns | where m matches \"^\\s+at \"";
        let diagnostic = draft_diagnostic(draft).expect("the draft does not parse");
        assert_eq!(diagnostic.kind, DraftKind::Error);
        assert_eq!(diagnostic.omitted, 0);
        assert!(
            diagnostic.first.contains("unknown escape \\s"),
            "{}",
            diagnostic.first
        );
        let all = std::iter::once(&diagnostic.first).chain(&diagnostic.rest);
        for line in all {
            assert!(!line.contains("text search, not a field filter"), "{line}");
        }
    }

    /// Advice past the core's cap is counted, not listed: the draft line
    /// holds the retained findings and says how many it left out.
    #[test]
    fn draft_advice_reports_omitted_findings() {
        let draft = (1..=10)
            .map(|i| format!("-service=s{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let diagnostic = draft_diagnostic(&draft).expect("the draft has advice");
        assert_eq!(diagnostic.kind, DraftKind::Advice);
        assert_eq!(
            1 + diagnostic.rest.len(),
            trawl_core::advice::MAX_ADVICE_FINDINGS
        );
        assert_eq!(diagnostic.omitted, 2);
        assert_eq!(
            omitted_advice_note(diagnostic.omitted),
            "and 2 more advice notes not shown"
        );
        assert_eq!(omitted_advice_note(1), "and 1 more advice note not shown");
    }

    fn envelope(code: trawl_api::ErrorCode, details: Vec<ErrorDetail>) -> trawl_api::ErrorEnvelope {
        trawl_api::ErrorEnvelope {
            code,
            message: "the summary".to_owned(),
            details,
            conflict: None,
        }
    }

    /// A refusal quotes the text the request sent, whatever the caller
    /// holds now: the excerpt is built against `sent` alone.
    #[test]
    fn a_refusal_quotes_the_text_it_was_sent_with() {
        let error = ApiError::Query {
            status: 400,
            envelope: Box::new(envelope(
                trawl_api::ErrorCode::ParseError,
                vec![detail("found 'h', expected '('", Some((43, 44)), None)],
            )),
        };
        let model = refusal_notice(&error, SAMPLE).expect("a parse error is a query error");
        assert_eq!(
            model.headline(),
            "Couldn't run the query: found 'h', expected '('"
        );
        let excerpt = model.blocks[0]
            .excerpt
            .as_ref()
            .expect("the span indexes SAMPLE");
        assert_eq!(excerpt.text, SAMPLE);

        // The same failure read against a shorter, newer draft cannot
        // place its caret, and drops the excerpt rather than misplace it.
        let model = refusal_notice(&error, "service=kubelet").expect("still a query error");
        assert!(model.blocks.is_empty());
    }

    /// Every failure that is not a query error keeps the generic copy
    /// and its Retry, so it builds no notice.
    #[test]
    fn only_a_query_error_builds_a_refusal_notice() {
        for error in [
            ApiError::Status(500),
            ApiError::Unauthorized { envelope: None },
            ApiError::Network("offline".to_owned()),
            ApiError::Server {
                status: 500,
                envelope: Box::new(envelope(trawl_api::ErrorCode::ExecutionError, vec![])),
            },
            ApiError::Recovering {
                status: 503,
                envelope: Box::new(envelope(trawl_api::ErrorCode::CorpusRecovering, vec![])),
            },
            ApiError::ResourceExhausted {
                status: 503,
                envelope: Box::new(envelope(
                    trawl_api::ErrorCode::QueryResourceExhausted,
                    vec![],
                )),
            },
        ] {
            assert_eq!(refusal_notice(&error, SAMPLE), None, "{error}");
        }
    }

    /// The issue's sample, sent as a live stream, reads as the live lead
    /// with the caret under the `h` of `host` in the sent text.
    #[test]
    fn a_stream_text_that_does_not_parse_builds_the_syntax_notice() {
        let model = live_syntax_notice(SAMPLE).expect("the sample does not parse");
        assert!(
            model.headline().starts_with(
                "Couldn't start the live stream. The query has a syntax error: found 'h', expected"
            ),
            "{}",
            model.headline()
        );
        let excerpt = model.blocks[0]
            .excerpt
            .as_ref()
            .expect("the span indexes SAMPLE");
        assert_eq!(excerpt.text, SAMPLE);
        assert_eq!(excerpt.pad, &SAMPLE[..43]);
        assert_eq!(excerpt.caret, "^");
    }

    #[test]
    fn a_stream_text_with_several_errors_counts_them() {
        let sent = "f=#a,#b";
        let errors = trawl_core::parser::parse(sent).expect_err("two bad elements");
        let model = live_syntax_notice(sent).expect("the text does not parse");
        assert_eq!(
            model.headline(),
            format!(
                "Couldn't start the live stream. The query has {} syntax errors.",
                errors.len()
            )
        );
        assert_eq!(model.blocks.len(), errors.len());
    }

    /// A text that parses says nothing about the query: the stream's
    /// generic copy and its Retry stand.
    #[test]
    fn a_stream_text_that_parses_builds_no_notice() {
        assert_eq!(
            live_syntax_notice("service=kubelet | stats count() by host"),
            None
        );
    }
}
