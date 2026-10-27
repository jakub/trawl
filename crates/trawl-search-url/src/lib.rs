// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The search URL codec (ADR-0027), as a pure library.
//!
//! The web UI and the MCP server both produce and read search links, so the
//! codec lives here rather than in the wasm bin crate. Nothing in this crate
//! touches a browser, a runtime or the network, and it builds on both the
//! host and `wasm32-unknown-unknown`.
//!
//! - [`search_url`]: the URL contract itself — `q`, `page`, `mode`, `f`, `r`
//!   and History's `hq`/`hpage`, with bounded, fail-closed decoding.
//! - [`query_merge`]: how filters and the range fold into the DSL that runs.
//! - [`filter_codec`]: the opaque, versioned `f` payload.
//! - [`result_path`]: the Search path that reopens a captured result.

pub mod filter_codec;
pub mod query_merge;
pub mod result_path;
pub mod search_url;
