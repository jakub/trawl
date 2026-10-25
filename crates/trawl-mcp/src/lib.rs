// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawl-mcp: the MCP server for trawl (ADR-0058).
//!
//! The server is a client. Every tool call crosses [`Backend`], whose only
//! implementation is trawl-client over HTTP to a trawld, so every guard the
//! API applies, applies: authentication, the trawl grant, permissions,
//! admission, deadlines and quotas. The crate never depends on
//! trawl-server, and it holds no model, no SQL and no prompt.
//!
//! - [`backend`]: the typed trait over the trawl-client calls the tools use.
//! - [`server`]: [`TrawlServer`], a hand-written `rmcp::ServerHandler`.
//! - [`tools`]: the fixed tool table, its schemas and dispatch.
//! - [`reply`]: what each tool answers, as data: the `structuredContent`
//!   mirror of the text block and the source of each output schema.
//! - [`render`]: the text contract: replies as one text block and its
//!   mirror under a byte budget, and refusals as error text.
//! - [`hints`]: the computed hints a search result carries.
//! - [`dsl`]: the DSL reference as sectioned resources, and the essentials
//!   block `orient` prints.
//! - [`serve`]: the stdio transport, [`serve_stdio`].

pub mod backend;
pub mod dsl;
pub mod hints;
pub mod render;
pub mod reply;
pub mod serve;
pub mod server;
pub mod tools;

pub use backend::Backend;
pub use serve::{ServeError, serve_stdio};
pub use server::TrawlServer;
