// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Toast notifications: pure kinds + wasm-only bus/host runtime.
//!
//! Split into pure + wasm layers, mirroring [`crate::theme`]:
//! - [`kinds`] — the pure [`ToastKind`] enum and its CSS-class
//!   contract. Builds on every target so native-compiling consumer
//!   code (API mappers, test helpers) can name toast kinds without
//!   pulling leptos.
//! - [`stack`] — the pure [`ToastStack`] state machine ([`Toast`],
//!   its optional [`ToastLink`], id allocation + push/dismiss). Also target-agnostic, so the
//!   "a success and an error toast are now live" outcome is unit-tested
//!   natively rather than eyeballed in a browser.
//! - [`runtime`] — wasm-only `ToastBus` push handle (a reactive
//!   `RwSignal<ToastStack>`) and the `<Toasts/>` host component.
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
pub mod stack;

#[cfg(target_arch = "wasm32")]
pub mod runtime;

pub use kinds::ToastKind;
pub use stack::{Toast, ToastLink, ToastStack};

#[cfg(target_arch = "wasm32")]
pub use runtime::{ToastBus, Toasts};
