// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WebGL shader backdrop over the vendored @paper-design/shaders
//! bundle (jakub/coastwatch#308, ADR-0012).
//!
//! Split into the crate's native/wasm layers:
//! - [`palette`] — pure per-theme palettes + every tunable knob (the
//!   single iteration site), natively tested;
//! - [`interop`] + the [`Atmosphere`] component — wasm32-only glue
//!   over `vendor/paper-shaders.js`.

pub mod palette;

#[cfg(target_arch = "wasm32")]
pub mod interop;

#[cfg(target_arch = "wasm32")]
mod component;

#[cfg(target_arch = "wasm32")]
pub use component::Atmosphere;
