// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusTone {
    /// Bare `.status-dot` — an unknown/other status. Renders as an
    /// invisible spacer dot.
    #[default]
    Neutral,
    Success,
    Error,
    Running,
}

impl StatusTone {
    #[must_use]
    pub fn css_suffix(self) -> Option<&'static str> {
        match self {
            Self::Neutral => None,
            Self::Success => Some("success"),
            Self::Error => Some("error"),
            Self::Running => Some("running"),
        }
    }
}

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
    }

    #[test]
    fn dot_class_composition() {
        assert_eq!(dot_class(StatusTone::Neutral), "status-dot");
        assert_eq!(dot_class(StatusTone::Error), "status-dot error");
    }

    #[test]
    fn default_tone_is_neutral() {
        assert_eq!(StatusTone::default(), StatusTone::Neutral);
    }
}
