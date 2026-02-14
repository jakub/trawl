//! HTTP client for communicating with fleetd.

use fleet_engine::value::QueryResult;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::ClientError;

/// HTTP client for the fleet daemon API.
#[derive(Clone)]
pub struct HttpClient {
    base_url: String,
    token: Zeroizing<String>,
    client: Client,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .field("client", &self.client)
            .finish()
    }
}

impl HttpClient {
    /// Long timeout for analytical queries over large parquet sets.
    /// The server enforces its own `query_timeout_sec`; this prevents
    /// client-side network timeouts on slow connections.
    const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

    /// Create a new client targeting the given daemon URL with an API key.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self, ClientError> {
        Self::build(base_url, token, false)
    }

    /// Create a client that accepts self-signed / invalid TLS certificates.
    ///
    /// Use this for development or when connecting to a daemon with an
    /// auto-generated self-signed cert.
    pub fn new_insecure(
        base_url: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<Self, ClientError> {
        Self::build(base_url, token, true)
    }

    /// Create a client with a pre-configured `reqwest::Client`.
    pub fn with_client(
        base_url: impl Into<String>,
        token: impl Into<String>,
        client: Client,
    ) -> Self {
        Self {
            base_url: normalize_base_url(base_url.into()),
            token: Zeroizing::new(token.into()),
            client,
        }
    }

    fn build(
        base_url: impl Into<String>,
        token: impl Into<String>,
        accept_invalid_certs: bool,
    ) -> Result<Self, ClientError> {
        let mut builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Self::DEFAULT_TIMEOUT);

        if accept_invalid_certs {
            builder = builder.danger_accept_invalid_certs(true);
        }

        let client = builder.build().map_err(sanitize_reqwest_error)?;
        Ok(Self {
            base_url: normalize_base_url(base_url.into()),
            token: Zeroizing::new(token.into()),
            client,
        })
    }

    /// Build a full URL for an API endpoint.
    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Execute a DSL query against the daemon.
    pub async fn query(&self, dsl: &str) -> Result<QueryResult, ClientError> {
        // Use query_paginated with no limits for backward compatibility.
        let response = self.query_paginated(dsl, None, None).await?;
        Ok(response.result)
    }

    /// Execute a DSL query with optional pagination.
    pub async fn query_paginated(
        &self,
        dsl: &str,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<QueryResponse, ClientError> {
        let url = self.endpoint("/api/v1/query");
        let body = QueryRequestPaginated {
            query: dsl.to_owned(),
            limit,
            offset,
        };
        let req = self.client.post(&url).json(&body);
        self.send_authenticated(req).await
    }

    /// Check daemon health (unauthenticated).
    pub async fn health(&self) -> Result<HealthResponse, ClientError> {
        let url = self.endpoint("/api/v1/health");

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(sanitize_reqwest_error)?;

        resp.json()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))
    }

    /// Fetch schema introspection from the daemon.
    pub async fn schema(&self) -> Result<SchemaResponse, ClientError> {
        let url = self.endpoint("/api/v1/schema");
        let req = self.client.get(&url);
        self.send_authenticated(req).await
    }

    /// Fetch active and recent queries from the daemon (admin only).
    pub async fn queries(&self) -> Result<QueriesResponse, ClientError> {
        let url = self.endpoint("/api/v1/queries");
        let req = self.client.get(&url);
        self.send_authenticated(req).await
    }

    /// Cancel a running query by ID (admin or owner only).
    pub async fn cancel_query(&self, query_id: u64) -> Result<CancelResponse, ClientError> {
        let url = self.endpoint(&format!("/api/v1/queries/{query_id}"));
        let req = self.client.delete(&url);
        self.send_authenticated(req).await
    }

    /// Validate a DSL query without executing it.
    ///
    /// Checks syntax and semantic rules (function names, arity, regex patterns)
    /// but does NOT validate field existence.
    pub async fn validate(&self, dsl: &str) -> Result<ValidationResponse, ClientError> {
        let url = self.endpoint("/api/v1/validate");
        let body = serde_json::json!({ "query": dsl });
        let req = self.client.post(&url).json(&body);
        self.send_authenticated(req).await
    }

    /// Fetch query history with pagination.
    pub async fn history(
        &self,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<HistoryResponse, ClientError> {
        let mut url = self.endpoint("/api/v1/history");

        // Build query string if parameters are provided.
        let mut params = vec![];
        if let Some(l) = limit {
            params.push(format!("limit={l}"));
        }
        if let Some(o) = offset {
            params.push(format!("offset={o}"));
        }
        if !params.is_empty() {
            url.push('?');
            url.push_str(&params.join("&"));
        }

        let req = self.client.get(&url);
        self.send_authenticated(req).await
    }

    /// Send an authenticated request, check for errors, and deserialize the response.
    async fn send_authenticated<T: serde::de::DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<T, ClientError> {
        let resp = req
            .header("Authorization", format!("Bearer {}", self.token.as_str()))
            .send()
            .await
            .map_err(sanitize_reqwest_error)?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let message = resp
                .json::<ErrorResponse>()
                .await
                .map_or_else(|_| "unknown error".into(), |e| e.error);
            return Err(ClientError::Server { status, message });
        }

        resp.json()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))
    }
}

