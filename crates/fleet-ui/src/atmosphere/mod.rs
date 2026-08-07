// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WebGL shader backdrop over the vendored @paper-design/shaders
//! bundle (jakub/coastwatch#308, ADR-0012).
//!
//! Split into the crate's native/wasm layers:
//! - [`palette`] — pure per-theme palettes + every tunable knob (the
//!   single iteration site), natively tested, always compiled;
//! - `interop` + the `Atmosphere` component — wasm32-only glue over
//!   `vendor/paper-shaders.js`, behind the default-off `atmosphere`
//!   cargo feature.
//!
//! That feature gate is a WIRE-SIZE gate, not a taste one: `interop`'s
//! `#[wasm_bindgen(module = "/vendor/paper-shaders.js")]` extern block
//! is a compile-time local snippet, so merely LINKING it makes
//! wasm-bindgen emit the 142 KB bundle into the consumer's dist and
//! modulepreload it from `index.html` — whether or not anything mounts
//! the component. Gating the extern block is what keeps those bytes
//! out of the dist of a consumer that never mounts the backdrop
//! (trawl-web-ui today).

pub mod palette;

#[cfg(all(target_arch = "wasm32", feature = "atmosphere"))]
pub mod interop;

#[cfg(all(target_arch = "wasm32", feature = "atmosphere"))]
mod component;

#[cfg(all(target_arch = "wasm32", feature = "atmosphere"))]
pub use component::Atmosphere;
