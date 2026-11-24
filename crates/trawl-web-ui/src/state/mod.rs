// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Search-page state: URL-as-canonical-state, query signals, result resource.

// `app_mode` + `section` carry pure `&'static` data (rail descriptors,
// mode enum) that builds on every target so their contracts are
// native-testable; the rest of `state` pulls leptos and is wasm32-only.
pub mod app_mode;
pub mod section;
// `stream_session_value` has no browser dependency either, so it builds
// on every target for the same reason.
pub mod stream_session_value;

#[cfg(target_arch = "wasm32")]
pub mod filter_rail;
#[cfg(target_arch = "wasm32")]
pub mod query;
#[cfg(target_arch = "wasm32")]
pub mod search_session;
#[cfg(target_arch = "wasm32")]
pub mod stats_stream;
#[cfg(target_arch = "wasm32")]
pub mod stream_session;
