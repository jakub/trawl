// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The stdio transport (ADR-0058): `trawl mcp` serves one session over its
//! own standard input and output.

use rmcp::ServiceExt;
use rmcp::service::ServerInitializeError;
use rmcp::transport::io::stdio_pipes;

use crate::backend::Backend;
use crate::server::TrawlServer;

/// Why [`serve_stdio`] ended with an error.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The session never started: the client closed the pipe, or its first
    /// message was neither `initialize` nor a valid inline request. Boxed:
    /// rmcp's error can hold the whole first message.
    #[error("the MCP session did not start: {0}")]
    Start(Box<ServerInitializeError>),
    /// The task running the session panicked or was aborted.
    #[error("the MCP session task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// Serve `server` over this process's standard input and output until the
/// client closes the session.
///
/// stdout carries protocol frames and nothing else, so the caller must
/// send every log line to stderr before calling this: the CLI configures
/// tracing with a stderr writer and starts no TUI. When both streams are
/// pipes, as an MCP client launches a stdio server, rmcp drives them
/// through Tokio's non-blocking pipe driver; otherwise it falls back to
/// the blocking handles. Must run inside a Tokio runtime with IO enabled.
pub async fn serve_stdio<B: Backend + 'static>(server: TrawlServer<B>) -> Result<(), ServeError> {
    let running = server
        .serve(stdio_pipes())
        .await
        .map_err(|error| ServeError::Start(Box::new(error)))?;
    let reason = running.waiting().await?;
    tracing::debug!(?reason, "MCP session ended");
    Ok(())
}
