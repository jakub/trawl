// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Search-page state: URL-as-canonical-state, query signals, result resource.
//!
//! The URL query string (`?q=...&page=N`) is the single source of truth for
//! executed-query state. The editor's in-progress text lives in a separate
//! signal so typing doesn't spam the URL or trigger refetches.

// `app_mode` + `section` carry pure `&'static` data (rail descriptors,
// mode enum) that builds on every target so their contracts are
// native-testable; the rest of `state` pulls leptos and is wasm32-only.
pub mod app_mode;
pub mod section;
// `stream_session_value` is the SSE lane's cell decoder and live ring,
// split out of the wasm32-only `stream_session` for the same reason: it
// has no browser dependency, and behind the gate neither its
// number-landing rule nor the live filter rail could be tested.
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