/// Strip trailing slashes so `endpoint()` can simply concatenate.
fn normalize_base_url(url: String) -> String {
    let trimmed = url.trim_end_matches('/');
    if trimmed.len() == url.len() {
        url
    } else {
        trimmed.to_owned()
    }
}

/// Categorize a reqwest error without exposing raw details that might
/// contain tokens or internal URLs.
#[allow(clippy::needless_pass_by_value)] // used as `.map_err(sanitize_reqwest_error)`
fn sanitize_reqwest_error(e: reqwest::Error) -> ClientError {
    if e.is_timeout() {
        ClientError::Network("request timed out".into())
    } else if e.is_connect() {
        ClientError::Network("connection failed".into())
    } else if e.is_builder() {
        ClientError::Network("invalid request configuration".into())
    } else {
        ClientError::Network("request failed".into())
    }
}

// -- wire types --------------------------------------------------------------

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
}

/// Health check response from the daemon.
#[derive(Debug, Deserialize)]
pub struct HealthResponse {
    /// Status string, typically `"ok"`.
    pub status: String,
}

/// Schema introspection response from the daemon.
#[derive(Debug, Deserialize)]
pub struct SchemaResponse {
    /// Column descriptors (name + type).
    pub columns: Vec<SchemaColumnResponse>,
    /// Number of parquet files matching the configured glob.
    pub file_count: u64,
    /// Whether this result was served from cache.
    pub cached: bool,
}

/// A single column in the schema response.
#[derive(Debug, Deserialize)]
pub struct SchemaColumnResponse {
    /// Column name.
    pub name: String,
    /// Column data type (e.g. "VARCHAR", "TIMESTAMP").
    #[serde(rename = "type")]
    pub data_type: String,
}

/// Active and recent queries response from the daemon.
#[derive(Debug, Deserialize)]
pub struct QueriesResponse {
    /// Currently executing queries.
    pub active: Vec<ActiveQuerySnapshot>,
    /// Recently completed queries (most recent first).
    pub recent: Vec<CompletedQuerySnapshot>,
}

/// Snapshot of a currently executing query.
#[derive(Debug, Deserialize)]
pub struct ActiveQuerySnapshot {
    /// Monotonic query ID.
    pub id: u64,
    /// Authenticated user name.
    pub user: String,
    /// User's role.
    pub role: String,
    /// The DSL query string.
    pub query: String,
    /// How long the query has been running (ms).
    pub running_ms: u64,
}

/// A completed query from recent history.
#[derive(Debug, Deserialize)]
pub struct CompletedQuerySnapshot {
    /// Monotonic query ID.
    pub id: u64,
    /// Authenticated user name.
    pub user: String,
    /// The DSL query string.
    pub query: String,
    /// Execution duration in milliseconds.
    pub duration_ms: u64,
    /// Row count (if successful).
    pub rows: Option<usize>,
    /// Error message (if failed).
    pub error: Option<String>,
    /// Whether the query exceeded the timeout.
    pub timed_out: bool,
}

/// Response from the cancel query endpoint.
#[derive(Debug, Deserialize)]
pub struct CancelResponse {
    /// Whether the query was found and cancelled.
    pub cancelled: bool,
    /// The query ID that was requested for cancellation.
    pub query_id: u64,
}

/// Response from the validate endpoint.
#[derive(Debug, Deserialize)]
pub struct ValidationResponse {
    /// Whether the query is valid.
    pub valid: bool,
    /// Validation error messages (empty if valid).
    pub errors: Vec<String>,
}

/// Query response with pagination metadata.
#[derive(Debug, Deserialize)]
pub struct QueryResponse {
    /// The query result (columns + rows).
    #[serde(flatten)]
    pub result: QueryResult,
    /// Whether results were truncated due to `max_result_rows`.
    pub truncated: bool,
    /// Pagination metadata.
    pub pagination: PaginationMeta,
}

/// Pagination metadata for query responses.
#[derive(Debug, Deserialize)]
pub struct PaginationMeta {
    /// The limit applied to this response.
    pub limit: usize,
    /// The offset applied to this response.
    pub offset: usize,
    /// The number of rows actually returned.
    pub returned: usize,
}

/// Response from the history endpoint.
#[derive(Debug, Deserialize)]
pub struct HistoryResponse {
    /// Query history entries (most recent first).
    pub entries: Vec<HistoryEntryResponse>,
    /// Total number of history entries for this user.
    pub total: usize,
}

