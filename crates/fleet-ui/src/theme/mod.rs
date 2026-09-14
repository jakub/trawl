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

pub mod prefs;

#[cfg(target_arch = "wasm32")]
pub mod runtime;

pub use prefs::{Details, ParseThemeError, RowStyle, Rows, Sidebar, Theme};

#[cfg(target_arch = "wasm32")]
pub use runtime::{UiPrefs, install};
