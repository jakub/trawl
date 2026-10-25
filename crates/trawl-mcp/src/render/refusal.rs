// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Refused calls as error text, and caret excerpts (ADR-0058).
//!
//! A refused tool call is `isError` with one text block and no
//! `structuredContent`: the tool's output schema describes success. The
//! text names trawld's error code and HTTP status, quotes trawld's message,
//! points a caret excerpt at each diagnostic's span in the text that was
//! sent, gives the hint, the request id an operator can find the failure
//! by, and the `Retry-After` wait. A transport failure or a response that
//! did not parse is named by its class only: no stack trace, no internal
//! path, no library reason.
//!
//! A span is checked before it is used, by the rules of the web UI's
//! error notice (ADR-0039): reversed, past the end of the sent text, or
//! off a character boundary at either end, and the excerpt is dropped
//! while the message stays. A caret under the wrong character is worse
//! than none.
//!
//! Every error text is at most [`MAX_ERROR_BYTES`], cut on a character
//! boundary with a marker.

use rmcp::model::{CallToolResult, ContentBlock};
use trawl_client::{ClientError, ErrorDetail, ErrorSpan, NetworkKind};
use unicode_width::UnicodeWidthStr;

use super::one_line;
use super::rows::floor_char_boundary;
use crate::reply::Diagnostic;

/// The most bytes an error text holds.
pub const MAX_ERROR_BYTES: usize = 8 * 1024;

/// The most diagnostics one error text lists; the rest are counted.
const MAX_DIAGNOSTICS: usize = 8;

/// Bytes of the sent line an excerpt keeps on each side of the span, and
/// of the span itself.
pub const EXCERPT_CONTEXT_BYTES: usize = 60;

/// What a cut error text ends with.
const CUT_MARKER: &str = "\n…[error text cut at 8192 bytes]";

/// An `isError` result holding `text`, capped at [`MAX_ERROR_BYTES`].
#[must_use]
pub fn error_result(text: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(cap(text))])
}

/// `text` cut to at most [`MAX_ERROR_BYTES`] on a character boundary, with
/// a marker saying it was cut.
#[must_use]
pub fn cap(mut text: String) -> String {
    if text.len() <= MAX_ERROR_BYTES {
        return text;
    }
    let keep = floor_char_boundary(&text, MAX_ERROR_BYTES - CUT_MARKER.len());
    text.truncate(keep);
    text.push_str(CUT_MARKER);
    text
}

/// The `isError` result for `tool` arguments that did not decode into the
/// tool's input.
#[must_use]
pub fn invalid_arguments(tool: &str, error: &serde_json::Error) -> CallToolResult {
    error_result(format!(
        "invalid arguments for {tool}: {}",
        one_line(&error.to_string())
    ))
}

/// The `isError` result for a call to `tool` that trawld refused or that
/// failed on the way, with `sent` the query text the call sent, if any.
#[must_use]
pub fn refusal(tool: &str, error: &ClientError, sent: Option<&str>) -> CallToolResult {
    error_result(refusal_text(tool, error, sent))
}

/// The text of [`refusal`], capped at [`MAX_ERROR_BYTES`].
///
/// The `Retry-After` wait and the request id end the text and survive the
/// cap: when the text is too long, the head (the code line, trawld's
/// message and the diagnostics) is cut instead, on a character boundary
/// with the cut marker.
#[must_use]
pub fn refusal_text(tool: &str, error: &ClientError, sent: Option<&str>) -> String {
    refusal_text_with(tool, error, sent, &[])
}

/// The `isError` result of [`refusal`] with `advice`, lines the tool adds
/// after trawld's message: what the refusal means for this call and the
/// call that is the way back.
#[must_use]
pub fn refusal_with(
    tool: &str,
    error: &ClientError,
    sent: Option<&str>,
    advice: &[String],
) -> CallToolResult {
    error_result(refusal_text_with(tool, error, sent, advice))
}

