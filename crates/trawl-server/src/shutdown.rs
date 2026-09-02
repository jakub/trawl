// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Graceful shutdown signal handling.

/// The sending half of the server-wide shutdown flag.
pub type ShutdownTx = tokio::sync::watch::Sender<bool>;

/// The receiving half of the server-wide shutdown flag.
pub type ShutdownRx = tokio::sync::watch::Receiver<bool>;

/// Create a shutdown channel, unset.
///
/// A watch channel rather than a `Notify`: the flag is STATE, so a task
/// that subscribes (or re-polls) after the send still sees it.
/// `Notify::notify_waiters()` stores nothing, and every waiter that had
/// not registered at that instant missed the shutdown for good.
#[must_use]
pub fn shutdown_channel() -> (ShutdownTx, ShutdownRx) {
    tokio::sync::watch::channel(false)
}

/// Resolve once the shutdown flag is set, or once the sender is gone.
///
/// A dropped sender means whoever owned the shutdown is gone, which is
/// shutdown by another name. Both outcomes return, and neither is an
/// error the caller has to handle.
pub async fn shutdown_observed(rx: &mut ShutdownRx) {
    let _ = rx.wait_for(|flagged| *flagged).await;
}

/// Wait for a shutdown signal (SIGINT or SIGTERM).
///
/// Returns when the first signal is received, allowing the server to
/// drain in-flight requests before exiting.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install SIGINT handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {
            tracing::info!(event_type = "shutdown_signal", "received SIGINT, shutting down");
        }
        () = terminate => {
            tracing::info!(event_type = "shutdown_signal", "received SIGTERM, shutting down");
        }
    }
}
