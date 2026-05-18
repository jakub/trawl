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
//! The crate is `wasm32`-only — everything below the `#![cfg]` gate
//! compiles only when targeting `wasm32-unknown-unknown`. On native,
//! `cargo check -p fleet-ui` resolves to an empty crate, which is the
//! intended behaviour for `cargo check --workspace` runs.

#![cfg(target_arch = "wasm32")]

pub mod theme;

pub use theme::{Density, RowStyle, Theme, UiPrefs, install};
