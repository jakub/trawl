// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Whether a daemon's human-readable log lines carry ANSI colour.
//!
//! Lives here so `trawld` and `trawl-web` apply one rule. Their stdout
//! usually goes to journald, `kubectl logs`, or a file a log shipper
//! reads, and escape sequences there end up inside stored events.
//! tracing-subscriber's default colours whenever `NO_COLOR` is unset and
//! never checks for a terminal, while an explicit `with_ansi(true)`
//! ignores `NO_COLOR` altogether, so the daemons pass the result of this
//! module to `with_ansi` instead of relying on either.

use std::ffi::OsStr;
use std::io::IsTerminal as _;

/// Colour only on a terminal, and never when `NO_COLOR` holds a
/// non-empty value (<https://no-color.org>).
#[must_use]
pub fn ansi_enabled(stdout_is_terminal: bool, no_color: Option<&OsStr>) -> bool {
    stdout_is_terminal && no_color.is_none_or(OsStr::is_empty)
}

/// [`ansi_enabled`] for this process's stdout and environment.
#[must_use]
pub fn stdout_ansi() -> bool {
    ansi_enabled(
        std::io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::ansi_enabled;
    use std::ffi::OsStr;

    #[test]
    fn colour_needs_a_terminal() {
        assert!(!ansi_enabled(false, None));
        assert!(!ansi_enabled(false, Some(OsStr::new(""))));
        assert!(ansi_enabled(true, None));
    }

    #[test]
    fn a_non_empty_no_color_wins_on_a_terminal() {
        assert!(!ansi_enabled(true, Some(OsStr::new("1"))));
        assert!(!ansi_enabled(true, Some(OsStr::new("0"))));
        assert!(ansi_enabled(true, Some(OsStr::new(""))));
    }
}
