//! fleet-server: the fleetd daemon.
//!
//! Multi-transport server (HTTP via axum, Unix socket, TCP+TLS) that
//! authenticates clients, executes queries, and streams results.

/// Server configuration and startup.
pub mod config;

/// Transport listeners (HTTP, Unix socket, TCP+TLS).
pub mod transport;

/// Connection and query lifecycle management.
pub mod session;
