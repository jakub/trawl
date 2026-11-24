// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WebGL shader backdrop over the vendored @paper-design/shaders
//! bundle (ADR-0012).

pub mod palette;

#[cfg(all(target_arch = "wasm32", feature = "atmosphere"))]
pub mod interop;

#[cfg(all(target_arch = "wasm32", feature = "atmosphere"))]
mod component;

#[cfg(all(target_arch = "wasm32", feature = "atmosphere"))]
pub use component::Atmosphere;
