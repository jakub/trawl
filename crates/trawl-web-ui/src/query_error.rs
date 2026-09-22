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
//! character counts, which is what lines a caret up under a monospace
//! glyph for everything but wide and combining glyphs.
//!
//! Only the wasm32 build consumes these helpers outside the tests.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::{ErrorDetail, ErrorSpan};

/// A span resolved against the text it indexes: the one line that holds
/// its start, and where on that line it sits.
#[cfg_attr(
    target_arch = "wasm32",
    expect(
        dead_code,
        reason = "the search page's notice wiring (#233) is its first reader; drop this then"
    )
)]
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
    /// Blank padding up to the span's start, then one `^` per character
    /// the span covers on that line (at least one). A tab in the line
    /// before the span pads with a tab, so the caret lands where the
    /// line's own tab stop put the text above it.
    pub caret: String,
}

/// Resolve a byte span against `text`.
///
/// `None` when the span cannot index this text: reversed, past the end,
/// or off a character boundary at either end. The caller then keeps the
/// message and drops the excerpt; a caret under the wrong character is
/// worse than none.
///
/// A zero-width span, including one at the end of the text, gets one
/// caret where it sits. A span running past the end of its line is cut
/// there. `\r\n` is one line ending.
#[cfg_attr(
    target_arch = "wasm32",
    expect(
        dead_code,
        reason = "the search page's notice wiring (#233) is its first reader; drop this then"
    )
)]
#[must_use]
pub fn locate(text: &str, span: &ErrorSpan) -> Option<Located> {
    let (start, end) = (span.start, span.end);
    if start > end
        || end > text.len()
        || !text.is_char_boundary(start)
        || !text.is_char_boundary(end)
    {
        return None;
    }

    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[start..].find('\n').map_or(text.len(), |i| start + i);
    let line = text[..line_start].matches('\n').count() + 1;
    let line_text = text[line_start..line_end]
        .strip_suffix('\r')
        .unwrap_or(&text[line_start..line_end]);

    let before = &text[line_start..start];
    let covered_end = end.min(line_start + line_text.len()).max(start);
    let width = text[start..covered_end].chars().count().max(1);

    let mut caret: String = before
        .chars()
        .map(|c| if c == '\t' { '\t' } else { ' ' })
        .collect();
    caret.extend(std::iter::repeat_n('^', width));

    Some(Located {
        line,
        column: before.chars().count() + 1,
        multi_line: text.contains('\n'),
        line_text: line_text.to_owned(),
        caret,
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
#[cfg_attr(
    target_arch = "wasm32",
    expect(
        dead_code,
        reason = "the search page's notice wiring (#233) is its first reader; drop this then"
    )
)]
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
    /// The caret line, padded past the prefix so it sits under `text`.
    pub caret: String,
}

impl Excerpt {
    #[cfg_attr(
        target_arch = "wasm32",
        expect(
            dead_code,
            reason = "the search page's notice wiring (#233) is its first reader; drop this then"
        )
    )]
    fn at(sent: &str, span: &ErrorSpan) -> Option<Self> {
        let located = locate(sent, span)?;
        let prefix = if located.multi_line {
            format!("line {}: ", located.line)
        } else {
            String::new()
        };
        let caret = " ".repeat(prefix.chars().count()) + &located.caret;
        Some(Self {
            prefix,
            text: located.line_text,
            caret,
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
    /// The first line.
    pub headline: String,
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
    #[cfg_attr(
        target_arch = "wasm32",
        expect(
            dead_code,
            reason = "the search page's notice wiring (#233) is its first reader; drop this then"
        )
    )]
    #[must_use]
    pub fn build(lead: Lead, details: &[ErrorDetail], summary: &str, sent: &str) -> Self {
        let excerpt = |d: &ErrorDetail| d.span.as_ref().and_then(|s| Excerpt::at(sent, s));
        let joined = |d: &ErrorDetail| join_hint(&d.message, d.hint.as_deref());
        match details {
            [] => Self {
                headline: single(lead, summary),
                blocks: Vec::new(),
            },
            [only] => Self {
                headline: single(lead, &joined(only)),
                blocks: excerpt(only)
                    .map(|e| Block {
                        message: None,
                        excerpt: Some(e),
                    })
                    .into_iter()
                    .collect(),
            },
            many => Self {
                headline: match lead {
                    Lead::Query => format!("Couldn't run the query: {} errors", many.len()),
                    Lead::LiveSyntax => format!(
                        "Couldn't start the live stream. The query has {} syntax errors.",
                        many.len()
                    ),
                },
                blocks: many
                    .iter()
                    .map(|d| Block {
                        message: Some(joined(d)),
                        excerpt: excerpt(d),
                    })
                    .collect(),
            },
        }
    }
}

/// The headline for one message.
#[cfg_attr(
    target_arch = "wasm32",
    expect(
        dead_code,
        reason = "the search page's notice wiring (#233) is its first reader; drop this then"
    )
)]
fn single(lead: Lead, message: &str) -> String {
    match lead {
        Lead::Query => format!("Couldn't run the query: {message}"),
        Lead::LiveSyntax => {
            format!("Couldn't start the live stream. The query has a syntax error: {message}")
        }
    }
}

/// The local parser's errors in the wire's detail shape, so the live
/// syntax notice renders through the same [`NoticeModel`] as a server
/// refusal.
#[cfg_attr(
    target_arch = "wasm32",
    expect(
        dead_code,
        reason = "live mode's syntax notice (#233) is its first reader; drop this then"
    )
)]
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

