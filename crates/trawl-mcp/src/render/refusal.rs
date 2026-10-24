// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Error text within its cap, and diagnostics as lines (ADR-0058).
//!
//! A tool result that is not a success is `isError` with one text block
//! and no `structuredContent`: the tool's output schema describes success.
//! Every error text is at most [`MAX_ERROR_BYTES`], cut on a character
//! boundary with a marker.

use rmcp::model::{CallToolResult, ContentBlock};

use super::one_line;
use super::rows::floor_char_boundary;
use crate::reply::Diagnostic;

/// The most bytes an error text holds.
pub const MAX_ERROR_BYTES: usize = 8 * 1024;

/// The most diagnostics one error text lists; the rest are counted.
const MAX_DIAGNOSTICS: usize = 8;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_text_stays_within_the_cap_on_a_char_boundary() {
        let text = cap("日".repeat(5000));
        assert!(text.len() <= MAX_ERROR_BYTES);
        assert!(text.ends_with(CUT_MARKER));
    }
}
