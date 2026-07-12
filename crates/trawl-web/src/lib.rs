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

pub mod assets;
pub mod config;
pub mod error;
pub mod middleware;
pub mod routes;
pub mod state;
