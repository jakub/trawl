// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin surface's pure logic: the CLI command the degraded case
//! file offers, and the job-status vocabulary the SPA reads back.
//!
//! Both mirror a server-side authority rather than importing it — the
//! same arrangement `WELL_KNOWN_LOG_FIELDS` has:
//!
//! - [`repin_command_hint`] mirrors `trawl-cli/src/schema.rs`'s
//!   `render_verdict` remedy line, down to the refusal rule, so the two
//!   operator surfaces offer the same command for the same field.
//! - [`repin_is_running`] / [`repin_is_terminal`] mirror the closed
//!   status vocabulary in `crates/trawl-server/src/store/repin.rs`
//!   (`RepinJobStatus`), which is the authority.
//!
//! Pure + ungated so its tests run natively; the callers are wasm32-only.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_core::sanitize::sanitize_display_text;

/// The one non-terminal status. Every other spelling — including a
/// status a future server adds — is terminal, so an unknown value stops
/// polling and renders verbatim rather than spinning forever.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
const STATUS_RUNNING: &str = "running";

/// Why no command is offered for a hostile field name. Mirrors the CLI's
/// sentence, retargeted at the surface the reader is holding.
pub const REPIN_HINT_REFUSED: &str = "This field's name cannot be safely embedded in a command \
     line (it does not survive display sanitisation, or it begins with `-` and would be read as a \
     flag), so no repin command is shown — take the exact name from GET /api/v1/schema/field \
     before running `trawl schema repin`.";

/// The `trawl schema repin … --dry-run` line for a degraded field, or
/// `None` when the field's name cannot be carried by a command line.
///
/// A field name is a client-chosen JSON key that ingest polices for
/// length and case only, so a name carrying a shell command or a bidi
/// override is legal — and this line is written to be pasted into a
/// shell. Two names get no command, matching the CLI exactly:
///
/// - one that does not survive display sanitisation, because the
///   sanitised spelling is a different string and would repin something
///   else (or nothing);
/// - one starting with `-`, which the argument parser reads as a flag
///   however it is quoted.
#[must_use]
pub fn repin_command_hint(field: &str, suggested_to: &str) -> Option<String> {
    let shown = sanitize_display_text(field);
    if shown != field || field.starts_with('-') {
        return None;
    }
    Some(format!(
        "trawl schema repin {} --to {} --dry-run",
        shell_quote(&shown),
        suggested_to.to_ascii_lowercase()
    ))
}

/// Whether a repin job is still working. Only the literal `running` is.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
#[must_use]
pub fn repin_is_running(status: &str) -> bool {
    status == STATUS_RUNNING
}

/// Whether a repin job has reached a status that will not change.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
#[must_use]
pub fn repin_is_terminal(status: &str) -> bool {
    !repin_is_running(status)
}

/// One hint string split into display segments: `(is_code, text)`, where
/// a backtick-delimited run is code and the ticks themselves are gone.
///
/// The hints above are written to be readable in a terminal, where a
/// backtick is the only markup there is. A browser must not paint the
/// ticks, so the view renders each code run as `<span class="mono">` and
/// the shared string stays CLI-compatible. An unterminated run is plain
/// text: these are constants, but a rule that reads correctness off that
/// is a rule that breaks when one is edited.
#[must_use]
pub fn hint_segments(hint: &str) -> Vec<(bool, String)> {
    let mut segments = Vec::new();
    let mut rest = hint;
    while let Some(open) = rest.find('`') {
        let Some(close) = rest[open + 1..].find('`').map(|i| open + 1 + i) else {
            break;
        };
        if open > 0 {
            segments.push((false, rest[..open].to_string()));
        }
        segments.push((true, rest[open + 1..close].to_string()));
        rest = &rest[close + 1..];
    }
    if !rest.is_empty() {
        segments.push((false, rest.to_string()));
    }
    segments
}

