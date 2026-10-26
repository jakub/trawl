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

use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, Implementation, ListResourcesResult,
    ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, Resource, ResourceContents, ServerCapabilities, ServerConfig,
};
use rmcp::service::{MaybeSendFuture, RequestContext};
use rmcp::{ErrorData, RoleServer, ServerHandler};

use crate::backend::Backend;
use crate::session::Seen;
use crate::{dsl, tools};

/// The name the server reports in `serverInfo`.
pub const SERVER_NAME: &str = "trawl";

/// The instructions the server sends a client at `initialize` and
/// `server/discover`.
pub const INSTRUCTIONS: &str = "Call orient first: it describes this trawl installation \
and the query language in one call.";

/// How long a client may treat `tools/list` as fresh. The table is fixed
/// per binary; the TTL stays short so an upgraded server is seen soon.
pub const TOOLS_TTL_MS: u64 = 60_000;

/// How long a client may treat the DSL resources as fresh. They are
/// compiled in, so they change only with the binary, like the tool table.
pub const RESOURCES_TTL_MS: u64 = TOOLS_TTL_MS;

/// The display zone a server uses until [`TrawlServer::with_display_zone`]
/// names another.
pub const DEFAULT_DISPLAY_ZONE: &str = "UTC";

/// The MCP server over one [`Backend`]: one session, since one stdio
/// session is one process.
#[derive(Debug)]
pub struct TrawlServer<B> {
    backend: B,
    display_zone: String,
    /// The `seen_seq` of each investigation this session touched
    /// (ADR-0059, [`crate::session`]).
    seen: Seen,
}

impl<B: Backend> TrawlServer<B> {
    /// A server whose every tool call goes to `backend`, displaying times
    /// in [`DEFAULT_DISPLAY_ZONE`].
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            display_zone: DEFAULT_DISPLAY_ZONE.to_owned(),
            seen: Seen::default(),
        }
    }

    /// The same server, displaying times in `zone`: the `ui.timezone` of
    /// the CLI config on the machine that runs `trawl mcp`, which `orient`
    /// labels as that machine's setting next to UTC. It is one of the
    /// spellings the CLI accepts: `"local"` (that machine's offset at the
    /// moment of display), `"UTC"`, or a fixed offset such as `"+05:30"`.
    /// The CLI refuses any other spelling before the server starts.
    #[must_use]
    pub fn with_display_zone(mut self, zone: impl Into<String>) -> Self {
        self.display_zone = zone.into();
        self
    }

    /// The backend the tools call.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// The display zone, as [`Self::with_display_zone`] set it.
    pub fn display_zone(&self) -> &str {
        &self.display_zone
    }

    /// What this session has seen of each investigation.
    pub fn seen(&self) -> &Seen {
        &self.seen
    }
}

impl<B: Backend + 'static> ServerHandler for TrawlServer<B> {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(
            Implementation::new(SERVER_NAME, trawl_core::version::PKG_VERSION).with_title("Trawl"),
        )
        .with_instructions(INSTRUCTIONS)
    }

    /// The fixed table, in order, with a private cache scope: the table is
    /// the same for every key, but a cache shared across keys is not this
    /// server's to allow.
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + MaybeSendFuture + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(tools::schema::tools())
            .with_ttl_ms(TOOLS_TTL_MS)
            .with_cache_scope(CacheScope::Private)))
    }

    /// Run one tool, racing it against the request's cancellation token.
    ///
    /// rmcp cancels the token when the client sends
    /// `notifications/cancelled` for the request or the session ends, so
    /// the tool's future, and the trawld request inside it, is dropped
    /// rather than left to finish. rmcp sends no response for a cancelled
    /// request.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        tokio::select! {
            result = tools::dispatch(
                &request.name,
                request.arguments,
                &self.backend,
                &self.display_zone,
                &self.seen,
            ) => {
                result.map(CallToolResponse::from)
            }
            () = context.ct.cancelled() => {
                Err(ErrorData::internal_error("the request was cancelled", None))
            }
        }
    }

    /// The DSL reference sections, in page order.
    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, ErrorData>> + MaybeSendFuture + '_ {
        let resources = dsl::sections()
            .iter()
            .map(|section| {
                Resource::new(&section.uri, &section.name)
                    .with_title(format!("trawl DSL reference: {}", section.title))
                    .with_mime_type(dsl::MIME_TYPE)
                    .with_size(section.text.len() as u64)
            })
            .collect();
        std::future::ready(Ok(ListResourcesResult::with_all_items(resources)
            .with_ttl_ms(RESOURCES_TTL_MS)
            .with_cache_scope(CacheScope::Private)))
    }

    /// One DSL reference section by URI; an unknown URI is
    /// resource-not-found, which rmcp sends as `-32602` to a 2026-07-28
    /// peer.
    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ReadResourceResponse, ErrorData>> + MaybeSendFuture + '_ {
        let result = match dsl::section(&request.uri) {
            Some(section) => {
                Ok(
                    ReadResourceResult::new(vec![ResourceContents::TextResourceContents {
                        uri: section.uri.clone(),
                        mime_type: Some(dsl::MIME_TYPE.to_owned()),
                        text: section.text.clone(),
                        meta: None,
                    }])
                    .with_ttl_ms(RESOURCES_TTL_MS)
                    .with_cache_scope(CacheScope::Private)
                    .into(),
                )
            }
            None => Err(ErrorData::resource_not_found(
                "no such resource; resources/list names the trawl://dsl/ sections",
                None,
            )),
        };
        std::future::ready(result)
    }
}
