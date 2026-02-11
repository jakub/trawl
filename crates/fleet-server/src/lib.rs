//! fleet-server: the fleetd daemon.
//!
//! Multi-transport server (HTTP via axum, Unix socket, TCP+TLS) that
//! authenticates clients, executes queries, and streams results.

pub mod auth;
pub mod config;
pub mod error;
pub mod handlers;
pub mod pool;
pub mod shutdown;
pub mod state;
pub mod tls;
pub mod tracker;
pub mod transport;
