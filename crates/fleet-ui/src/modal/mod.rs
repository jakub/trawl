// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Modal dialog family.

pub mod confirm_state;

#[cfg(target_arch = "wasm32")]
pub mod confirm;
#[cfg(target_arch = "wasm32")]
pub mod confirm_reason;
#[cfg(target_arch = "wasm32")]
pub mod shell;

pub use confirm_state::ConfirmState;

#[cfg(target_arch = "wasm32")]
pub use confirm::ConfirmModal;
#[cfg(target_arch = "wasm32")]
pub use confirm_reason::ConfirmWithReasonModal;
#[cfg(target_arch = "wasm32")]
pub use shell::Modal;
