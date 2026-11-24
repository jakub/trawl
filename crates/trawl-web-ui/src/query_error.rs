// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a query error says, and where it points (ADR-0039).
//!
//! Spans are UTF-8 byte offsets; columns are character counts.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::{ErrorDetail, ErrorSpan};
use trawl_core::text_layout::locate_span;

use crate::api_error::ApiError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// 1-based line number.
    pub line: usize,
    /// 1-based character column of the span's start on that line.
    pub column: usize,
    pub multi_line: bool,
    pub line_text: String,
    pub before: String,
    pub width: usize,
}

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

#[must_use]
pub fn join_hint(message: &str, hint: Option<&str>) -> String {
    match hint {
        Some(hint) => format!("{message} — {hint}"),
        None => message.to_owned(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lead {
    Query,
    LiveSyntax,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excerpt {
    pub prefix: String,
    pub text: String,
    /// The prefix and the quoted line up to the span, rendered hidden so the
    /// carets start where the span's first glyph was laid out above them.
    pub pad: String,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// The detail's message with its hint. `None` for a lone detail,
    /// whose message is already the headline.
    pub message: Option<String>,
    pub excerpt: Option<Excerpt>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeModel {
    pub verdict: String,
    /// What the first line says after the verdict and a `: `: the
    /// server's message or the error count. The notice shows a lone detail's
    /// message under its excerpt instead, when it has one.
    pub detail: Option<String>,
    pub blocks: Vec<Block>,
}

impl NoticeModel {
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

    #[cfg(test)]
    fn headline(&self) -> String {
        match &self.detail {
            Some(detail) => format!("{}: {detail}", self.verdict),
            None => self.verdict.clone(),
        }
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftKind {
    Error,
    Advice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftDiagnostic {
    pub kind: DraftKind,
    pub first: String,
    pub rest: Vec<String>,
    pub omitted: usize,
}

#[must_use]
pub fn omitted_advice_note(omitted: usize) -> String {
    if omitted == 1 {
        "and 1 more advice note not shown".to_owned()
    } else {
        format!("and {omitted} more advice notes not shown")
    }
}

fn draft_line(doc: &str, start: usize, end: usize, message: &str, hint: Option<&str>) -> String {
    let message = join_hint(message, hint);
    match locate(doc, &ErrorSpan { start, end }) {
        Some(at) => format!("Line {}:{} — {message}", at.line, at.column),
        None => message,
    }
}

/// Empty when trawl-core's semantic checks refuse the pipeline. The browser
/// marks parse errors alone and leaves the semantic error to the server's
/// verdict (ADR-0039), so this only withholds the advice.
#[must_use]
pub fn query_advice(query: &trawl_core::ast::Query) -> trawl_core::advice::AdviceReport {
    if trawl_core::emitter::validate_pipeline(&query.pipeline).is_err() {
        return trawl_core::advice::AdviceReport::default();
    }
    trawl_core::advice::advise(query)
}

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
            let report = query_advice(&query);
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

    #[test]
    fn a_wide_glyph_before_the_span_pads_with_itself() {
        let text = "last=15m service=雪 | stats count( by host";
        let h = text.rfind('h').expect("host is in the text");
        let excerpt = Excerpt::at(text, &span(h, h + 1)).expect("in bounds");
        assert_eq!(excerpt.pad, "last=15m service=雪 | stats count( by ");
        assert_eq!(excerpt.caret, "^");
        assert_eq!(locate(text, &span(h, h + 1)).expect("in bounds").column, 38);
    }

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
        assert_eq!(locate(text, &span(0, text.len() + 1)), None);
        assert_eq!(locate(text, &span(text.len() + 1, text.len() + 1)), None);
        assert_eq!(locate(text, &span(3, 2)), None);
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

        let first = Excerpt::at(text, &span(8, 17)).expect("in bounds");
        assert_eq!(first.prefix, "line 1: ");
        assert_eq!(first.text, "service=kubelet");
        assert_eq!(first.pad, "line 1: service=");
        assert_eq!(first.caret, "^".repeat(7));
    }

    #[test]
    fn a_span_on_a_dropped_carriage_return_sits_at_the_end_of_the_displayed_line() {
        let at = locate("x\r\nnext", &span(2, 3)).expect("in bounds");
        assert_eq!(at.line, 1);
        assert_eq!(at.line_text, "x");
        assert_eq!(at.column, 2);
        assert_eq!((at.before.as_str(), at.width), ("x", 1));

        let at = locate("x\r\nnext", &span(1, 2)).expect("in bounds");
        assert_eq!(at.column, 2);
        assert_eq!((at.before.as_str(), at.width), ("x", 1));

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

    #[test]
    fn draft_semantic_errors_suppress_advice() {
        let draft = "-service=x | stats nosuchfn(x)";
        assert!(trawl_core::parser::parse(draft).is_ok(), "the draft parses");
        assert_eq!(draft_diagnostic(draft), None);
        assert!(
            query_advice(&trawl_core::parser::parse(draft).unwrap())
                .findings
                .is_empty()
        );
    }

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

        let model = refusal_notice(&error, "service=kubelet").expect("still a query error");
        assert!(model.blocks.is_empty());
    }

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

    #[test]
    fn a_stream_text_that_parses_builds_no_notice() {
        assert_eq!(
            live_syntax_notice("service=kubelet | stats count() by host"),
            None
        );
    }
}
