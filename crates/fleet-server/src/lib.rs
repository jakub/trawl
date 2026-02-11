//! fleet-server: the fleetd daemon.
//!
//! Multi-transport server (HTTP via axum, Unix socket, TCP+TLS) that
//! authenticates clients, executes queries, and streams results.

pub mod config;
pub mod error;
pub mod pool;
pub mod shutdown;
