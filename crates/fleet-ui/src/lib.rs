// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared Leptos 0.8 design system for fleet apps (ADR-0030, step 2).
//!
//! Ships design tokens, reset + base styles, the theme preference
//! system, and four typed components (`Btn`, `Field`, `ConfirmModal`,
//! `Toasts`). Consumed by trawl-web-ui and the future coastwatch-web
//! via workspace path deps; CSS is consumed via Trunk's `data-trunk
//! rel="css"` directive pointing at `styles/fleet-ui.css` in this
//! crate's directory.
//!
//! Most modules are wasm32-only — they pull leptos / web-sys / gloo
//! and only make sense in a browser. The exception is [`theme::prefs`],
//! a pure parsing layer that builds on every target so the JSON
//! contract with `localStorage` can be exercised by native unit tests.

pub mod theme;

#[cfg(target_arch = "wasm32")]
pub mod button;
#[cfg(target_arch = "wasm32")]
pub mod field;
#[cfg(target_arch = "wasm32")]
pub mod modal;
#[cfg(target_arch = "wasm32")]
pub mod toast;

pub use theme::{Density, RowStyle, Theme};

#[cfg(target_arch = "wasm32")]
pub use button::{Btn, Variant};
#[cfg(target_arch = "wasm32")]
pub use field::{Field, Helper};
#[cfg(target_arch = "wasm32")]
pub use modal::ConfirmModal;
#[cfg(target_arch = "wasm32")]
pub use theme::{UiPrefs, install};
#[cfg(target_arch = "wasm32")]
pub use toast::{Toast, ToastBus, ToastKind, Toasts};
