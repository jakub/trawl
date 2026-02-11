//! HTTP client for communicating with fleetd.

use fleet_engine::value::{Column, QueryResult, Value};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::error::ClientError;

/// HTTP client for the fleet daemon API.
#[derive(Debug, Clone)]
pub struct HttpClient {
    base_url: String,
    token: String,
    client: Client,
}

impl HttpClient {
    /// Create a new client targeting the given daemon URL with an API key.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
            client: Client::new(),
        }
    }

    /// Execute a DSL query against the daemon.
    pub async fn query(&self, dsl: &str) -> Result<QueryResult, ClientError> {
        let url = format!("{}/api/v1/query", self.base_url);

        let body = QueryRequest {
            query: dsl.to_owned(),
        };

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&body)
            .send()
            .await
            .map_err(|e| ClientError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let message = resp
                .json::<ErrorResponse>()
                .await
                .map_or_else(|_| "unknown error".into(), |e| e.error);
            return Err(ClientError::Server { status, message });
        }

        let json: QueryResponse = resp
            .json()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))?;

        Ok(json.into())
    }

    /// Check daemon health (unauthenticated).
    pub async fn health(&self) -> Result<serde_json::Value, ClientError> {
        let url = format!("{}/api/v1/health", self.base_url);

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| ClientError::Network(e.to_string()))?;

        resp.json()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))
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
