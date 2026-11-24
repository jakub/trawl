// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Typed `<Badge tone=Tone>`, the one badge treatment (ADR-0003).

pub mod tone;

#[cfg(target_arch = "wasm32")]
pub mod component;

pub use tone::{Tone, badge_class};

#[cfg(target_arch = "wasm32")]
pub use component::Badge;
