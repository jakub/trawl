// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Toast notifications: pure kinds + wasm-only bus/host runtime.
//!
//! Split into two layers, mirroring [`crate::theme`]:
//! - [`kinds`] — the pure [`ToastKind`] enum and its CSS-class
//!   contract. Builds on every target so native-compiling consumer
//!   code (API mappers, test helpers) can name toast kinds without
//!   pulling leptos.
//! - [`runtime`] — wasm-only `ToastBus` push handle and the
//!   `<Toasts/>` host component.
//!
//! # Bus ownership contract
//!
//! [`Shell`](crate::shell::Shell) owns the bus: it constructs one
//! `ToastBus`, `provide_context`s it, and mounts the single
//! `<Toasts/>` host. Anything rendered inside `Shell` — including
//! router `<Outlet/>` content passed as `children` and the `footer`
//! slot — reaches it with `expect_context::<ToastBus>()`. Consumers
//! must NOT construct a second bus or mount their own `<Toasts/>`
//! inside a `Shell` — that yields two competing toast stacks.
//! Only screens rendered outside a `Shell` (e.g. a login page that
//! wants toasts) need their own bus + host.

pub mod kinds;

#[cfg(target_arch = "wasm32")]
pub mod runtime;

pub use kinds::ToastKind;

#[cfg(target_arch = "wasm32")]
pub use runtime::{Toast, ToastBus, Toasts};