/// [`refusal_text`] with `advice` after trawld's message and diagnostics,
/// or after the failure line when trawld sent none.
#[must_use]
pub fn refusal_text_with(
    tool: &str,
    error: &ClientError,
    sent: Option<&str>,
    advice: &[String],
) -> String {
    let mut head = Vec::new();
    let mut trailer = Vec::new();
    match error {
        ClientError::Server {
            status,
            error: envelope,
            retry_after_secs,
            ..
        } => {
            let code = serde_json::to_value(&envelope.code)
                .ok()
                .and_then(|code| code.as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".to_owned());
            head.push(format!("{tool} was refused: {code} (HTTP {status})."));
            head.push(format!("trawld says: {}", one_line(&envelope.message)));
            let diagnostics = diagnostics(sent.unwrap_or(""), &envelope.details);
            diagnostic_lines(&diagnostics, &mut head);
            if let Some(wait) = retry_after_secs {
                trailer.push(format!("retry after: {wait} {}", seconds(*wait)));
            } else if *status == 429 {
                trailer.push("retry after: trawld named no wait".to_owned());
            }
        }
        ClientError::Network(network) => {
            head.push(format!("{tool} failed: {}.", network_class(network.kind)));
        }
        ClientError::Parse { .. } => {
            head.push(format!("{tool} failed: trawld's response did not parse."));
        }
        ClientError::TooLarge { cap, .. } => {
            head.push(format!(
                "{tool} failed: trawld's response was larger than the {cap}-byte cap."
            ));
        }
        ClientError::InvalidCa(_) => {
            head.push(format!(
                "{tool} failed: the profile's pinned CA bundle holds no usable certificate."
            ));
        }
        ClientError::InvalidUrl(_) => {
            head.push(format!(
                "{tool} failed: the profile's server URL is not usable."
            ));
        }
    }
    head.extend(advice.iter().map(|line| one_line(line)));
    if let Some(id) = error.request_id() {
        trailer.push(format!("request id: {}", one_line(id)));
    }
    cap_head(&head.join("\n"), &trailer.join("\n"))
}

/// `head`, then `trailer` on the lines after it, within
/// [`MAX_ERROR_BYTES`]. When both do not fit, `head` is cut on a character
/// boundary and ends with the cut marker. A trailer that leaves no room for
/// a head (trawld sent a request id of kilobytes) is cut with the rest.
fn cap_head(head: &str, trailer: &str) -> String {
    if trailer.is_empty() {
        return cap(head.to_owned());
    }
    let whole = format!("{head}\n{trailer}");
    if whole.len() <= MAX_ERROR_BYTES {
        return whole;
    }
    match MAX_ERROR_BYTES.checked_sub(trailer.len() + 1 + CUT_MARKER.len()) {
        Some(room) if room > 0 => {
            let kept = &head[..floor_char_boundary(head, room)];
            format!("{kept}{CUT_MARKER}\n{trailer}")
        }
        _ => cap(whole),
    }
}

/// The diagnostics of `details`, each with a caret excerpt cut from `sent`
/// when its span indexes `sent`.
#[must_use]
pub fn diagnostics(sent: &str, details: &[ErrorDetail]) -> Vec<Diagnostic> {
    details
        .iter()
        .map(|detail| Diagnostic {
            message: detail.message.clone(),
            excerpt: detail.span.as_ref().and_then(|span| excerpt(sent, span)),
            start: detail
                .span
                .as_ref()
                .map(|span| super::rows::as_u64(span.start)),
            end: detail
                .span
                .as_ref()
                .map(|span| super::rows::as_u64(span.end)),
            hint: detail.hint.clone(),
        })
        .collect()
}

/// Print `diagnostics` as `error:` lines with their excerpts and hints.
pub(crate) fn diagnostic_lines(diagnostics: &[Diagnostic], lines: &mut Vec<String>) {
    for diagnostic in diagnostics.iter().take(MAX_DIAGNOSTICS) {
        lines.push(format!("error: {}", one_line(&diagnostic.message)));
        if let Some(excerpt) = &diagnostic.excerpt {
            for line in excerpt.lines() {
                lines.push(format!("  {line}"));
            }
        }
        if let Some(hint) = &diagnostic.hint {
            lines.push(format!("  hint: {}", one_line(hint)));
        }
    }
    if diagnostics.len() > MAX_DIAGNOSTICS {
        lines.push(format!(
            "and {} more errors",
            diagnostics.len() - MAX_DIAGNOSTICS
        ));
    }
}

/// A caret excerpt of `sent` at `span`: the line holding the span's start,
/// then a line whose carets sit under the span.
///
/// `None` when the span cannot index `sent` (see the module docs). The
/// line is cut to [`EXCERPT_CONTEXT_BYTES`] on each side of the span, and
/// the span to as many, on character boundaries, with `…` where it was
/// cut. A multi-line text names the line, `line N: `. The caret column
/// counts display columns as the string width of the text before the span,
/// so wide characters take two, combining marks none, and an emoji
/// sequence the columns of its cluster; a tab stays a tab under itself. A control
/// character or a line or paragraph separator prints as U+FFFD so the
/// excerpt stays on its two lines.
#[must_use]
pub fn excerpt(sent: &str, span: &ErrorSpan) -> Option<String> {
    let (start, end) = (span.start, span.end);
    if start > end
        || end > sent.len()
        || !sent.is_char_boundary(start)
        || !sent.is_char_boundary(end)
    {
        return None;
    }
    let line_start = sent[..start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = sent[start..].find('\n').map_or(sent.len(), |i| start + i);
    let line_number = sent[..line_start].matches('\n').count() + 1;
    let full_line = &sent[line_start..line_end];
    let line = full_line.strip_suffix('\r').unwrap_or(full_line);
    let displayed_end = line_start + line.len();
    let start_on_line = start.min(displayed_end);
    let covered_end = end.min(displayed_end).max(start_on_line);

    // The window, on character boundaries of the sent text.
    let window_start =
        ceil_char_boundary(sent, start_on_line.saturating_sub(EXCERPT_CONTEXT_BYTES))
            .max(line_start);
    let covered_cut = floor_char_boundary(sent, start_on_line + EXCERPT_CONTEXT_BYTES);
    let shown_covered_end = covered_end.min(covered_cut);
    let after_end =
        floor_char_boundary(sent, shown_covered_end + EXCERPT_CONTEXT_BYTES).min(displayed_end);

    let prefix = if sent.contains('\n') {
        format!("line {line_number}: ")
    } else {
        String::new()
    };
    let lead = if window_start > line_start { "…" } else { "" };
    let before = displayed(&sent[window_start..start_on_line]);
    let covered = displayed(&sent[start_on_line..shown_covered_end]);
    let after = displayed(&sent[shown_covered_end..after_end]);
    let trail = if after_end < displayed_end || shown_covered_end < covered_end {
        "…"
    } else {
        ""
    };

    let pad = padding(&format!("{prefix}{lead}{before}"));
    let carets = covered
        .split('\t')
        .map(UnicodeWidthStr::width)
        .sum::<usize>()
        .saturating_add(covered.matches('\t').count())
        .max(1);
    Some(format!(
        "{prefix}{lead}{before}{covered}{after}{trail}\n{pad}{}",
        "^".repeat(carets)
    ))
}

/// Blanks as wide as `text` displays: each tab stays a tab, and each run
/// between tabs is measured as a string, so an emoji ZWJ sequence or a
/// variation selector takes the columns a terminal gives the whole cluster.
fn padding(text: &str) -> String {
    text.split('\t')
        .map(|segment| " ".repeat(segment.width()))
        .collect::<Vec<_>>()
        .join("\t")
}

/// `text` with every control character but the tab, and the Unicode line
/// and paragraph separators, as U+FFFD.
fn displayed(text: &str) -> String {
    text.chars()
        .map(|c| {
            if (c.is_control() && c != '\t') || c == '\u{2028}' || c == '\u{2029}' {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

/// The smallest character boundary of `text` at or above `index`.
fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut at = index.min(text.len());
    while !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// The transport failure's class, as the tail of a sentence.
fn network_class(kind: NetworkKind) -> &'static str {
    match kind {
        NetworkKind::UntrustedCertificate => {
            "trawld's TLS certificate did not verify under the profile's trust"
        }
        NetworkKind::Connect => "no connection to trawld opened",
        NetworkKind::Timeout => "trawld sent no response within the deadline",
        NetworkKind::BodyTimeout => "trawld's response did not finish within the deadline",
        NetworkKind::BodyRead => "trawld's response broke off while it was read",
        NetworkKind::Redirect => "trawld answered with a redirect the client does not follow",
        NetworkKind::UploadCutOff => "trawld closed the connection while the request was sent",
        NetworkKind::Other => "the request to trawld failed in transport",
    }
}

/// `second` or `seconds`.
fn seconds(n: u32) -> &'static str {
    if n == 1 { "second" } else { "seconds" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: usize, end: usize) -> ErrorSpan {
        ErrorSpan { start, end }
    }

    #[test]
    fn an_unusable_span_drops_the_excerpt() {
        let sent = "level=é";
        assert!(excerpt(sent, &span(3, 2)).is_none(), "reversed");
        assert!(excerpt(sent, &span(0, 99)).is_none(), "past the end");
        assert!(excerpt(sent, &span(7, 8)).is_none(), "inside é");
    }

    #[test]
    fn a_zero_width_span_at_the_end_gets_one_caret() {
        assert_eq!(excerpt("level=", &span(6, 6)).unwrap(), "level=\n      ^");
    }

    #[test]
    fn a_multi_line_text_names_the_line() {
        let sent = "service=api\r\n| stat count()";
        let at = sent.find("stat").unwrap();
        assert_eq!(
            excerpt(sent, &span(at, at + 4)).unwrap(),
            "line 2: | stat count()\n          ^^^^"
        );
    }

    #[test]
    fn a_long_line_is_cut_around_the_span() {
        let sent = format!("{} bad {}", "a".repeat(200), "b".repeat(200));
        let at = sent.find("bad").unwrap();
        let text = excerpt(&sent, &span(at, at + 3)).unwrap();
        let (line, carets) = text.split_once('\n').unwrap();
        assert!(line.starts_with('…') && line.ends_with('…'), "{line}");
        assert!(line.len() < 2 * EXCERPT_CONTEXT_BYTES + 20, "{line}");
        assert_eq!(carets.trim_start(), "^^^");
        assert_eq!(&line[line.find("bad").unwrap()..][..3], "bad");
        assert_eq!(
            carets.len() - 3,
            line[..line.find("bad").unwrap()].chars().count()
        );
    }

    #[test]
    fn a_cut_text_stays_within_the_cap_on_a_char_boundary() {
        let text = cap("日".repeat(5000));
        assert!(text.len() <= MAX_ERROR_BYTES);
        assert!(text.ends_with(CUT_MARKER));
    }
}
