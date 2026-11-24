// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Typed `<Btn variant=Variant>` over the `.btn-pri` / `.btn-sec` /
//! `.btn-danger` classes, so no call site spells one by hand.

pub mod variant;

#[cfg(target_arch = "wasm32")]
pub mod component;

pub use variant::{Size, Variant, btn_class};

#[cfg(target_arch = "wasm32")]
pub use component::Btn;