/// `arg` as one POSIX shell word — mirrors the CLI's `shell_quote`.
///
/// Bare when the name is already a shell-inert token (which every
/// ordinary field name is, and the common case must stay
/// copy-pasteable-looking), otherwise single-quoted with embedded quotes
/// closed and reopened (`'\''`), the one escape that works inside single
/// quotes.
fn shell_quote(arg: &str) -> String {
    let bare = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if bare {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        REPIN_HINT_REFUSED, hint_segments, repin_command_hint, repin_is_running, repin_is_terminal,
    };

    #[test]
    fn hint_segments_lift_the_ticks_out_of_the_text() {
        // Plain text is one segment and keeps its spelling.
        assert_eq!(
            hint_segments("no markup here"),
            vec![(false, "no markup here".to_string())]
        );
        assert_eq!(
            hint_segments("run `trawl schema repin` first"),
            vec![
                (false, "run ".to_string()),
                (true, "trawl schema repin".to_string()),
                (false, " first".to_string()),
            ]
        );
        // The shared CLI string keeps its backticks; the browser never
        // paints one.
        assert!(REPIN_HINT_REFUSED.contains('`'));
        let segments = hint_segments(REPIN_HINT_REFUSED);
        assert!(segments.iter().any(|(code, _)| *code), "no code run found");
        for (_, text) in &segments {
            assert!(!text.contains('`'), "a tick survived into {text:?}");
        }
        // Round-trips the text itself, ticks aside.
        let flattened: String = segments.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(flattened, REPIN_HINT_REFUSED.replace('`', ""));
        // An unterminated run is text, not an open code span.
        assert_eq!(
            hint_segments("a `dangling tick"),
            vec![(false, "a `dangling tick".to_string())]
        );
    }

    #[test]
    fn ordinary_names_get_a_bare_command() {
        assert_eq!(
            repin_command_hint("duration", "VARCHAR").as_deref(),
            Some("trawl schema repin duration --to varchar --dry-run")
        );
        assert_eq!(
            repin_command_hint("http.status_code", "BIGINT").as_deref(),
            Some("trawl schema repin http.status_code --to bigint --dry-run")
        );
    }

    #[test]
    fn shell_active_names_are_quoted() {
        assert_eq!(
            repin_command_hint("a b", "varchar").as_deref(),
            Some("trawl schema repin 'a b' --to varchar --dry-run")
        );
        assert_eq!(
            repin_command_hint("a;rm -rf /", "varchar").as_deref(),
            Some("trawl schema repin 'a;rm -rf /' --to varchar --dry-run")
        );
        assert_eq!(
            repin_command_hint("it's", "varchar").as_deref(),
            Some(r"trawl schema repin 'it'\''s' --to varchar --dry-run")
        );
    }

    #[test]
    fn unrenderable_or_flag_shaped_names_get_no_command() {
        // Does not survive sanitisation: the shown spelling would be a
        // different field.
        assert_eq!(repin_command_hint("dur\u{202e}ation", "varchar"), None);
        assert_eq!(repin_command_hint("dur\u{200b}ation", "varchar"), None);
        // Read as a flag however it is quoted.
        assert_eq!(repin_command_hint("-x", "varchar"), None);
        assert_eq!(repin_command_hint("--to", "varchar"), None);
    }

    /// The closed vocabulary of `RepinJobStatus` in
    /// `crates/trawl-server/src/store/repin.rs` — the authority this
    /// module mirrors. A status added there without landing here renders
    /// as terminal, which is the safe default (polling stops).
    #[test]
    fn only_running_is_non_terminal() {
        for status in [
            "succeeded",
            "failed",
            "refused_needs_force",
            "blocked",
            // not in the vocabulary — a future server's spelling
            "something_new",
        ] {
            assert!(!repin_is_running(status), "{status}");
            assert!(repin_is_terminal(status), "{status}");
        }
        assert!(repin_is_running("running"));
        assert!(!repin_is_terminal("running"));
    }
}
