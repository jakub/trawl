// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Segmented/>` — exclusive-choice pill strip (issue #31).
//!
//! Unifies trawl's three hand-rolled segmented controls (the export
//! modal's `.fmt-btn` row, the schema page's `.seg-mini` density
//! toggle, and the date-range popover's mini tab strip) onto ONE
//! canonical treatment (ADR-0003): a bordered pill group with a single
//! accent-wash active style, `.seg > button.seg-opt(.on)` in
//! fleet-ui.css.
//!
//! Option identity is a `&'static str` id; apps with typed enums adapt
//! at the call site (a two-line id ↔ enum map), keeping app semantics
//! in the app (ADR-0002) — same contract as [`Tabs`](crate::tabs).
//! The size axis reuses [`Size`](crate::button::Size): `Default` for
//! modal-scale controls, `Sm` for toolbar-scale (schema's density
//! toggle); `Xs` renders as `Sm` (no third scale in the design).
//!
//! Split into two layers, mirroring [`crate::badge`]:
//! - [`class`] — the pure [`SegmentedOption`] identity and the
//!   [`segmented_class`] size/full composition. Builds on every target
//!   so the class attribute rendered by `<Segmented>` is locked by
//!   native unit tests.
//! - [`component`] — the wasm-only `<Segmented>` component.

pub mod class;

#[cfg(target_arch = "wasm32")]
pub mod component;

pub use class::{SegmentedOption, segmented_class};

#[cfg(target_arch = "wasm32")]
pub use component::Segmented;
