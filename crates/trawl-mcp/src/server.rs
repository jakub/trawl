// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`TrawlServer`], the hand-written `rmcp::ServerHandler` (ADR-0058).
//!
//! The handler keeps rmcp's default protocol version set and its default
//! `server/discover`, so a 2025-11-25 client's `initialize` and a
//! 2026-07-28 client's `server/discover` both work. It does not use rmcp's
//! tool router macros: those sort tools by name and turn a typed result
//! into a JSON dump, where ADR-0058 wants a fixed order and a text block
//! that stands alone.

use rmcp::ServerHandler;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};

use crate::backend::Backend;

/// The name the server reports in `serverInfo`.
pub const SERVER_NAME: &str = "trawl";

/// The instructions the server sends a client at `initialize` and
/// `server/discover`.
pub const INSTRUCTIONS: &str = "Call orient first: it describes this trawl installation \
and the query language in one call.";

/// The MCP server over one [`Backend`].
#[derive(Debug)]
pub struct TrawlServer<B> {
    backend: B,
}

impl<B: Backend> TrawlServer<B> {
    /// A server whose every tool call goes to `backend`.
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    /// The backend the tools call.
    pub fn backend(&self) -> &B {
        &self.backend
    }
}

impl<B: Backend + 'static> ServerHandler for TrawlServer<B> {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().build())
            .with_server_info(
                Implementation::new(SERVER_NAME, trawl_core::version::PKG_VERSION)
                    .with_title("Trawl"),
            )
            .with_instructions(INSTRUCTIONS)
    }
}
