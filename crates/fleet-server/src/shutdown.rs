//! Graceful shutdown signal handling.

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
