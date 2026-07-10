// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Typed `<Btn variant=Variant>` replacing the stringly-typed
//! `class="btn-pri"` / `"btn-sec"` / `"btn-danger"` pattern.
//!
//! Split into two layers, mirroring [`crate::toast`]:
//! - [`variant`] — the pure [`Variant`] enum and its CSS-class
//!   contract. Builds on every target so the class attribute rendered
//!   by `<Btn>` is locked by native unit tests.
//! - [`component`] — the wasm-only `<Btn>` component.
//!
//! Adding a new variant is a one-line enum change visible to every
//! consumer — no more drift between callers that misspell or forget a
//! class name.

pub mod variant;

#[cfg(target_arch = "wasm32")]
pub mod component;

pub use variant::Variant;

#[cfg(target_arch = "wasm32")]
pub use component::Btn;
