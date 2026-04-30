// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Typed helpers for calling the trawl-web proxy from the browser.
//!
//! All calls are same-origin (`/me`, `/login`, `/logout`, `/api/v1/...`)
//! and rely on the httpOnly session cookie being attached automatically
//! by the browser. Never touches `Authorization` — that's the proxy's job.

use gloo_net::http::Request;
use serde::{Deserialize, Serialize};
use trawl_api::{
    CreateSavedRequest, DeleteSavedResponse, DeleteScheduleResponse, ExportFormat, ExportRequest,
    HealthResponse, HistoryResponse, ListAllRunsResponse, ListReportRunsResponse,
    ListSavedResponse, QueryRequest, QueryResponse, ReportRunResponse, ReportRunSummary,
    RunsStatsResponse, SavedQueryResponse, ScheduleResponse, ServiceSchemaResponse,
    SetScheduleRequest, UpdateSavedRequest,
};

/// Rows per page for the snapshot results table.
pub const PAGE_SIZE: usize = 50;

#[derive(Debug, Clone, thiserror::Error)]
pub enum ApiError {
    #[error("network: {0}")]
    Network(String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("server returned {0}")]
    Status(u16),

    #[error("decode: {0}")]
    Decode(String),
}

impl From<gloo_net::Error> for ApiError {
    fn from(e: gloo_net::Error) -> Self {
        Self::Network(e.to_string())
    }
}

#[derive(Debug, Serialize)]
pub struct LoginRequest<'a> {
    pub api_key: &'a str,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)] // fields consumed by later commits (search page, admin gating)
pub struct LoginResponse {
    pub name: String,
    pub role: String,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)] // `exp` is for client-side expiry countdowns in a later commit
pub struct MeResponse {
    pub name: String,
    pub role: String,
    pub exp: i64,
}

