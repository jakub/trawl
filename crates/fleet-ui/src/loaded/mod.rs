// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Loaded/>` — render-prop wrapper for tri-state
//! (loading / error / ready) async surfaces. Deliberately not a leptos
//! `Suspense`/`ErrorBoundary` restructure: it wraps the resource
//! handling a page already has instead of rewriting it.
//!
//! Split into two layers, mirroring [`crate::toast`]:
//! - [`state`] — the pure [`LoadState`] enum, resource mapping, and
//!   canonical copy strings, natively unit-tested.
//! - [`component`] — the wasm-only `<Loaded>` wrapper.

pub mod state;

#[cfg(target_arch = "wasm32")]
pub mod component;

pub use state::{LoadState, error_copy, loading_copy, missing_copy};

#[cfg(target_arch = "wasm32")]
pub use component::Loaded;
