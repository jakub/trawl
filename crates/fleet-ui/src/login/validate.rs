// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure `<Login/>` logic: key validation + error precedence. No
//! `leptos`, no `web_sys` — builds on every target so the two
//! contracts ("blank/whitespace keys are rejected before `on_submit`
//! fires" and "local validation error wins over the external/server
//! error") are exercised by native unit tests rather than deferred to
//! a wasm integration suite. Mirrors [`crate::theme::prefs`] and
//! [`crate::button::variant`].

/// The message surfaced when the submitted key is unusable. Public so
/// the wasm component can render the exact same string the tests lock.
pub const KEY_REQUIRED: &str = "API key is required";

/// Validate the entered API key. `Err(KEY_REQUIRED)` when it's blank or
/// whitespace-only (the component must not invoke `on_submit` in that
/// case); `Ok(())` when it's usable.
///
/// # Errors
/// Returns [`KEY_REQUIRED`] if `key` is empty or all whitespace.
pub fn validate_key(key: &str) -> Result<(), &'static str> {
    if key.trim().is_empty() {
        Err(KEY_REQUIRED)
    } else {
        Ok(())
    }
}

/// Combine the local (client-side validation) error with the external
/// (caller/server) error. Local wins: after a failed submit the user
/// must see the fresh validation message, not a stale server one.
#[must_use]
pub fn combined(local: Option<String>, external: Option<String>) -> Option<String> {
    local.or(external)
}

#[cfg(test)]
mod tests {
    use super::{KEY_REQUIRED, combined, validate_key};

    #[test]
    fn empty_key_rejected() {
        assert_eq!(validate_key(""), Err(KEY_REQUIRED));
    }

    #[test]
    fn whitespace_only_key_rejected() {
        assert_eq!(validate_key("   "), Err(KEY_REQUIRED));
        assert_eq!(validate_key("\t\n "), Err(KEY_REQUIRED));
    }

    #[test]
    fn valid_key_passes() {
        assert_eq!(validate_key("sk-abc123"), Ok(()));
        // surrounding whitespace doesn't disqualify a non-blank key
        assert_eq!(validate_key("  token  "), Ok(()));
    }

    #[test]
    fn local_error_wins_over_external() {
        let out = combined(Some("API key is required".into()), Some("bad token".into()));
        assert_eq!(out.as_deref(), Some("API key is required"));
    }

    #[test]
    fn external_shown_when_no_local_error() {
        let out = combined(None, Some("bad token".into()));
        assert_eq!(out.as_deref(), Some("bad token"));
    }

    #[test]
    fn none_when_neither_error_present() {
        assert_eq!(combined(None, None), None);
    }
}