/// POST /login with the given API key. Returns identity on success,
/// [`ApiError::Unauthorized`] on bad key.
pub async fn login(api_key: &str) -> Result<LoginResponse, ApiError> {
    let body = LoginRequest { api_key };
    let resp = Request::post("/api/auth/login")
        .header("content-type", "application/json")
        .body(serde_json::to_string(&body).map_err(|e| ApiError::Decode(e.to_string()))?)?
        .send()
        .await?;

    match resp.status() {
        200 => resp
            .json::<LoginResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// GET /me — read current session identity.
pub async fn me() -> Result<MeResponse, ApiError> {
    let resp = Request::get("/api/auth/me").send().await?;
    match resp.status() {
        200 => resp
            .json::<MeResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// POST /logout — clears the session cookie.
pub async fn logout() -> Result<(), ApiError> {
    let resp = Request::post("/api/auth/logout").send().await?;
    match resp.status() {
        204 | 200 => Ok(()),
        s => Err(ApiError::Status(s)),
    }
}

/// GET /api/v1/health — unauthenticated health/version probe from trawld.
pub async fn health() -> Result<HealthResponse, ApiError> {
    let resp = Request::get("/api/v1/health").send().await?;
    match resp.status() {
        200 => resp
            .json::<HealthResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        s => Err(ApiError::Status(s)),
    }
}

/// GET /api/v1/history — paginated query history for the current session.
pub async fn history(limit: usize, offset: usize) -> Result<HistoryResponse, ApiError> {
    let url = format!("/api/v1/history?limit={limit}&offset={offset}");
    let resp = Request::get(&url).send().await?;
    match resp.status() {
        200 => resp
            .json::<HistoryResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// POST /api/v1/saved — create a named saved query ("net").
pub async fn create_saved(name: &str, query: &str) -> Result<SavedQueryResponse, ApiError> {
    let body = CreateSavedRequest {
        name: name.to_owned(),
        query: query.to_owned(),
    };
    let resp = Request::post("/api/v1/saved")
        .header("content-type", "application/json")
        .body(serde_json::to_string(&body).map_err(|e| ApiError::Decode(e.to_string()))?)?
        .send()
        .await?;
    match resp.status() {
        200 | 201 => resp
            .json::<SavedQueryResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// GET /api/v1/schema/services — rich per-service metadata (columns,
/// stats, daily event counts). Cached server-side; see
/// `schema_refresh.rs`. Used by the Schema page to render per-service
/// cards and their drawer inspectors.
pub async fn schema_services() -> Result<ServiceSchemaResponse, ApiError> {
    let resp = Request::get("/api/v1/schema/services").send().await?;
    match resp.status() {
        200 => resp
            .json::<ServiceSchemaResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// POST /api/v1/query — execute a DSL query with fixed [`PAGE_SIZE`] paging.
pub async fn query(q: &str, page: usize) -> Result<QueryResponse, ApiError> {
    let body = QueryRequest {
        query: q.to_owned(),
        limit: Some(PAGE_SIZE),
        offset: Some(page * PAGE_SIZE),
        timezone: None,
    };
    let resp = Request::post("/api/v1/query")
        .header("content-type", "application/json")
        .body(serde_json::to_string(&body).map_err(|e| ApiError::Decode(e.to_string()))?)?
        .send()
        .await?;

    match resp.status() {
        200 => resp
            .json::<QueryResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// GET /api/v1/saved — list all saved queries ("nets") for the session.
pub async fn list_saved() -> Result<ListSavedResponse, ApiError> {
    let resp = Request::get("/api/v1/saved").send().await?;
    match resp.status() {
        200 => resp
            .json::<ListSavedResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// PUT /api/v1/saved/{id} — update a saved query's DSL.
pub async fn update_saved(id: i64, query: &str) -> Result<SavedQueryResponse, ApiError> {
    let body = UpdateSavedRequest {
        query: query.to_owned(),
        name: None,
    };
    let resp = Request::put(&format!("/api/v1/saved/{id}"))
        .header("content-type", "application/json")
        .body(serde_json::to_string(&body).map_err(|e| ApiError::Decode(e.to_string()))?)?
        .send()
        .await?;
    match resp.status() {
        200 => resp
            .json::<SavedQueryResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// DELETE /api/v1/saved/{id} — delete a saved query.
pub async fn delete_saved(id: i64) -> Result<DeleteSavedResponse, ApiError> {
    let resp = Request::delete(&format!("/api/v1/saved/{id}"))
        .send()
        .await?;
    match resp.status() {
        200 => resp
            .json::<DeleteSavedResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// PUT /api/v1/saved/{id}/schedule — create or update a schedule.
pub async fn set_schedule(
    saved_id: i64,
    interval: &str,
    max_runs: Option<u64>,
    enabled: bool,
) -> Result<ScheduleResponse, ApiError> {
    let body = SetScheduleRequest {
        interval: interval.to_owned(),
        max_runs,
        enabled,
    };
    let resp = Request::put(&format!("/api/v1/saved/{saved_id}/schedule"))
        .header("content-type", "application/json")
        .body(serde_json::to_string(&body).map_err(|e| ApiError::Decode(e.to_string()))?)?
        .send()
        .await?;
    match resp.status() {
        200 => resp
            .json::<ScheduleResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// DELETE /api/v1/saved/{id}/schedule — remove a schedule.
pub async fn delete_schedule(saved_id: i64) -> Result<DeleteScheduleResponse, ApiError> {
    let resp = Request::delete(&format!("/api/v1/saved/{saved_id}/schedule"))
        .send()
        .await?;
    match resp.status() {
        200 => resp
            .json::<DeleteScheduleResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// GET /api/v1/saved/{id}/runs — paginated runs for a saved query.
pub async fn list_runs(
    saved_id: i64,
    limit: usize,
    offset: usize,
) -> Result<ListReportRunsResponse, ApiError> {
    let url = format!("/api/v1/saved/{saved_id}/runs?limit={limit}&offset={offset}");
    let resp = Request::get(&url).send().await?;
    match resp.status() {
        200 => resp
            .json::<ListReportRunsResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// GET `/api/v1/saved/{id}/runs/{run_id}` — single run with result data.
pub async fn get_run(saved_id: i64, run_id: i64) -> Result<ReportRunResponse, ApiError> {
    let url = format!("/api/v1/saved/{saved_id}/runs/{run_id}");
    let resp = Request::get(&url).send().await?;
    match resp.status() {
        200 => resp
            .json::<ReportRunResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// GET /api/v1/runs — paginated runs across all saved queries.
pub async fn list_all_runs(limit: usize, offset: usize) -> Result<ListAllRunsResponse, ApiError> {
    let url = format!("/api/v1/runs?limit={limit}&offset={offset}");
    let resp = Request::get(&url).send().await?;
    match resp.status() {
        200 => resp
            .json::<ListAllRunsResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// POST /api/v1/export?format={fmt} — export query results as binary.
///
/// Returns raw bytes and a suggested filename from `Content-Disposition`.
pub async fn export(
    query: &str,
    format: &ExportFormat,
    limit: Option<usize>,
) -> Result<(Vec<u8>, String), ApiError> {
    let body = ExportRequest {
        query: query.to_owned(),
        limit,
    };
    let url = format!("/api/v1/export?format={format}");
    let resp = Request::post(&url)
        .header("content-type", "application/json")
        .body(serde_json::to_string(&body).map_err(|e| ApiError::Decode(e.to_string()))?)?
        .send()
        .await?;

    match resp.status() {
        200 => {
            let filename = parse_content_disposition(
                resp.headers().get("content-disposition").as_deref(),
                format,
            );
            let bytes = resp
                .binary()
                .await
                .map_err(|e| ApiError::Decode(e.to_string()))?;
            Ok((bytes, filename))
        }
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// PUT /api/v1/saved/{id} — update a saved query's DSL and optionally its name.
pub async fn update_saved_full(
    id: i64,
    query: &str,
    name: Option<&str>,
) -> Result<SavedQueryResponse, ApiError> {
    let body = UpdateSavedRequest {
        query: query.to_owned(),
        name: name.map(str::to_owned),
    };
    let resp = Request::put(&format!("/api/v1/saved/{id}"))
        .header("content-type", "application/json")
        .body(serde_json::to_string(&body).map_err(|e| ApiError::Decode(e.to_string()))?)?
        .send()
        .await?;
    match resp.status() {
        200 => resp
            .json::<SavedQueryResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// POST /api/v1/saved/{id}/run — trigger an immediate report run.
pub async fn trigger_run(saved_id: i64) -> Result<ReportRunSummary, ApiError> {
    let resp = Request::post(&format!("/api/v1/saved/{saved_id}/run"))
        .send()
        .await?;
    match resp.status() {
        200 => resp
            .json::<ReportRunSummary>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// GET /api/v1/runs/stats — aggregate run statistics.
pub async fn runs_stats() -> Result<RunsStatsResponse, ApiError> {
    let resp = Request::get("/api/v1/runs/stats").send().await?;
    match resp.status() {
        200 => resp
            .json::<RunsStatsResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

fn parse_content_disposition(header: Option<&str>, format: &ExportFormat) -> String {
    if let Some(val) = header
        && let Some(start) = val.find("filename=\"")
    {
        let rest = &val[start + 10..];
        if let Some(end) = rest.find('"') {
            return rest[..end].to_string();
        }
    }
    match format {
        ExportFormat::Csv => "export.csv",
        ExportFormat::Json => "export.ndjson",
        ExportFormat::Parquet => "export.parquet",
    }
    .to_string()
}

pub mod intel;
