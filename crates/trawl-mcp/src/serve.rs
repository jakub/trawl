// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The stdio transport (ADR-0058): `trawl mcp` serves one session over its
//! own standard input and output.
//!
//! rmcp's line transport logs a read or write error and ends the session
//! as if the client had closed it, so the two halves of the pipe pair are
//! wrapped in adapters that remember the first error each side saw. A
//! session that ended after one is a transport failure; EOF on stdin is
//! not an error and ends the session cleanly.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use rmcp::ServiceExt;
use rmcp::service::ServerInitializeError;
use rmcp::transport::io::stdio_pipes;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

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
    /// Reading stdin or writing stdout failed after the session started.
    /// The text names the direction and the error's kind, never a byte of
    /// the stream.
    #[error("the MCP transport failed: {0}")]
    Transport(String),
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
///
/// # Errors
///
/// [`ServeError::Transport`] when a read or a write failed during the
/// session, even though rmcp ended it as closed.
pub async fn serve_stdio<B: Backend + 'static>(server: TrawlServer<B>) -> Result<(), ServeError> {
    let failure = Failure::default();
    let (reader, writer) = stdio_pipes();
    let transport = (
        Recording {
            inner: reader,
            failure: failure.clone(),
            direction: Direction::Read,
        },
        Recording {
            inner: writer,
            failure: failure.clone(),
            direction: Direction::Write,
        },
    );
    let running = server
        .serve(transport)
        .await
        .map_err(|error| ServeError::Start(Box::new(error)))?;
    let reason = running.waiting().await?;
    tracing::debug!(?reason, "MCP session ended");
    match failure.first() {
        Some(failed) => Err(ServeError::Transport(failed)),
        None => Ok(()),
    }
}

/// Which side of the pipe pair failed.
#[derive(Debug, Clone, Copy)]
enum Direction {
    Read,
    Write,
}

/// The first transport error either side saw, as the text
/// [`ServeError::Transport`] carries.
#[derive(Debug, Clone, Default)]
struct Failure(Arc<Mutex<Option<String>>>);

impl Failure {
    /// Remember `error` unless an earlier one is already remembered.
    fn record(&self, direction: Direction, error: &io::Error) {
        let mut first = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if first.is_none() {
            let side = match direction {
                Direction::Read => "reading stdin",
                Direction::Write => "writing stdout",
            };
            *first = Some(format!("{side} failed: {}", error.kind()));
        }
    }

    fn first(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// One half of the pipe pair, recording its first error into a shared
/// [`Failure`] and otherwise passing every call through.
struct Recording<T> {
    inner: T,
    failure: Failure,
    direction: Direction,
}

impl<T> Recording<T> {
    /// `poll`, with an error it returns recorded.
    fn noted<V>(&self, poll: Poll<io::Result<V>>) -> Poll<io::Result<V>> {
        if let Poll::Ready(Err(error)) = &poll {
            self.failure.record(self.direction, error);
        }
        poll
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Recording<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_read(cx, buf);
        this.noted(poll)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Recording<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_write(cx, buf);
        this.noted(poll)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_flush(cx);
        this.noted(poll)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_shutdown(cx);
        this.noted(poll)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        this.noted(poll)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    /// A writer whose every write fails with `kind`.
    struct Broken(io::ErrorKind);

    impl AsyncWrite for Broken {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::new(self.0, "secret bytes from the stream")))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn the_first_error_is_kept_by_direction_and_kind_only() {
        let failure = Failure::default();
        let mut writer = Recording {
            inner: Broken(io::ErrorKind::BrokenPipe),
            failure: failure.clone(),
            direction: Direction::Write,
        };
        assert!(writer.write_all(b"{}\n").await.is_err());
        let mut second = Recording {
            inner: Broken(io::ErrorKind::ConnectionReset),
            failure: failure.clone(),
            direction: Direction::Write,
        };
        assert!(second.write_all(b"{}\n").await.is_err());
        let first = failure.first().expect("an error was recorded");
        assert_eq!(first, "writing stdout failed: broken pipe");
    }

    #[tokio::test]
    async fn end_of_input_is_not_a_failure() {
        let failure = Failure::default();
        let mut reader = Recording {
            inner: &b"{}\n"[..],
            failure: failure.clone(),
            direction: Direction::Read,
        };
        let mut read = String::new();
        reader.read_to_string(&mut read).await.unwrap();
        assert_eq!(read, "{}\n");
        assert!(failure.first().is_none());
    }
}