/// The draft diagnostic: the local parser's verdict on the draft, as
/// visible text under the editor.
#[cfg_attr(
    target_arch = "wasm32",
    expect(
        dead_code,
        reason = "the editor's draft diagnostic line (#233) is its first reader; drop this then"
    )
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftDiagnostic {
    /// The first error, in full.
    pub first: String,
    /// Every further error, behind the `+N more` disclosure.
    pub rest: Vec<String>,
}

/// Parse `doc` locally and describe each error as `Line L:C — message`,
/// with the hint joined on. `None` when the draft parses.
#[cfg_attr(
    target_arch = "wasm32",
    expect(
        dead_code,
        reason = "the editor's draft diagnostic line (#233) is its first reader; drop this then"
    )
)]
#[must_use]
pub fn draft_diagnostic(doc: &str) -> Option<DraftDiagnostic> {
    let errors = trawl_core::parser::parse(doc).err()?;
    let mut lines = errors.iter().map(|e| {
        let message = join_hint(&e.message, e.hint.as_deref());
        let span = ErrorSpan {
            start: e.span.start,
            end: e.span.end,
        };
        match locate(doc, &span) {
            Some(at) => format!("Line {}:{} — {message}", at.line, at.column),
            None => message,
        }
    });
    let first = lines.next()?;
    Some(DraftDiagnostic {
        first,
        rest: lines.collect(),
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
        assert_eq!(at.caret, format!("{}^", " ".repeat(43)));
        assert_eq!(&SAMPLE[at.caret.len() - 1..=43], "h");
    }

    /// Columns count characters, not bytes: `é` is two bytes and `🌊`
    /// four, and each pads the caret by one.
    #[test]
    fn columns_count_characters_not_bytes() {
        let text = "a=é b=🌊 x";
        let x = text.find('x').expect("x is in the text");
        assert_eq!(x, 12);
        let at = locate(text, &span(x, x + 1)).expect("x is on a boundary");
        assert_eq!(at.column, 9);
        assert_eq!(at.caret, format!("{}^", " ".repeat(8)));

        let wave = text.find('🌊').expect("the wave is in the text");
        let at = locate(text, &span(wave, wave + '🌊'.len_utf8())).expect("whole glyph");
        assert_eq!(at.column, 7);
        assert_eq!(at.caret, format!("{}^", " ".repeat(6)));
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
        assert_eq!(at.caret, "    ^");
        assert_eq!(at.column, 5);
    }

    #[test]
    fn a_span_at_the_end_of_the_text_sits_after_the_last_character() {
        let text = "service=kubelet | stats count(";
        let at = locate(text, &span(text.len(), text.len())).expect("EOF is in bounds");
        assert_eq!(at.caret, format!("{}^", " ".repeat(text.len())));
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
        assert_eq!(at.caret, format!("{}^^^^", " ".repeat(18)));

        let excerpt = Excerpt::at(text, &span(h, h + 4)).expect("in bounds");
        assert_eq!(excerpt.prefix, "line 2: ");
        assert_eq!(excerpt.text, "| stats count( by host");
        assert_eq!(excerpt.caret, format!("{}^^^^", " ".repeat(8 + 18)));

        // The first line of a multi-line text is named too, and loses its
        // `\r`; a span running into the line ending is cut at the line.
        let first = Excerpt::at(text, &span(8, 17)).expect("in bounds");
        assert_eq!(first.prefix, "line 1: ");
        assert_eq!(first.text, "service=kubelet");
        assert_eq!(
            first.caret,
            format!("{}{}", " ".repeat(8 + 8), "^".repeat(7))
        );
    }

    #[test]
    fn a_tab_before_the_span_pads_with_a_tab() {
        let text = "a=1\t| x";
        let at = locate(text, &span(6, 7)).expect("in bounds");
        assert_eq!(at.caret, "   \t  ^");
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
            model.headline,
            "Couldn't run the query: timechart on 'x' is not a timestamp"
        );
        assert!(model.blocks.is_empty());

        let model = NoticeModel::build(Lead::LiveSyntax, &[], "boom", "q");
        assert_eq!(
            model.headline,
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
            model.headline,
            "Couldn't run the query: found 'h', expected ')'"
        );
        assert_eq!(
            model.blocks,
            vec![Block {
                message: None,
                excerpt: Some(Excerpt {
                    prefix: String::new(),
                    text: SAMPLE.to_owned(),
                    caret: format!("{}^", " ".repeat(43)),
                }),
            }]
        );

        let model = NoticeModel::build(Lead::LiveSyntax, &details, "summary", SAMPLE);
        assert_eq!(
            model.headline,
            "Couldn't start the live stream. The query has a syntax error: found 'h', expected ')'"
        );
        assert_eq!(model.blocks.len(), 1);
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
            model.headline,
            "Couldn't run the query: unknown function: countt — did you mean 'count'?"
        );
        assert_eq!(model.headline.matches("did you mean").count(), 1);
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
        assert_eq!(model.headline, "Couldn't run the query: 2 errors");
        assert_eq!(model.blocks.len(), 2);
        assert_eq!(model.blocks[0].message.as_deref(), Some("first — try this"));
        assert_eq!(
            model.blocks[0].excerpt.as_ref().map(|e| e.caret.as_str()),
            Some("^^^^")
        );
        assert_eq!(model.blocks[1].message.as_deref(), Some("second"));
        assert_eq!(model.blocks[1].excerpt, None);

        let model = NoticeModel::build(Lead::LiveSyntax, &details, "first", SAMPLE);
        assert_eq!(
            model.headline,
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
}