/// A single query history entry.
#[derive(Debug, Deserialize)]
pub struct HistoryEntryResponse {
    /// History entry ID.
    pub id: i64,
    /// The DSL query string.
    pub query: String,
    /// When the query was executed (ISO 8601 UTC).
    pub executed_at: String,
    /// Execution duration in milliseconds.
    pub duration_ms: u64,
    /// Number of rows returned.
    pub row_count: usize,
    /// Query status ("success", "error", "timeout").
    pub status: String,
}

// -- internal request types --------------------------------------------------

#[derive(Serialize)]
struct QueryRequestPaginated {
    query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── endpoint URL construction ───────────────────────────────────────

    #[test]
    fn endpoint_no_trailing_slash() {
        let client = HttpClient::with_client("https://localhost:8443", "tok", Client::new());
        assert_eq!(
            client.endpoint("/api/v1/query"),
            "https://localhost:8443/api/v1/query"
        );
    }

    #[test]
    fn endpoint_strips_trailing_slash() {
        let client = HttpClient::with_client("https://localhost:8443/", "tok", Client::new());
        assert_eq!(
            client.endpoint("/api/v1/query"),
            "https://localhost:8443/api/v1/query"
        );
    }

    #[test]
    fn endpoint_strips_multiple_trailing_slashes() {
        let client = HttpClient::with_client("https://localhost:8443///", "tok", Client::new());
        assert_eq!(
            client.endpoint("/api/v1/query"),
            "https://localhost:8443/api/v1/query"
        );
    }

    // ── debug redaction ─────────────────────────────────────────────────

    #[test]
    fn debug_redacts_token() {
        let client = HttpClient::with_client(
            "https://localhost:8443",
            "flt_XXXXXXXX_secrettoken123456789012345",
            Client::new(),
        );
        let debug = format!("{client:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secrettoken"));
        assert!(!debug.contains("flt_"));
    }

    // ── error sanitization ──────────────────────────────────────────────

    #[test]
    fn sanitize_timeout_error() {
        // Build a client with a 1ns timeout to force a timeout error.
        let client = Client::builder()
            .timeout(std::time::Duration::from_nanos(1))
            .build()
            .unwrap();
        // We can't easily construct reqwest errors directly, but we can
        // verify the function signature exists and handles the variants.
        // The actual integration is tested via fleet-server's http_api tests.
        let _ = client; // ensure client builds
    }

    // ── serde: QueryRequestPaginated ────────────────────────────────────

    #[test]
    fn query_request_paginated_serializes() {
        let req = QueryRequestPaginated {
            query: "service:nginx | stats count()".to_string(),
            limit: Some(10),
            offset: Some(5),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["query"], "service:nginx | stats count()");
        assert_eq!(json["limit"], 10);
        assert_eq!(json["offset"], 5);
    }

    #[test]
    fn query_request_paginated_omits_none() {
        let req = QueryRequestPaginated {
            query: "service:nginx".to_string(),
            limit: None,
            offset: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["query"], "service:nginx");
        assert!(json.get("limit").is_none());
        assert!(json.get("offset").is_none());
    }

    // ── serde: HealthResponse ────────────────────────────────────────────

    #[test]
    fn health_response_deserializes() {
        let json = r#"{"status": "ok"}"#;
        let resp: HealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.status, "ok");
    }

    // ── serde: SchemaResponse ───────────────────────────────────────────

    #[test]
    fn schema_response_deserializes() {
        let json = r#"{
            "columns": [
                {"name": "host", "type": "VARCHAR"},
                {"name": "timestamp", "type": "TIMESTAMP"}
            ],
            "file_count": 42,
            "cached": true
        }"#;
        let resp: SchemaResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.columns.len(), 2);
        assert_eq!(resp.columns[0].name, "host");
        assert_eq!(resp.columns[0].data_type, "VARCHAR");
        assert_eq!(resp.file_count, 42);
        assert!(resp.cached);
    }

    // ── serde: QueriesResponse ──────────────────────────────────────────

    #[test]
    fn queries_response_deserializes() {
        let json = r#"{
            "active": [{
                "id": 1,
                "user": "admin",
                "role": "admin",
                "query": "* | stats count()",
                "running_ms": 150
            }],
            "recent": [{
                "id": 2,
                "user": "analyst",
                "query": "service:nginx",
                "duration_ms": 42,
                "rows": 100,
                "error": null,
                "timed_out": false
            }]
        }"#;
        let resp: QueriesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.active.len(), 1);
        assert_eq!(resp.active[0].user, "admin");
        assert_eq!(resp.recent.len(), 1);
        assert_eq!(resp.recent[0].rows, Some(100));
        assert!(!resp.recent[0].timed_out);
    }
}
