// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The proxy's plain-HTTP listener (ADR-0054).
//!
//! TLS is not terminated here: it belongs to the deployment's ingress or
//! reverse proxy, so there is no handshake deadline to hold.

use std::future::Future;
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tower::Service as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionTimeouts {
    pub header_read: Duration,
}

impl ConnectionTimeouts {
    pub const DEFAULT: Self = Self {
        header_read: Duration::from_secs(30),
    };
}

pub const SHUTDOWN_DRAIN: Duration = Duration::from_secs(30);

const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

pub async fn serve(
    listener: TcpListener,
    router: Router,
    timeouts: ConnectionTimeouts,
    shutdown: impl Future<Output = ()>,
) {
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let mut connections = JoinSet::new();
    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (tcp, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        tracing::warn!(error = %error, "accept failed; accepting again after a backoff");
                        tokio::select! {
                            () = tokio::time::sleep(ACCEPT_ERROR_BACKOFF) => continue,
                            () = shutdown.as_mut() => break,
                        }
                    }
                };
                let router = router.clone();
                let mut shutdown_rx = shutdown_rx.clone();
                connections.spawn(async move {
                    let service = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                        let mut router = router.clone();
                        async move { router.call(req).await }
                    });
                    // HTTP/1.1 alone, through hyper's HTTP/1 builder: no
                    // protocol detection, which waits without bound for a
                    // preface. The header timer needs the tokio timer;
                    // without one hyper drops the deadline silently.
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder
                        .timer(TokioTimer::new())
                        .header_read_timeout(timeouts.header_read);
                    let conn = builder.serve_connection(TokioIo::new(tcp), service);
                    let mut conn = std::pin::pin!(conn);
                    tokio::select! {
                        result = conn.as_mut() => {
                            if let Err(error) = result {
                                tracing::debug!(peer = %peer, error = %error, "connection error");
                            }
                        }
                        () = shutdown_observed(&mut shutdown_rx) => {
                            conn.as_mut().graceful_shutdown();
                            if let Err(error) = conn.await {
                                tracing::debug!(peer = %peer, error = %error, "connection error during shutdown");
                            }
                        }
                    }
                });
            }
            // Reap a finished connection task as soon as it ends. The arm
            // is disabled while the set is empty, where `join_next` would
            // answer at once and spin the loop.
            Some(joined) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = joined && error.is_panic() {
                    tracing::error!("a connection task panicked");
                }
            }
            () = shutdown.as_mut() => break,
        }
    }

    tracing::info!(
        connections = connections.len(),
        "shutdown: draining open connections"
    );
    let _ = shutdown_tx.send(true);
    let drain = async { while connections.join_next().await.is_some() {} };
    if tokio::time::timeout(SHUTDOWN_DRAIN, drain).await.is_err() {
        tracing::warn!(
            remaining = connections.len(),
            "shutdown drain deadline passed; aborting the remaining connections"
        );
        connections.abort_all();
    }
    tracing::info!("trawl-web stopped");
}

/// The watch guard is dropped here, inside the function, so the future a
/// connection task selects on holds no lock across its own awaits.
async fn shutdown_observed(rx: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = rx.wait_for(|flagged| *flagged).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_timeouts_default_to_the_decided_deadline() {
        assert_eq!(
            ConnectionTimeouts::DEFAULT,
            ConnectionTimeouts {
                header_read: Duration::from_secs(30)
            }
        );
    }
}
