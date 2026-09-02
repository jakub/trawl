// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Sparkline/>` — inline SVG line + fill area. Pure SVG, no app
//! types; the donut chart and histogram stay app-side because they
//! encode level/type semantics.
//!
//! Split into two layers, mirroring [`crate::button`]:
//! - [`geometry`] — pure point/scale math, natively unit-tested.
//! - [`component`] — the wasm-only `<Sparkline>` component.

pub mod geometry;

#[cfg(target_arch = "wasm32")]
pub mod component;

pub use geometry::{SparkPath, spark_path};

#[cfg(target_arch = "wasm32")]
pub use component::Sparkline;
