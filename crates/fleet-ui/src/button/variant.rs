// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Primary,
    Secondary,
    Danger,
    Form,
}

impl Variant {
    #[must_use]
    pub fn css_class(self) -> &'static str {
        match self {
            Self::Primary => "btn-pri",
            Self::Secondary => "btn-sec",
            Self::Danger => "btn-danger",
            Self::Form => "btn",
        }
    }
}

/// [`Size::Xs`] is a true modifier (`font-size` + `padding` only) and
/// composes with the variant class: `btn-sec btn-xs`. [`Size::Sm`] is a
/// self-contained compact style — `.btn-sm` carries its own
/// background/border/hover — so it renders standalone and the variant class
/// is suppressed; composing the two would alter the hover background.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Size {
    #[default]
    Default,
    Sm,
    Xs,
}

impl Size {
    #[must_use]
    pub fn css_suffix(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::Sm => Some("btn-sm"),
            Self::Xs => Some("btn-xs"),
        }
    }
}

#[must_use]
pub fn btn_class(variant: Variant, size: Size, full: bool) -> String {
    let base = match size {
        Size::Sm => "btn-sm".to_string(),
        Size::Xs => format!("{} btn-xs", variant.css_class()),
        Size::Default => variant.css_class().to_string(),
    };
    if full {
        format!("{base} btn-full")
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::{Size, Variant, btn_class};

    #[test]
    fn css_class_mapping() {
        assert_eq!(Variant::Primary.css_class(), "btn-pri");
        assert_eq!(Variant::Secondary.css_class(), "btn-sec");
        assert_eq!(Variant::Danger.css_class(), "btn-danger");
        assert_eq!(Variant::Form.css_class(), "btn");
    }

    #[test]
    fn size_suffix_mapping() {
        assert_eq!(Size::Default.css_suffix(), None);
        assert_eq!(Size::Sm.css_suffix(), Some("btn-sm"));
        assert_eq!(Size::Xs.css_suffix(), Some("btn-xs"));
    }

    #[test]
    fn btn_class_composition() {
        assert_eq!(
            btn_class(Variant::Secondary, Size::Default, false),
            "btn-sec"
        );
        assert_eq!(
            btn_class(Variant::Form, Size::Default, true),
            "btn btn-full"
        );

        assert_eq!(
            btn_class(Variant::Secondary, Size::Xs, false),
            "btn-sec btn-xs"
        );
        assert_eq!(
            btn_class(Variant::Primary, Size::Xs, false),
            "btn-pri btn-xs"
        );

        assert_eq!(btn_class(Variant::Secondary, Size::Sm, false), "btn-sm");
    }
}
