// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Theme, row-style, sidebar and reading-mode preferences synced to `localStorage`
//! and projected onto `<html data-*>` attributes.
//!
//! Split into two layers:
//! - [`prefs`] — pure types, parsers, and serialisation. Builds on
//!   every target so the JSON contract can be unit-tested natively.
//! - [`runtime`] — wasm-only DOM + `localStorage` glue and the
//!   [`runtime::install`] entry point that wires reactive signals to
//!   the persisted state.
//!
//! [`ThemePreference`] is the stored System/Light/Dark choice; [`Theme`] is
//! the binary resolved appearance consumed by CSS, charts, and Atmosphere.
//! Runtime readers receive a read-only signal and controls select preferences.
//! Consumers that need the appearance before Wasm starts must also adopt
//! `js/theme-bootstrap.js` as a same-origin, blocking classic Trunk asset
//! before styles and Wasm, using the same storage namespace as installation.
//! The bootstrap only reads storage; CSS owns `color-scheme`.

pub mod prefs;

#[cfg(target_arch = "wasm32")]
pub mod runtime;

pub use prefs::{Details, ParseThemeError, RowStyle, Rows, Sidebar, Theme, ThemePreference};

#[cfg(target_arch = "wasm32")]
pub use runtime::{UiPrefs, install};
