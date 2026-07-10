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

#[cfg(test)]
mod tests {
    use super::Variant;

    #[test]
    fn css_class_mapping() {
        assert_eq!(Variant::Primary.css_class(), "btn-pri");
        assert_eq!(Variant::Secondary.css_class(), "btn-sec");
        assert_eq!(Variant::Danger.css_class(), "btn-danger");
        assert_eq!(Variant::Form.css_class(), "btn");
    }
}
