// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure segmented-strip class composition. No `leptos`, no `web_sys` —
//! builds on every target so the size/full class fragment rendered by
//! `<Segmented>` is exercised by native unit tests (mirrors
//! [`crate::badge::tone`]). The strip's option identity ([`SegmentedOption`])
//! is a `&'static str` id, likewise pure; apps with typed enums adapt at
//! the call site (ADR-0002).

use crate::button::Size;

/// One option in the strip.
#[derive(Debug, Clone)]
pub struct SegmentedOption {
    pub id: &'static str,
    pub label: &'static str,
}

impl SegmentedOption {
    #[must_use]
    pub fn new(id: &'static str, label: &'static str) -> Self {
        Self { id, label }
    }
}

/// Compose the `class` attribute for the strip container rendered by
/// `<Segmented>`. The base class is `seg`; `Sm`/`Xs` add `seg-sm` and
/// `full` adds `seg-full`.
///
/// The design has only two scales, so [`Size::Xs`] collapses onto the
/// same `seg-sm` fragment as [`Size::Sm`] — there is no `seg-xs` rule in
/// `styles/fleet-ui.css`. The contract is with the `.seg` / `.seg-sm` /
/// `.seg-full` selectors there.
#[must_use]
pub fn segmented_class(size: Size, full: bool) -> String {
    let mut class = String::from("seg");
    if !matches!(size, Size::Default) {
        class.push_str(" seg-sm");
    }
    if full {
        class.push_str(" seg-full");
    }
    class
}

#[cfg(test)]
mod tests {
    use super::{SegmentedOption, segmented_class};
    use crate::button::Size;

    #[test]
    fn base_class_for_default_scale() {
        assert_eq!(segmented_class(Size::Default, false), "seg");
    }

    #[test]
    fn sm_and_xs_both_render_as_seg_sm() {
        // Only two scales ship: Xs deliberately collapses onto Sm's
        // `seg-sm` fragment (no `seg-xs` rule in the stylesheet).
        assert_eq!(segmented_class(Size::Sm, false), "seg seg-sm");
        assert_eq!(segmented_class(Size::Xs, false), "seg seg-sm");
    }

    #[test]
    fn full_adds_seg_full_across_scales() {
        assert_eq!(segmented_class(Size::Default, true), "seg seg-full");
        assert_eq!(segmented_class(Size::Sm, true), "seg seg-sm seg-full");
        assert_eq!(segmented_class(Size::Xs, true), "seg seg-sm seg-full");
    }

    #[test]
    fn option_new_sets_id_and_label() {
        let opt = SegmentedOption::new("csv", "CSV");
        assert_eq!(opt.id, "csv");
        assert_eq!(opt.label, "CSV");
    }
}
