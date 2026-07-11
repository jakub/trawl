// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure badge tone. No `leptos`, no `web_sys` — builds on every target
//! so the per-tone CSS-class fragment is exercised by native unit
//! tests (mirrors [`crate::button::variant`]). Issue #31 replaces
//! trawl's 22 `class="intel-badge" style="background:…;color:…"` call
//! sites with `<Badge tone=Tone::…>`; apps map their domain kinds
//! (story state, TLP marking, claim relationship, …) onto this closed
//! tone set at the call site — there is deliberately NO color/style
//! passthrough prop (ADR-0003 hard decision, human call 2026-07-10).

/// Semantic tone. Maps onto the `.bdg.{tone}` classes shipped in
/// `styles/fleet-ui.css`, drawing on the fleet color tokens
/// (`--ink-3` / `--blue` / `--green` / `--amber` / `--red`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tone {
    #[default]
    Neutral,
    Info,
    Success,
    Warn,
    Danger,
}

impl Tone {
    /// CSS class fragment appended by [`badge_class`]. The contract
    /// with fleet-ui.css's `.bdg.neutral` / `.bdg.info` / `.bdg.success`
    /// / `.bdg.warn` / `.bdg.danger` selectors.
    #[must_use]
    pub fn css_class(self) -> &'static str {
        match self {
            Self::Neutral => "neutral",
            Self::Info => "info",
            Self::Success => "success",
            Self::Warn => "warn",
            Self::Danger => "danger",
        }
    }
}

/// Compose the full `class` attribute rendered by `<Badge>`. The base
/// class is `bdg`, NOT `badge` — fleet-ui.css already owns a
/// `.rail .it .badge` rule (the rail count chip), and a top-level
/// `.badge` rule would leak `display` / `text-transform` /
/// `letter-spacing` into that chip through the cascade.
#[must_use]
pub fn badge_class(tone: Tone) -> String {
    format!("bdg {}", tone.css_class())
}

#[cfg(test)]
mod tests {
    use super::{Tone, badge_class};

    #[test]
    fn css_class_mapping() {
        assert_eq!(Tone::Neutral.css_class(), "neutral");
        assert_eq!(Tone::Info.css_class(), "info");
        assert_eq!(Tone::Success.css_class(), "success");
        assert_eq!(Tone::Warn.css_class(), "warn");
        assert_eq!(Tone::Danger.css_class(), "danger");
    }

    #[test]
    fn badge_class_composition() {
        assert_eq!(badge_class(Tone::Neutral), "bdg neutral");
        assert_eq!(badge_class(Tone::Danger), "bdg danger");
    }

    #[test]
    fn default_tone_is_neutral() {
        assert_eq!(Tone::default(), Tone::Neutral);
    }
}
