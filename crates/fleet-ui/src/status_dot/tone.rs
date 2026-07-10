// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure status-dot tone. No `leptos`, no `web_sys` — builds on every
//! target so the per-tone CSS-class fragment is exercised by native
//! unit tests (mirrors [`crate::button::variant`]). Issue #31 folds
//! trawl's two hand-rolled dot families — the four-way
//! `"status-dot success" / "status-dot error" / …` run-status match
//! (duplicated in `nets`/`runs`/`net_drawer`) and the binary
//! `.sd-dot` / `.sd-dot.errors` schema-health dot — onto one canonical
//! `.status-dot` family. Mapping a domain status *string* onto a tone
//! stays app-side (ADR-0002: status vocabularies are app semantics).

/// Dot tone. Maps onto the `.status-dot.{tone}` classes shipped in
/// `styles/fleet-ui.css`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusTone {
    /// Bare `.status-dot` — an unknown/other status. Renders as an
    /// invisible spacer dot, matching the pre-unification `_ =>` arm.
    #[default]
    Neutral,
    Success,
    Error,
    Running,
    Timeout,
}

impl StatusTone {
    /// CSS class fragment appended by [`dot_class`]. `None` for the
    /// neutral (bare-class) tone.
    #[must_use]
    pub fn css_suffix(self) -> Option<&'static str> {
        match self {
            Self::Neutral => None,
            Self::Success => Some("success"),
            Self::Error => Some("error"),
            Self::Running => Some("running"),
            Self::Timeout => Some("timeout"),
        }
    }
}

/// Compose the full `class` attribute rendered by `<StatusDot>`.
#[must_use]
pub fn dot_class(tone: StatusTone) -> String {
    match tone.css_suffix() {
        Some(suffix) => format!("status-dot {suffix}"),
        None => "status-dot".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{StatusTone, dot_class};

    #[test]
    fn css_suffix_mapping() {
        assert_eq!(StatusTone::Neutral.css_suffix(), None);
        assert_eq!(StatusTone::Success.css_suffix(), Some("success"));
        assert_eq!(StatusTone::Error.css_suffix(), Some("error"));
        assert_eq!(StatusTone::Running.css_suffix(), Some("running"));
        assert_eq!(StatusTone::Timeout.css_suffix(), Some("timeout"));
    }

    #[test]
    fn dot_class_composition() {
        // Bare class for the unknown-status arm, exactly as the four
        // pre-unification `match` copies rendered it.
        assert_eq!(dot_class(StatusTone::Neutral), "status-dot");
        assert_eq!(dot_class(StatusTone::Error), "status-dot error");
    }

    #[test]
    fn default_tone_is_neutral() {
        assert_eq!(StatusTone::default(), StatusTone::Neutral);
    }
}
