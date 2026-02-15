//! HTTP client for communicating with fleetd.

use reqwest::Client;
use zeroize::Zeroizing;

use crate::error::ClientError;
use crate::types::{
    CancelResponse, DeleteSavedResponse, HealthResponse, HistoryResponse, ListSavedResponse,
    QueriesResponse, QueryResponse, SavedQueryResponse, SchemaResponse, ValidationResponse,
};
use crate::types::{
    CreateSavedRequest, ErrorResponse, ExportRequest, QueryRequestPaginated, UpdateSavedRequest,
    ValidateRequest,
};

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

        let resp = check_status(resp).await?;
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
        let body = ValidateRequest { query: dsl };
        let req = self.client.post(&url).json(&body);
        self.send_authenticated(req).await
    }

    /// Fetch query history with pagination.
    pub async fn history(
        &self,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<HistoryResponse, ClientError> {
        let url = self.endpoint("/api/v1/history");
        let mut req = self.client.get(&url);
        if let Some(l) = limit {
            req = req.query(&[("limit", l.to_string())]);
        }
        if let Some(o) = offset {
            req = req.query(&[("offset", o.to_string())]);
        }
        self.send_authenticated(req).await
    }

    /// List all saved queries for the authenticated user.
    pub async fn list_saved(&self) -> Result<ListSavedResponse, ClientError> {
        let url = self.endpoint("/api/v1/saved");
        let req = self.client.get(&url);
        self.send_authenticated(req).await
    }

    /// Create a new saved query.
    pub async fn create_saved(
        &self,
        name: &str,
        query: &str,
    ) -> Result<SavedQueryResponse, ClientError> {
        let url = self.endpoint("/api/v1/saved");
        let body = CreateSavedRequest { name, query };
        let req = self.client.post(&url).json(&body);
        self.send_authenticated(req).await
    }

    /// Update an existing saved query's content.
    pub async fn update_saved(
        &self,
        id: i64,
        query: &str,
    ) -> Result<SavedQueryResponse, ClientError> {
        let url = self.endpoint(&format!("/api/v1/saved/{id}"));
        let body = UpdateSavedRequest { query };
        let req = self.client.put(&url).json(&body);
        self.send_authenticated(req).await
    }

    /// Delete a saved query.
    pub async fn delete_saved(&self, id: i64) -> Result<DeleteSavedResponse, ClientError> {
        let url = self.endpoint(&format!("/api/v1/saved/{id}"));
        let req = self.client.delete(&url);
        self.send_authenticated(req).await
    }

    /// Export query results in the specified format.
    ///
    /// Returns raw bytes suitable for writing to a file.
    pub async fn export(
        &self,
        query: &str,
        format: &str,
        limit: Option<usize>,
    ) -> Result<Vec<u8>, ClientError> {
        let url = self.endpoint("/api/v1/export");
        let body = ExportRequest { query, limit };

        let resp = self
            .client
            .post(&url)
            .query(&[("format", format)])
            .header("Authorization", format!("Bearer {}", self.token.as_str()))
            .json(&body)
            .send()
            .await
            .map_err(sanitize_reqwest_error)?;

        let resp = check_status(resp).await?;
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| ClientError::Parse(e.to_string()))
    }

    /// Stream live query results via Server-Sent Events.
    ///
    /// Returns a response handle that can be used to read SSE events.
    /// The caller is responsible for parsing the SSE event stream.
    ///
    /// # Parameters
    /// - `query`: DSL query string to execute repeatedly
    /// - `interval_secs`: Interval between query executions (1-60 seconds)
    pub async fn stream(
        &self,
        query: &str,
        interval_secs: Option<u64>,
    ) -> Result<reqwest::Response, ClientError> {
        let url = self.endpoint("/api/v1/stream");

        let mut req = self.client.get(&url).query(&[("query", query)]);

        if let Some(interval) = interval_secs {
            req = req.query(&[("interval", interval.to_string())]);
        }

        let resp = req
            .header("Authorization", format!("Bearer {}", self.token.as_str()))
            .send()
            .await
            .map_err(sanitize_reqwest_error)?;

        let resp = check_status(resp).await?;
        Ok(resp)
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

        let resp = check_status(resp).await?;
        resp.json()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))
    }
}

/// Check HTTP response status and return a `Server` error on failure.
async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, ClientError> {
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let message = resp
            .json::<ErrorResponse>()
            .await
            .map_or_else(|_| "unknown error".into(), |e| e.error);
        return Err(ClientError::Server { status, message });
    }
    Ok(resp)
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
    } else if e.is_redirect() {
        ClientError::Network("unexpected redirect".into())
    } else if e.is_decode() {
        ClientError::Network("response decode error".into())
    } else if e.is_body() {
        ClientError::Network("request body error".into())
    } else {
        ClientError::Network("request failed".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::QueryRequestPaginated;

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
