// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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
