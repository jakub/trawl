//! fleet-server: the fleetd daemon.
//!
//! Multi-transport server (HTTP via axum, Unix socket, TCP+TLS) that
//! authenticates clients, executes queries, and streams results.

pub mod audit;
pub mod auth;
pub mod config;
pub mod error;
pub mod handlers;
pub mod ingest;
pub mod pool;
pub mod rate_limit;
pub mod retention;
pub mod shutdown;
pub(crate) mod source;
pub mod state;
pub mod telemetry;
pub mod tls;
pub mod tracker;
pub mod transport;
