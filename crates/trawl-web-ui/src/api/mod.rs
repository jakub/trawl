// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Typed helpers for calling the trawl-web proxy from the browser.
//!
//! All calls are same-origin (`/api/auth/...`, `/api/v1/...`) and rely on
//! the httpOnly session cookie being attached automatically by the
//! browser. Never touches `Authorization` — that's the proxy's job.

use gloo_net::http::Request;
use serde::{Deserialize, Serialize};
use trawl_api::{
    CatalogFieldResponse, ClearHistoryResponse, CreateSavedRequest, DeleteSavedResponse,
    DeleteScheduleResponse, ErrorResponse, ExportFormat, ExportRequest, HealthResponse,
    HistoryResponse, ListAllRunsResponse, ListReportRunsResponse, ListSavedResponse, QueryRequest,
    QueryResponse, RepinJobResponse, RepinRequest, RepinResponse, RepinStatusResponse,
    ReportRunResponse, ReportRunSummary, RunsStatsResponse, SavedQueryResponse, ScheduleResponse,
    ServiceSchemaResponse, SetScheduleRequest, UpdateSavedRequest,
};

use crate::repin_flow::{BoundCeilings, ConflictBody, classify_conflict};

/// Rows per page for the snapshot results table. Defined by the search
/// URL contract (the `page` parameter's offset check is part of it) and
/// re-exported here so request building keeps one path to it.
pub use crate::search_url::PAGE_SIZE;

/// Rows per page in both run browsers.
pub const RUNS_PAGE_SIZE: std::num::NonZeroUsize = std::num::NonZeroUsize::new(20).unwrap();

#[derive(Debug, Clone, thiserror::Error)]
pub enum ApiError {
    #[error("network: {0}")]
    Network(String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("server returned {0}")]
    Status(u16),

    /// A non-2xx whose body carried the server's own error envelope. The
    /// message is server-written and safe by construction (trawld's
    /// envelope never quotes DSL or generated SQL), and the repin modal
    /// renders it verbatim rather than inventing copy for a 400/403/503
    /// it cannot classify.
    #[error("{message}")]
    Server {
        /// The HTTP status the message came with.
        status: u16,
        /// The envelope's human-readable summary.
        message: String,
    },

    #[error("decode: {0}")]
    Decode(String),

    /// The client refused to send the request at all — nothing left the
    /// browser. The empty query is the case that matters: the server
    /// reads it as every row (ADR-0027), so a door that would post it
    /// answers this instead.
    #[error("{0}")]
    Refused(&'static str),
}

impl ApiError {
    /// The HTTP status this failure carries, when it carries one at all.
    ///
    /// `None` is not a status class: a network or decode failure means
    /// the request's fate is unknown, which is exactly what callers who
    /// branch on definitiveness (`repin_flow::is_pre_claim_failure`)
    /// have to tell apart from a server that answered.
    #[must_use]
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::Status(status) | Self::Server { status, .. } => Some(*status),
            // The one status this enum spells as a word rather than a
            // number.
            Self::Unauthorized => Some(401),
            // A refusal never reached the network, so its fate is not
            // unknown — but it carries no status either.
            Self::Network(_) | Self::Decode(_) | Self::Refused(_) => None,
        }
    }
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
#[allow(dead_code)] // the shell reads identity from `/api/auth/me`, not the login answer
pub struct LoginResponse {
    pub name: String,
    /// Names of every role the key holds (display only).
    pub roles: Vec<String>,
    /// Server-resolved trawl permissions — the gating currency (ADR-0006).
    pub permissions: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)] // `exp` is decoded off the wire; nothing renders an expiry countdown
pub struct MeResponse {
    pub name: String,
    /// Names of every role the key holds (display only).
    pub roles: Vec<String>,
    /// Server-resolved trawl permissions — the gating currency (ADR-0006).
    pub permissions: Vec<String>,
    pub exp: i64,
}

/// POST `/api/auth/login` with the given API key. Returns identity on
/// success, [`ApiError::Unauthorized`] on bad key.
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

/// GET `/api/auth/me` — read current session identity.
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

/// POST `/api/auth/logout` — clears the session cookie.
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
        200 | 503 => resp
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

