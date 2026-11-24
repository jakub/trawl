// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawl-mcp: the MCP server for trawl (ADR-0058).

pub mod backend;
mod blocking;
pub mod dsl;
pub mod hints;
pub mod render;
pub mod reply;
pub mod serve;
pub mod server;
pub mod session;
pub mod tools;

// The tool tests' fake backend, for a unit test that needs one: it names
// this crate as `trawl_mcp`.
#[cfg(test)]
extern crate self as trawl_mcp;
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod support;

pub use backend::Backend;
pub use serve::{ServeError, serve_stdio};
pub use server::TrawlServer;
