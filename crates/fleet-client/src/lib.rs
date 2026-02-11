//! fleet-client: shared client library for communicating with fleetd.
//!
//! Used by fleet-cli, fleet-admin, and the TUI. Handles connection
//! management, authentication, query submission, and result streaming
//! across all supported transports.

/// Client connection management and transport selection.
pub mod connection;
