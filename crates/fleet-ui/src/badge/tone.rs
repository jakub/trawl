// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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

/// The base class is `bdg`, not `badge`, because fleet-ui.css already owns a
/// `.rail .it .badge` rule (the rail count chip) and a top-level `.badge`
/// rule would leak `display` / `text-transform` / `letter-spacing` into
/// that chip through the cascade.
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
