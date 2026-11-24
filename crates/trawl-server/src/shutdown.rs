// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::Duration;

/// Long enough that a process out of file descriptors does not spin on the
/// error; short enough that it serves again soon after descriptors free up.
pub const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

pub type ShutdownTx = tokio::sync::watch::Sender<bool>;

pub type ShutdownRx = tokio::sync::watch::Receiver<bool>;

/// A watch channel rather than a `Notify`: the flag is STATE, so a task
/// that subscribes (or re-polls) after the send still sees it.
#[must_use]
pub fn shutdown_channel() -> (ShutdownTx, ShutdownRx) {
    tokio::sync::watch::channel(false)
}

/// A dropped sender means whoever owned the shutdown is gone, which is
/// shutdown by another name.
pub async fn shutdown_observed(rx: &mut ShutdownRx) {
    let _ = rx.wait_for(|flagged| *flagged).await;
}

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
