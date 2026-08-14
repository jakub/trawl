// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin surface's pure logic: the CLI command the degraded case
//! file offers, and the job-status vocabulary the SPA reads back.
//!
//! Both are MIRRORS of server-side authorities, deliberately not
//! imports — the same arrangement `WELL_KNOWN_LOG_FIELDS` has:
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

/// The one non-terminal status (synthesis ruling R5). Every other
/// spelling — including a status a future server adds — is terminal, so
/// an unknown value stops polling and renders verbatim rather than
/// spinning forever.
// The status half of this module is consumed by the repin progress
// surface (ADR-0011 C2 M3); M2 ships the mirror and its test.
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
/// length and case ONLY, so a name carrying a shell command or a bidi
/// override is legal — and this line is written to be pasted into a
/// shell. Two names get no command, matching the CLI exactly:
///
/// - one that does not survive display sanitisation, because the
///   sanitised spelling is a DIFFERENT string and would repin something
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
    use super::{repin_command_hint, repin_is_running, repin_is_terminal};

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
