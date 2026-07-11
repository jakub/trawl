// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Modal dialog family (issue #28, confirm-state helper issue #31).
//!
//! - [`shell`] — the `<Modal/>` primitive: scrim + panel + header/body/
//!   footer slots, window-level Escape / Cmd-Ctrl+Enter, NodeRef-based
//!   outside-click dismissal. Every dialog in the fleet composes it.
//! - [`confirm`] — `<ConfirmModal/>`, the confirm/cancel dialog
//!   (unchanged public API, now built on the shell).
//! - [`confirm_reason`] — `<ConfirmWithReasonModal/>`, confirm plus a
//!   required reason textarea (promoted from trawl-web-ui; coastwatch's
//!   quarantine-resolve and derivation-retract flows are the next
//!   consumers).
//! - [`confirm_state`] — the pure [`ConfirmState`] open/close payload
//!   state each page used to hand-roll as `RwSignal<Option<T>>`. Builds
//!   on every target so the lifecycle is natively unit-tested; the
//!   components above stay wasm-only.

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
