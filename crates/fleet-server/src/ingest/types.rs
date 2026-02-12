//! Request/response types for the ingest endpoint.

use serde::Serialize;

/// Response body for `POST /api/v1/ingest`.
#[derive(Debug, Serialize)]
pub struct IngestResponse {
    /// Number of events accepted into the WAL.
    pub accepted: usize,
}