/// DELETE /api/v1/history clears only the current session key's history.
pub async fn clear_history() -> Result<ClearHistoryResponse, ApiError> {
    let resp = Request::delete("/api/v1/history").send().await?;
    match resp.status() {
        200 => resp
            .json::<ClearHistoryResponse>()
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

/// GET `/api/v1/schema/field?name=` — one field's pin, one page of its
/// per-service observations (page with the response's `services_cursor`
/// as `after`), its retained conflict evidence, and the analyzer's
/// verdict when the pin is degraded.
///
/// A field with no pin answers 404, which surfaces here as
/// [`ApiError::Status(404)`] — the case file maps that to fleet-ui's
/// neutral `LoadState::Missing`, never to an error banner.
pub async fn catalog_field(
    name: &str,
    after: Option<&str>,
    limit: usize,
) -> Result<CatalogFieldResponse, ApiError> {
    // A catalog key is any ASCII-folded client JSON key — it can carry
    // `&`, `#`, `%` — and the cursor is opaque server text, so both go
    // through component encoding rather than into the URL raw.
    let mut url = format!("/api/v1/schema/field?name={}&limit={limit}", encode(name));
    if let Some(cursor) = after {
        url.push_str("&after=");
        url.push_str(&encode(cursor));
    }
    let resp = Request::get(&url).send().await?;
    match resp.status() {
        200 => resp
            .json::<CatalogFieldResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// What `POST /api/v1/schema/repin` answered. The HTTP status carries
/// the verdict (ADR-0011), so the call site branches on this rather than
/// on a single body type.
#[derive(Debug, Clone)]
pub enum RepinOutcome {
    /// 200 — the scan ran and stopped. The job is the plan.
    DryRun(RepinJobResponse),
    /// 202 — the rewrite is claimed and running detached. Poll
    /// [`repin_status`] for the rest of its life.
    Started(RepinJobResponse),
    /// 409 whose body decoded as a job — the scan projected values the
    /// target pin cannot keep and no force was passed. Terminal
    /// (`refused_needs_force`); the corpus is untouched.
    Refused(RepinJobResponse),
    /// 409 whose body decoded as the error envelope — the one-running
    /// slot is held by another job, install-wide.
    Busy(String),
}

/// POST `/api/v1/schema/repin` — plan (`dry_run`) or start a repin.
///
/// `SchemaWrite`-gated server-side, which is the only enforcement: this
/// call is issued exactly as written whatever the SPA believes about the
/// session's permissions.
///
/// The 409 is double-shaped — a refusal carries the plan, a held slot
/// carries the error envelope — and the two are told apart by decoding
/// the body ([`crate::repin_flow::classify_conflict`]), never by
/// matching on error text.
pub async fn repin(
    field: &str,
    to: &str,
    dry_run: bool,
    force: bool,
    ceilings: Option<BoundCeilings>,
) -> Result<RepinOutcome, ApiError> {
    let body = RepinRequest {
        // The exact name, never the sanitised display copy: the catalog
        // key is what the server folds and looks up.
        field: field.to_owned(),
        to: to.to_owned(),
        // The SPA offers physical targets only (see `repin_flow::REPIN_LADDER`),
        // and a dialect is meaningful for SEVERITY alone — the server 400s it
        // on anything else, so sending one here could only ever be a bug.
        dialect: None,
        dry_run,
        force,
        // A forced execution restates the pair the operator was shown, so
        // the bound the server enforces is the bound on screen. `None` is
        // every other rung of the ladder: the scans (which resolve their
        // own) and the unforced run (which accepts no loss at all).
        max_nulled_rows: ceilings.map(|c| c.max_nulled),
        max_ambiguous_rows: ceilings.map(|c| c.max_ambiguous),
    };
    let resp = Request::post("/api/v1/schema/repin")
        .header("content-type", "application/json")
        .body(serde_json::to_string(&body).map_err(|e| ApiError::Decode(e.to_string()))?)?
        .send()
        .await?;

    match resp.status() {
        200 => Ok(RepinOutcome::DryRun(repin_job(&resp).await?)),
        202 => Ok(RepinOutcome::Started(repin_job(&resp).await?)),
        409 => {
            let text = resp
                .text()
                .await
                .map_err(|e| ApiError::Decode(e.to_string()))?;
            Ok(match classify_conflict(&text) {
                ConflictBody::Plan(job) => RepinOutcome::Refused(*job),
                ConflictBody::Busy(msg) => RepinOutcome::Busy(msg),
            })
        }
        // Every other status — including 401/403 — keeps the server's
        // own message so the modal can show it verbatim. A repin refusal
        // is an operator-facing sentence ("not a pinned field", "repin
        // requires an ingest-enabled node"), and replacing it with a
        // status number would throw away the only actionable part.
        s => Err(server_error(&resp, s).await),
    }
}

/// GET `/api/v1/schema/repin/status` — the running job if any, else the
/// newest job of any status.
///
/// Install-wide and unfiltered by design (there is no `?id=`): the
/// caller matches `job.id` against the id it holds and treats anything
/// else as the slot having moved on.
pub async fn repin_status() -> Result<RepinStatusResponse, ApiError> {
    let resp = Request::get("/api/v1/schema/repin/status").send().await?;
    match resp.status() {
        200 => resp
            .json::<RepinStatusResponse>()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

/// The job row out of a `RepinResponse` body.
async fn repin_job(resp: &gloo_net::http::Response) -> Result<RepinJobResponse, ApiError> {
    resp.json::<RepinResponse>()
        .await
        .map(|r| r.job)
        .map_err(|e| ApiError::Decode(e.to_string()))
}

/// A non-2xx as [`ApiError::Server`] when the body carries the error
/// envelope, else the bare status.
async fn server_error(resp: &gloo_net::http::Response, status: u16) -> ApiError {
    let Ok(body) = resp.text().await else {
        return ApiError::Status(status);
    };
    serde_json::from_str::<ErrorResponse>(&body).map_or(ApiError::Status(status), |env| {
        ApiError::Server {
            status,
            message: env.error.message,
        }
    })
}

/// One URL query-parameter value.
fn encode(raw: &str) -> String {
    js_sys::encode_uri_component(raw)
        .as_string()
        .unwrap_or_else(|| raw.to_string())
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
///
/// The PUT takes the schedule's whole shape, so `window` and `lag` are
/// arguments rather than constants: omitting a window is not "leave it
/// alone", it is query mode. The pair comes from
/// `schedule_edit::WindowDraft::to_request`, which is what the form is
/// showing.
///
/// The server refuses several window and lag combinations by name, so a
/// non-2xx carries its envelope message through rather than collapsing
/// to a bare status the operator cannot act on.
pub async fn set_schedule(
    saved_id: i64,
    interval: &str,
    max_runs: Option<u64>,
    enabled: bool,
    window: Option<&str>,
    lag: Option<&str>,
) -> Result<ScheduleResponse, ApiError> {
    let body = SetScheduleRequest {
        interval: interval.to_owned(),
        max_runs,
        enabled,
        window: window.map(str::to_owned),
        lag: lag.map(str::to_owned),
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
        s => Err(server_error(&resp, s).await),
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
///
/// An empty query never leaves the browser: the server's emitter turns
/// it into `SELECT *` with no WHERE, so an export posted with the
/// malformed gate's blanked query would write the whole corpus to a file
/// while the page says the link was refused (ADR-0027). The modal above
/// refuses it too; this is the door nothing gets past.
pub async fn export(
    query: &str,
    format: &ExportFormat,
    limit: Option<usize>,
) -> Result<(Vec<u8>, String), ApiError> {
    if !crate::search_url::is_executable(query) {
        return Err(ApiError::Refused(crate::search_url::EMPTY_QUERY_REFUSAL));
    }
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

/// Read capacity for the current admin session.
pub async fn stats() -> Result<trawl_api::StatsResponse, ApiError> {
    telemetry_get("/api/v1/stats").await
}

/// Bootstrap the shell's dashboard before the first stream snapshot.
pub async fn dashboard() -> Result<trawl_api::DashboardSnapshot, ApiError> {
    telemetry_get("/api/v1/dashboard").await
}

/// Read active and recent queries visible to this session.
pub async fn queries() -> Result<trawl_api::QueriesResponse, ApiError> {
    telemetry_get("/api/v1/queries").await
}

async fn telemetry_get<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, ApiError> {
    let resp = Request::get(url).send().await?;
    match resp.status() {
        200 => resp
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        status => Err(server_error(&resp, status).await),
    }
}

/// Request cancellation without inferring success from an HTTP status alone.
pub async fn cancel_query(id: u64) -> Result<trawl_api::CancelResponse, ApiError> {
    let resp = Request::delete(&format!("/api/v1/queries/{id}"))
        .send()
        .await?;
    match resp.status() {
        200 => resp
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        status => Err(server_error(&resp, status).await),
    }
}
