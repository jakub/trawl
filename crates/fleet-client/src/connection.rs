//! HTTP client for communicating with fleetd.

use fleet_engine::value::{Column, QueryResult, Value};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::error::ClientError;

/// HTTP client for the fleet daemon API.
#[derive(Clone)]
pub struct HttpClient {
    base_url: String,
    token: String,
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
    /// Create a new client targeting the given daemon URL with an API key.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self, ClientError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(sanitize_reqwest_error)?;
        Ok(Self {
            base_url: base_url.into(),
            token: token.into(),
            client,
        })
    }

    /// Create a client that accepts self-signed / invalid TLS certificates.
    ///
    /// Use this for development or when connecting to a daemon with an
    /// auto-generated self-signed cert.
    pub fn new_insecure(
        base_url: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<Self, ClientError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(120))
            .danger_accept_invalid_certs(true)
            .build()
            .map_err(sanitize_reqwest_error)?;
        Ok(Self {
            base_url: base_url.into(),
            token: token.into(),
            client,
        })
    }

    /// Create a client with a pre-configured `reqwest::Client`.
    pub fn with_client(
        base_url: impl Into<String>,
        token: impl Into<String>,
        client: Client,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
            client,
        }
    }

    /// Build a full URL for an API endpoint, normalizing trailing slashes.
    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    /// Execute a DSL query against the daemon.
    pub async fn query(&self, dsl: &str) -> Result<QueryResult, ClientError> {
        let url = self.endpoint("/api/v1/query");
        let body = QueryRequest {
            query: dsl.to_owned(),
        };
        let req = self.client.post(&url).json(&body);
        let json: QueryResponse = self.send_authenticated(req).await?;
        Ok(json.into())
    }

    /// Check daemon health (unauthenticated).
    pub async fn health(&self) -> Result<serde_json::Value, ClientError> {
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

    /// Send an authenticated request, check for errors, and deserialize the response.
    async fn send_authenticated<T: serde::de::DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<T, ClientError> {
        let resp = req
            .header("Authorization", format!("Bearer {}", self.token))
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

#[derive(Serialize)]
struct QueryRequest {
    query: String,
}

#[derive(Deserialize)]
struct QueryResponse {
    columns: Vec<String>,
    rows: Vec<Vec<serde_json::Value>>,
    #[allow(dead_code)]
    row_count: usize,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
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

impl From<QueryResponse> for QueryResult {
    fn from(resp: QueryResponse) -> Self {
        Self {
            columns: resp
                .columns
                .into_iter()
                .map(|name| Column { name })
                .collect(),
            rows: resp
                .rows
                .into_iter()
                .map(|row| row.into_iter().map(json_to_value).collect())
                .collect(),
        }
    }
}

fn json_to_value(val: serde_json::Value) -> Value {
    match val {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Boolean(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                Value::String(n.to_string())
            }
        }
        serde_json::Value::String(s) => Value::String(s),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            Value::String(val.to_string())
        }
    }
}
