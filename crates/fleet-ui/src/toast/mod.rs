// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub mod kinds;
pub mod stack;

#[cfg(target_arch = "wasm32")]
pub mod runtime;

pub use kinds::ToastKind;
pub use stack::{Toast, ToastLink, ToastStack};

#[cfg(target_arch = "wasm32")]
pub use runtime::{ToastBus, Toasts};
