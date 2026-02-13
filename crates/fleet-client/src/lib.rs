//! fleet-client: shared client library for communicating with fleetd.
//!
//! Used by fleet-cli, fleet-admin, and the TUI. Handles connection
//! management, authentication, query submission, and result streaming
//! across all supported transports.

pub mod connection;
pub mod error;

pub use connection::{
    ActiveQuerySnapshot, CompletedQuerySnapshot, HealthResponse, HttpClient, QueriesResponse,
    SchemaColumnResponse, SchemaResponse,
};
pub use error::ClientError;
