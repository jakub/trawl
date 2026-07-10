// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure button variant. No `leptos`, no `web_sys` — builds on every
//! target so the per-variant CSS-class fragment is exercised by native
//! unit tests (mirrors [`crate::toast::kinds`]). The migration in issue
//! #27 restructured trawl's login submit from hand-written
//! `class="btn btn-full"` markup into `<Btn variant=Variant::Form>`.
//!
//! Scope note: the rendered class *composition*
//! (`format!("{} btn-full", …)`) lives in the wasm-only
//! [`Btn`](super::Btn) component, so it is not verified natively. These
//! tests only lock the class fragment each variant maps to; the
//! `btn-full` suffix and the ordering that make up the full attribute
//! are a wasm-only concern.

/// Visual variant. Maps onto the CSS classes shipped in
/// `styles/fleet-ui.css`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Primary,
    Secondary,
    Danger,
    /// The large amber form primary (bare `.btn`) — the login page's
    /// submit button. Distinct from [`Variant::Primary`], which is the
    /// compact `.btn-pri` used in modal footers and toolbars.
    Form,
}

impl Variant {
    /// CSS class rendered by `<Btn>` (`class="{css_class} [btn-full]"`).
    /// The contract with fleet-ui.css's `.btn-pri` / `.btn-sec` /
    /// `.btn-danger` / `.btn` selectors. Public (like
    /// [`ToastKind::as_class`](crate::ToastKind::as_class)) so it stays
    /// nameable from native code and isn't dead-code on non-wasm builds.
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

/// Size axis, orthogonal to [`Variant`]. Maps onto the `.btn-sm` /
/// `.btn-xs` classes shipped in `styles/fleet-ui.css`.
///
/// The two non-default sizes behave differently by CSS design, and
/// [`btn_class`] encodes that asymmetry:
///
/// * [`Size::Xs`] is a true modifier (`font-size` + `padding` only) and
///   composes with the variant class: `btn-sec btn-xs`.
/// * [`Size::Sm`] is a self-contained compact style — `.btn-sm` carries
///   its own background/border/hover — so it renders standalone and the
///   variant class is suppressed (every pre-extraction call site used
///   bare `class="btn-sm"`; composing would alter the hover background).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Size {
    #[default]
    Default,
    Sm,
    Xs,
}

impl Size {
    /// CSS class fragment appended (or, for `Sm`, substituted) by
    /// [`btn_class`]. `None` for the default size.
    #[must_use]
    pub fn css_suffix(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::Sm => Some("btn-sm"),
            Self::Xs => Some("btn-xs"),
        }
    }
}

/// Compose the full `class` attribute rendered by `<Btn>`. Pure (and
/// natively tested) so the variant/size/full interaction — including the
/// `Sm`-standalone rule — is locked by unit tests rather than asserted
/// in prose. Order is variant-then-size, matching the hand-written
/// `class="btn-sec btn-xs"` markup this replaces.
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
        // Default size: variant class alone, `btn-full` appended when full.
        assert_eq!(
            btn_class(Variant::Secondary, Size::Default, false),
            "btn-sec"
        );
        assert_eq!(
            btn_class(Variant::Form, Size::Default, true),
            "btn btn-full"
        );

        // Xs is a true modifier: composes variant-then-size, matching the
        // existing hand-written `class="btn-sec btn-xs"` call sites.
        assert_eq!(
            btn_class(Variant::Secondary, Size::Xs, false),
            "btn-sec btn-xs"
        );
        assert_eq!(
            btn_class(Variant::Primary, Size::Xs, false),
            "btn-pri btn-xs"
        );

        // Sm is NOT a modifier: `.btn-sm` in the stylesheet is a
        // self-contained compact style (own background/border/hover), and
        // every pre-migration call site used it standalone. Emitting
        // `btn-sec btn-sm` would change the hover background, so Sm
        // suppresses the variant class entirely.
        assert_eq!(btn_class(Variant::Secondary, Size::Sm, false), "btn-sm");
    }
}
