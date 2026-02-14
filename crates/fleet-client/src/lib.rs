//! fleet-client: shared client library for communicating with fleetd.
//!
//! Used by fleet-cli, fleet-admin, and the TUI. Handles connection
//! management, authentication, query submission, and result streaming
//! across all supported transports.

pub mod connection;
pub mod error;

pub use connection::{
    ActiveQuerySnapshot, CancelResponse, CompletedQuerySnapshot, HealthResponse, HttpClient,
    PaginationMeta, QueriesResponse, QueryResponse, SchemaColumnResponse, SchemaResponse,
    ValidationResponse,
};
pub use error::ClientError;
