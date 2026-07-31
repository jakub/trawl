// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

/// Environment inherited by ordinary controller children.
///
/// Secret-looking ambient variables are excluded by construction. Resolved
/// values are added to individual consumers later.
#[must_use]
pub fn sanitized_base_from(
    environment: impl IntoIterator<Item = (OsString, OsString)>,
) -> BTreeMap<OsString, OsString> {
    environment
        .into_iter()
        .filter(|(name, _)| allowed_name(name))
        .collect()
}

#[must_use]
pub fn sanitized_base() -> BTreeMap<OsString, OsString> {
    sanitized_base_from(std::env::vars_os())
}

fn allowed_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    matches!(
        name,
        "PATH"
            | "HOME"
            | "USER"
            | "LOGNAME"
            | "SHELL"
            | "TERM"
            | "COLORTERM"
            | "LANG"
            | "TZ"
            | "TMPDIR"
            | "CARGO_HOME"
            | "RUSTUP_HOME"
            | "RUSTC_WRAPPER"
            | "NO_COLOR"
            | "CLICOLOR"
            | "FORCE_COLOR"
    ) || name.starts_with("LC_")
        || name.starts_with("XDG_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_excludes_tokens_and_agent_sockets() {
        let input = [
            ("PATH", "/bin"),
            ("HOME", "/home/dev"),
            ("LC_ALL", "C"),
            ("XDG_STATE_HOME", "/state"),
            ("OP_SERVICE_ACCOUNT_TOKEN", "secret"),
            ("DATABASE_URL", "secret"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let base = sanitized_base_from(input);
        assert_eq!(base.get(OsStr::new("PATH")), Some(&OsString::from("/bin")));
        assert!(base.contains_key(OsStr::new("LC_ALL")));
        assert!(base.contains_key(OsStr::new("XDG_STATE_HOME")));
        assert!(!base.contains_key(OsStr::new("OP_SERVICE_ACCOUNT_TOKEN")));
        assert!(!base.contains_key(OsStr::new("DATABASE_URL")));
        assert!(!base.contains_key(OsStr::new("SSH_AUTH_SOCK")));
        assert!(!base.contains_key(OsStr::new("AWS_SECRET_ACCESS_KEY")));
    }
}
