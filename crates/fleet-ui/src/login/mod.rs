// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Login/>` — generic API-key sign-in form.

pub mod validate;

#[cfg(target_arch = "wasm32")]
pub mod component;

#[cfg(target_arch = "wasm32")]
pub use component::Login;
