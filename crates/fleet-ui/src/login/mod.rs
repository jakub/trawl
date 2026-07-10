// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Login/>` — generic API-key sign-in form.
//!
//! Split into pure + wasm layers, mirroring [`crate::button`] and
//! [`crate::toast`]:
//! - [`validate`] — the pure key-validation + error-precedence logic.
//!   Builds on every target so the "blank key never fires `on_submit`"
//!   and "local error wins over external" contracts are locked by
//!   native unit tests.
//! - [`component`] — the wasm-only `<Login/>` component, which wires
//!   those helpers into leptos signals.

pub mod validate;

#[cfg(target_arch = "wasm32")]
pub mod component;

#[cfg(target_arch = "wasm32")]
pub use component::Login;
