//! trawl-server: the trawld daemon.
//!
//! Multi-transport server (HTTP via axum, Unix socket, TCP+TLS) that
//! authenticates clients, executes queries, and streams results.

pub mod audit;
pub mod auth;
pub mod bus;
pub mod config;
pub mod error;
pub mod handlers;
pub mod hot_buffer;
pub mod ingest;
pub mod metrics;
pub mod monitor;
pub mod pool;
pub mod query_log;
pub mod rate_limit;
pub mod retention;
pub mod scheduler;
pub mod shutdown;
pub(crate) mod source;
pub mod state;
pub mod stats;
pub mod telemetry;
pub mod tls;
pub mod tracker;
pub mod transport;
