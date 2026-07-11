// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Typed `<StatusDot tone=StatusTone>` replacing the hand-rolled
//! `class="status-dot …"` / `class="sd-dot …"` dot markup
//! (issue #31, ADR-0003 unification).
//!
//! Split into two layers, mirroring [`crate::button`]:
//! - [`tone`] — the pure [`StatusTone`] enum and its CSS-class
//!   contract, natively unit-tested.
//! - [`component`] — the wasm-only `<StatusDot>` component.

pub mod tone;

#[cfg(target_arch = "wasm32")]
pub mod component;

pub use tone::{StatusTone, dot_class};

#[cfg(target_arch = "wasm32")]
pub use component::StatusDot;
