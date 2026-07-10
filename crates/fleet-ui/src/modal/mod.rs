// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Modal dialog family (issue #28).
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

pub mod confirm;
pub mod confirm_reason;
pub mod shell;

pub use confirm::ConfirmModal;
pub use confirm_reason::ConfirmWithReasonModal;
pub use shell::Modal;
