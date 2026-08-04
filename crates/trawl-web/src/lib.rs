// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl-web` library crate.
//!
//! Exists alongside the binary so integration tests can exercise the proxy's
//! handlers and middleware without spawning a real server.
//!
//! Session crypto and cookie building live in `fleet_auth::session`
//! (ADR-0004 slice 2) — this crate holds only the proxy-specific glue.

/// Default tracing filter for `trawl-web`, installed when `RUST_LOG` is unset.
///
/// This exact string is a cross-packaging contract — the binary's fallback,
/// the Helm chart's `web.logLevel`, and the Debian `trawl-web.default`
/// example all carry it. It enumerates every target the proxy emits under:
/// `trawl_web` (its own session, origin, upstream and startup diagnostics)
/// and `fleet_auth` (the shared session/origin primitives). trawld's
/// `DEFAULT_LOG_FILTER` names neither, so the sidecar must never inherit it
/// — a target-only filter that omits `trawl_web` silences the proxy
/// entirely. A global `info` is deliberately not the default: it would
/// enable noisy dependency targets (hyper, rustls, reqwest).
pub const DEFAULT_LOG_FILTER: &str = "trawl_web=info,fleet_auth=info";

pub mod assets;
pub mod config;
pub mod error;
pub mod middleware;
pub mod routes;
pub mod state;
