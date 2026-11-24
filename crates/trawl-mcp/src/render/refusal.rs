// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Refused calls as error text, and caret excerpts (ADR-0058).
//!
//! A refused tool call has no `structuredContent`: the tool's output schema
//! describes success. A transport failure or a response that did not parse
//! is named by its class only: no stack trace, no internal path, no library
//! reason.

use rmcp::model::{CallToolResult, ContentBlock};
use trawl_client::{ClientError, ErrorDetail, ErrorSpan, NetworkKind};
use trawl_core::sanitize::is_unsafe_display_char;
use trawl_core::text_layout::locate_span;
use unicode_width::UnicodeWidthStr;

use super::one_line;
use super::rows::floor_char_boundary;
use crate::reply::Diagnostic;

pub const MAX_ERROR_BYTES: usize = 8 * 1024;

const MAX_DIAGNOSTICS: usize = 8;

pub const EXCERPT_CONTEXT_BYTES: usize = 60;

const CUT_MARKER: &str = "\n…[error text cut at 8192 bytes]";

#[must_use]
pub fn error_result(text: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(cap(text))])
}

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

#[must_use]
pub fn invalid_arguments(tool: &str, error: &serde_json::Error) -> CallToolResult {
    error_result(format!(
        "invalid arguments for {tool}: {}",
        one_line(&error.to_string())
    ))
}

#[must_use]
pub fn refusal(tool: &str, error: &ClientError, sent: Option<&str>) -> CallToolResult {
    error_result(refusal_text(tool, error, sent))
}

#[must_use]
pub fn refusal_text(tool: &str, error: &ClientError, sent: Option<&str>) -> String {
    refusal_text_with(tool, error, sent, &[])
}

#[must_use]
pub fn refusal_with(
    tool: &str,
    error: &ClientError,
    sent: Option<&str>,
    advice: &[String],
) -> CallToolResult {
    error_result(refusal_text_with(tool, error, sent, advice))
}

#[must_use]
pub fn refusal_text_with(
    tool: &str,
    error: &ClientError,
    sent: Option<&str>,
    advice: &[String],
) -> String {
    let (mut head, trailer) = refusal_lines(tool, error, sent);
    head.extend(advice.iter().map(|line| one_line(line)));
    cap_between("", &head.join("\n"), &trailer.join("\n"))
}

#[must_use]
pub fn refusal_after(
    lead: &str,
    tool: &str,
    error: &ClientError,
    kept: &[String],
) -> CallToolResult {
    let (head, trailer) = refusal_lines(tool, error, None);
    let tail: Vec<String> = kept
        .iter()
        .map(|line| one_line(line))
        .chain(trailer)
        .collect();
    error_result(cap_between(lead, &head.join("\n"), &tail.join("\n")))
}

#[must_use]
pub fn error_after(lead: &str, text: &str) -> CallToolResult {
    error_result(cap_between(lead, text, ""))
}

fn refusal_lines(
    tool: &str,
    error: &ClientError,
    sent: Option<&str>,
) -> (Vec<String>, Vec<String>) {
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
    if let Some(id) = error.request_id() {
        trailer.push(format!("request id: {}", one_line(id)));
    }
    (head, trailer)
}

/// A lead and tail always leave room for a head: the longest lead names
/// two validated ids, and the longest tail a validated result id, a `u32`
/// wait and a request id trawl-client reads only up to
/// [`trawl_client::MAX_REQUEST_ID_BYTES`], a few hundred bytes in all.
fn cap_between(lead: &str, head: &str, tail: &str) -> String {
    let joined = |head: &str| {
        [lead, head, tail]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let whole = joined(head);
    if whole.len() <= MAX_ERROR_BYTES {
        return whole;
    }
    let fixed: usize = [lead, tail]
        .into_iter()
        .filter(|part| !part.is_empty())
        .map(|part| part.len() + 1)
        .sum();
    match MAX_ERROR_BYTES.checked_sub(fixed + CUT_MARKER.len()) {
        Some(room) if room > 0 => {
            let kept = &head[..floor_char_boundary(head, room)];
            joined(&format!("{kept}{CUT_MARKER}"))
        }
        _ => {
            let room = MAX_ERROR_BYTES.saturating_sub(tail.len() + 1 + CUT_MARKER.len());
            let kept = &lead[..floor_char_boundary(lead, room)];
            let marker = if kept.is_empty() {
                CUT_MARKER.trim_start_matches('\n')
            } else {
                CUT_MARKER
            };
            cap(format!("{kept}{marker}\n{tail}"))
        }
    }
}

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

/// Any character [`is_unsafe_display_char`] names but the tab prints as
/// U+FFFD, so the excerpt stays on its two lines and cannot reorder or hide
/// the text around it.
#[must_use]
pub fn excerpt(sent: &str, span: &ErrorSpan) -> Option<String> {
    let at = locate_span(sent, span.start, span.end)?;
    let (line_start, displayed_end) = (at.line_start, at.line_end);
    let (start_on_line, covered_end) = (at.start, at.end);

    let window_start =
        ceil_char_boundary(sent, start_on_line.saturating_sub(EXCERPT_CONTEXT_BYTES))
            .max(line_start);
    let covered_cut = floor_char_boundary(sent, start_on_line + EXCERPT_CONTEXT_BYTES);
    let shown_covered_end = covered_end.min(covered_cut);
    let after_end =
        floor_char_boundary(sent, shown_covered_end + EXCERPT_CONTEXT_BYTES).min(displayed_end);

    let prefix = if at.multi_line {
        format!("line {}: ", at.line)
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
/// between tabs is measured as a string, so a variation selector takes the columns a
/// terminal gives the whole cluster.
fn padding(text: &str) -> String {
    text.split('\t')
        .map(|segment| " ".repeat(segment.width()))
        .collect::<Vec<_>>()
        .join("\t")
}

/// The tab stays because [`padding`] and the caret count line up on it. The
/// replacement takes one column, so a character that took none before the
/// span still moves the caret by the column its replacement prints in.
fn displayed(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c != '\t' && is_unsafe_display_char(c) {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut at = index.min(text.len());
    while !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

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
    fn a_lead_that_leaves_no_room_is_cut_before_the_tail() {
        let tail = "The result was captured.\nretry after: 7 seconds\nrequest id: req-1";
        for lead in [
            "l".repeat(MAX_ERROR_BYTES),
            "日".repeat(MAX_ERROR_BYTES / 3),
        ] {
            let text = cap_between(&lead, "trawld says: busy", tail);
            assert!(text.len() <= MAX_ERROR_BYTES, "{}", text.len());
            assert!(
                text.ends_with(&format!("{CUT_MARKER}\n{tail}")),
                "{}",
                &text[text.len() - 120..]
            );
            let kept = &text[..text.len() - CUT_MARKER.len() - 1 - tail.len()];
            assert!(lead.starts_with(kept) && !kept.is_empty(), "{kept:.40}");
        }
    }

    #[test]
    fn a_cut_text_stays_within_the_cap_on_a_char_boundary() {
        let text = cap("日".repeat(5000));
        assert!(text.len() <= MAX_ERROR_BYTES);
        assert!(text.ends_with(CUT_MARKER));
    }
}
