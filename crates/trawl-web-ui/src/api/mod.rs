// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Typed helpers for calling the trawl-web proxy from the browser.
//!
//! All calls are same-origin (`/api/auth/...`, `/api/v1/...`) and rely on
//! the httpOnly session cookie being attached automatically by the
//! browser. Never touches `Authorization` — that's the proxy's job.
//!
//! Every failure goes to the one classifier (ADR-0066).

use gloo_net::http::{Request, RequestBuilder, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use trawl_api::{
    CatalogFieldResponse, ClearHistoryResponse, CreateSavedRequest, DeleteSavedResponse,
    DeleteScheduleResponse, ExportFormat, ExportRequest, HealthResponse, HistoryResponse,
    ListAllRunsResponse, ListReportRunsResponse, ListSavedResponse, QueryRequest, QueryResponse,
    RepinJobResponse, RepinRequest, RepinStatusResponse, ReportRunResponse, ReportRunSummary,
    RunsSortDir, RunsSortKey, RunsStatsResponse, SavedQueryResponse, ScheduleResponse,
    ServiceSchemaResponse, SetScheduleRequest, UpdateSavedRequest,
};

use trawl_api::investigations::{
    AppendEntry, AppendResponse, Attach, Investigation, InvestigationId, InvestigationList,
    InvestigationRead, InvestigationStatus, PatchInvestigation, PatchResponse,
};
use trawl_api::results::{CaptureRequest, CaptureResponse, CaptureWindow, ResultPage};

use crate::evidence_queue::{Paced, retry_after_ms};
use crate::fetch_plan::FetchPlan;
use crate::investigation_live::{WriteFailure, write_refusal};
use crate::query_merge::CaptureParts;
use crate::repin_flow::BoundCeilings;
use crate::response_policy;
use crate::send_investigation::{SendFailure, send_refusal};

pub use crate::search_url::PAGE_SIZE;

pub const RUNS_PAGE_SIZE: std::num::NonZeroUsize = std::num::NonZeroUsize::new(20).unwrap();

pub use crate::api_error::ApiError;
pub use crate::repin_flow::RepinOutcome;

impl From<gloo_net::Error> for ApiError {
    fn from(e: gloo_net::Error) -> Self {
        Self::Network(e.to_string())
    }
}

struct Answer {
    status: u16,
    body: Option<String>,
}

async fn send(request: Request) -> Result<Response, ApiError> {
    Ok(request.send().await?)
}

async fn read(resp: &Response) -> Option<String> {
    resp.text().await.ok()
}

async fn exchange(request: Request) -> Result<Answer, ApiError> {
    let resp = send(request).await?;
    Ok(Answer {
        status: resp.status(),
        body: read(&resp).await,
    })
}

async fn ordinary<T: DeserializeOwned>(request: Request, ok: &[u16]) -> Result<T, ApiError> {
    let Answer { status, body } = exchange(request).await?;
    response_policy::answer(status, body.as_deref(), ok)
}

async fn paced<T: DeserializeOwned>(request: Request) -> Paced<Result<T, ApiError>> {
    let resp = match send(request).await {
        Ok(resp) => resp,
        Err(e) => return Paced::Answered(Err(e)),
    };
    let status = resp.status();
    match response_policy::paced(
        status,
        resp.headers().get("retry-after").as_deref(),
        js_sys::Date::now(),
        http_date_ms,
    ) {
        Paced::Throttled { retry_after_ms } => Paced::Throttled { retry_after_ms },
        Paced::Answered(()) => Paced::Answered(response_policy::answer(
            status,
            read(&resp).await.as_deref(),
            &[200],
        )),
    }
}

fn http_date_ms(date: &str) -> Option<f64> {
    Some(js_sys::Date::parse(date)).filter(|ms| !ms.is_nan())
}

fn with_json<B: Serialize>(builder: RequestBuilder, body: &B) -> Result<Request, ApiError> {
    let json = serde_json::to_string(body).map_err(|e| ApiError::Decode(e.to_string()))?;
    Ok(builder
        .header("content-type", "application/json")
        .body(json)?)
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

pub async fn login(api_key: &str) -> Result<LoginResponse, ApiError> {
    let request = with_json(Request::post("/api/auth/login"), &LoginRequest { api_key })?;
    ordinary(request, &[200]).await
}

pub async fn me() -> Result<MeResponse, ApiError> {
    ordinary(Request::get("/api/auth/me").build()?, &[200]).await
}

pub async fn logout() -> Result<(), ApiError> {
    let Answer { status, body } = exchange(Request::post("/api/auth/logout").build()?).await?;
    response_policy::logout(status, body.as_deref())
}

/// GET /api/v1/health — health/version probe. trawld serves it without
/// auth, but the browser reaches it through the trawl-web `/api/v1/*` proxy,
/// which requires a session; `/healthz` is the public probe.
pub async fn health() -> Result<HealthResponse, ApiError> {
    let Answer { status, body } = exchange(Request::get("/api/v1/health").build()?).await?;
    response_policy::health(status, body.as_deref())
}

pub async fn history(
    limit: usize,
    offset: usize,
    filter: &str,
) -> Result<HistoryResponse, ApiError> {
    let mut url = format!("/api/v1/history?limit={limit}&offset={offset}");
    if !filter.is_empty() {
        url.push_str("&filter=");
        url.push_str(&crate::search_url::percent_encode(filter));
    }
    ordinary(Request::get(&url).build()?, &[200]).await
}

/// DELETE /api/v1/history clears only the current session key's history.
pub async fn clear_history() -> Result<ClearHistoryResponse, ApiError> {
    ordinary(Request::delete("/api/v1/history").build()?, &[200]).await
}

pub async fn create_saved(name: &str, query: &str) -> Result<SavedQueryResponse, ApiError> {
    let body = CreateSavedRequest {
        name: name.to_owned(),
        query: query.to_owned(),
    };
    ordinary(
        with_json(Request::post("/api/v1/saved"), &body)?,
        &[200, 201],
    )
    .await
}

pub async fn schema_services() -> Result<ServiceSchemaResponse, ApiError> {
    ordinary(Request::get("/api/v1/schema/services").build()?, &[200]).await
}

/// GET `/api/v1/schema/field?name=` — one field's pin, one page of its
/// per-service observations (page with the response's `services_cursor`
/// as `after`), its retained conflict evidence, and the analyzer's
/// verdict when the pin is degraded.
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
    ordinary(Request::get(&url).build()?, &[200]).await
}

/// `SchemaWrite`-gated server-side, which is the only enforcement: this
/// call is issued exactly as written whatever the SPA believes about the
/// session's permissions.
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
    let request = with_json(Request::post("/api/v1/schema/repin"), &body)?;
    let Answer { status, body } = exchange(request).await?;
    response_policy::repin(status, body.as_deref())
}

/// GET `/api/v1/schema/repin/status` — the running job if any, else the
/// newest job of any status.
///
/// Install-wide and unfiltered: this is the read for a job nobody here
/// holds the id of. A job this page started or adopted is followed by
/// its id instead ([`repin_job_status`]).
pub async fn repin_status() -> Result<RepinStatusResponse, ApiError> {
    ordinary(Request::get("/api/v1/schema/repin/status").build()?, &[200]).await
}

/// GET `/api/v1/schema/repin/status?id=` — job `id`, whatever has run
/// since it was claimed.
///
/// Gated server-side on `schema_write` or `schema_read`, so the session
/// that started a job can always follow it.
pub async fn repin_job_status(id: i64) -> Result<Option<RepinJobResponse>, ApiError> {
    let request = Request::get(&format!("/api/v1/schema/repin/status?id={id}")).build()?;
    let Answer { status, body } = exchange(request).await?;
    response_policy::repin_job_status(status, body.as_deref())
}

fn encode(raw: &str) -> String {
    js_sys::encode_uri_component(raw)
        .as_string()
        .unwrap_or_else(|| raw.to_string())
}

/// The plan owns the arithmetic (ADR-0037): this transport does none of
/// its own.
pub async fn query(q: &str, plan: FetchPlan) -> Result<QueryResponse, ApiError> {
    let (limit, offset) = plan.window();
    let body = QueryRequest {
        query: q.to_owned(),
        limit: Some(limit),
        offset: Some(offset),
        histogram: matches!(plan, FetchPlan::Page { .. }),
    };
    ordinary(with_json(Request::post("/api/v1/query"), &body)?, &[200]).await
}

pub async fn list_saved() -> Result<ListSavedResponse, ApiError> {
    ordinary(Request::get("/api/v1/saved").build()?, &[200]).await
}

pub async fn update_saved(id: i64, query: &str) -> Result<SavedQueryResponse, ApiError> {
    let body = UpdateSavedRequest {
        query: query.to_owned(),
        name: None,
    };
    ordinary(
        with_json(Request::put(&format!("/api/v1/saved/{id}")), &body)?,
        &[200],
    )
    .await
}

pub async fn delete_saved(id: i64) -> Result<DeleteSavedResponse, ApiError> {
    ordinary(
        Request::delete(&format!("/api/v1/saved/{id}")).build()?,
        &[200],
    )
    .await
}

/// The PUT takes the schedule's whole shape, so `window` and `lag` are
/// arguments rather than constants: omitting a window is not "leave it
/// alone", it is query mode. The pair comes from
/// `schedule_edit::WindowDraft::to_request`, which is what the form is
/// showing.
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
    let builder = Request::put(&format!("/api/v1/saved/{saved_id}/schedule"));
    ordinary(with_json(builder, &body)?, &[200]).await
}

pub async fn delete_schedule(saved_id: i64) -> Result<DeleteScheduleResponse, ApiError> {
    let request = Request::delete(&format!("/api/v1/saved/{saved_id}/schedule")).build()?;
    ordinary(request, &[200]).await
}

pub async fn list_runs(
    saved_id: i64,
    limit: usize,
    offset: usize,
) -> Result<ListReportRunsResponse, ApiError> {
    let url = format!("/api/v1/saved/{saved_id}/runs?limit={limit}&offset={offset}");
    ordinary(Request::get(&url).build()?, &[200]).await
}

/// Needs `query` beside `saved_query` (ADR-0018), so a session without
/// `query` never builds the preview that calls it.
pub async fn get_run(saved_id: i64, run_id: i64) -> Result<ReportRunResponse, ApiError> {
    let url = format!("/api/v1/saved/{saved_id}/runs/{run_id}");
    ordinary(Request::get(&url).build()?, &[200]).await
}

pub async fn list_all_runs(
    limit: usize,
    offset: usize,
    key: RunsSortKey,
    dir: RunsSortDir,
) -> Result<ListAllRunsResponse, ApiError> {
    let url = format!(
        "/api/v1/runs?limit={limit}&offset={offset}&sort={}&dir={}",
        key.as_str(),
        dir.as_str()
    );
    ordinary(Request::get(&url).build()?, &[200]).await
}

/// An empty query never leaves the browser: the server's emitter turns
/// it into `SELECT *` with no WHERE (ADR-0027).
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
    let resp = send(with_json(Request::post(&url), &body)?).await?;
    let filename =
        parse_content_disposition(resp.headers().get("content-disposition").as_deref(), format);
    let bytes = response_policy::export(resp.status(), resp.binary().await.ok())?;
    Ok((bytes, filename))
}

pub async fn update_saved_full(
    id: i64,
    query: &str,
    name: Option<&str>,
) -> Result<SavedQueryResponse, ApiError> {
    let body = UpdateSavedRequest {
        query: query.to_owned(),
        name: name.map(str::to_owned),
    };
    ordinary(
        with_json(Request::put(&format!("/api/v1/saved/{id}")), &body)?,
        &[200],
    )
    .await
}

/// POST /api/v1/saved/{id}/run — Run now: fire the schedule's next
/// window early. The answer is the claimed run, window included, so the
/// caller can state the bounds the server chose rather than guess them.
pub async fn trigger_run(saved_id: i64) -> Result<ReportRunSummary, ApiError> {
    let request = Request::post(&format!("/api/v1/saved/{saved_id}/run")).build()?;
    ordinary(request, &[200]).await
}

pub async fn runs_stats() -> Result<RunsStatsResponse, ApiError> {
    ordinary(Request::get("/api/v1/runs/stats").build()?, &[200]).await
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

pub async fn capabilities() -> Result<trawl_api::capabilities::CapabilitiesResponse, ApiError> {
    ordinary(Request::get("/api/v1/capabilities").build()?, &[200]).await
}

pub async fn list_investigations() -> Result<InvestigationList, ApiError> {
    ordinary(Request::get("/api/v1/investigations").build()?, &[200]).await
}

pub async fn read_investigation(
    id: &InvestigationId,
    since: u64,
    limit: usize,
    signal: Option<&web_sys::AbortSignal>,
) -> Paced<Result<InvestigationRead, ApiError>> {
    let url = format!("/api/v1/investigations/{id}?since={since}&limit={limit}");
    match Request::get(&url).abort_signal(signal).build() {
        Ok(request) => paced(request).await,
        Err(e) => Paced::Answered(Err(e.into())),
    }
}

pub async fn append_keep(
    id: &InvestigationId,
    result_id: &str,
    row: u64,
    reason: &str,
) -> Result<AppendResponse, WriteFailure> {
    let body = AppendEntry::Keep {
        result_id: result_id.to_owned(),
        rows: vec![row],
        reason: reason.to_owned(),
        seen_seq: None,
    };
    let request = Request::post(&format!("/api/v1/investigations/{id}/entries"))
        .header("content-type", "application/json")
        .body(serde_json::to_string(&body).map_err(|_| WriteFailure::Unknown)?)
        .map_err(|_| WriteFailure::Unknown)?;
    let resp = request.send().await.map_err(|_| WriteFailure::Unknown)?;
    match resp.status() {
        200 | 201 => resp.json().await.map_err(|_| WriteFailure::Unknown),
        status => Err(write_refusal(
            status,
            &resp.text().await.unwrap_or_default(),
        )),
    }
}

pub async fn patch_status(
    id: &InvestigationId,
    expected_seq: u64,
    status: InvestigationStatus,
) -> Result<Investigation, WriteFailure> {
    let body = PatchInvestigation {
        expected_seq,
        seen_seq: None,
        title: None,
        question: None,
        status: Some(status),
    };
    let request = Request::patch(&format!("/api/v1/investigations/{id}"))
        .header("content-type", "application/json")
        .body(serde_json::to_string(&body).map_err(|_| WriteFailure::Unknown)?)
        .map_err(|_| WriteFailure::Unknown)?;
    let resp = request.send().await.map_err(|_| WriteFailure::Unknown)?;
    match resp.status() {
        200 => resp
            .json::<PatchResponse>()
            .await
            .map(|answer| answer.investigation)
            .map_err(|_| WriteFailure::Unknown),
        status => Err(write_refusal(
            status,
            &resp.text().await.unwrap_or_default(),
        )),
    }
}

pub async fn capture_attached(
    parts: &CaptureParts,
    investigation: &InvestigationId,
    purpose: Option<&str>,
) -> Result<CaptureResponse, SendFailure> {
    let body = CaptureRequest {
        query: parts.query.clone(),
        window: parts.window.as_ref().map(|window| CaptureWindow {
            earliest: Some(window.earliest.clone()),
            latest: Some(window.latest.clone()),
            last: None,
        }),
        investigation: Some(Attach {
            id: investigation.clone(),
            seen_seq: None,
            purpose: purpose.map(str::to_owned),
        }),
    };
    let request = Request::post("/api/v1/results")
        .header("content-type", "application/json")
        .body(serde_json::to_string(&body).map_err(|_| SendFailure::Unknown)?)
        .map_err(|_| SendFailure::Unknown)?;
    let resp = request.send().await.map_err(|_| SendFailure::Unknown)?;
    match resp.status() {
        200 | 201 => resp.json().await.map_err(|_| SendFailure::Unknown),
        status => {
            let pause = retry_after_ms(
                resp.headers().get("retry-after").as_deref(),
                js_sys::Date::now(),
                http_date_ms,
            );
            Err(send_refusal(
                status,
                &resp.text().await.unwrap_or_default(),
                pause,
            ))
        }
    }
}

pub async fn result_page(
    id: &str,
    offset: u64,
    limit: Option<u64>,
    signal: Option<&web_sys::AbortSignal>,
) -> Paced<Result<ResultPage, ApiError>> {
    if !crate::investigation_log::is_result_id(id) {
        return Paced::Answered(Err(ApiError::Refused("That is not a captured result id.")));
    }
    let url = match limit {
        Some(limit) => format!("/api/v1/results/{id}?offset={offset}&limit={limit}"),
        None => format!("/api/v1/results/{id}?offset={offset}"),
    };
    match Request::get(&url).abort_signal(signal).build() {
        Ok(request) => paced(request).await,
        Err(e) => Paced::Answered(Err(e.into())),
    }
}

pub async fn stats() -> Result<trawl_api::StatsResponse, ApiError> {
    telemetry_get("/api/v1/stats").await
}

pub async fn dashboard() -> Result<trawl_api::DashboardSnapshot, ApiError> {
    telemetry_get("/api/v1/dashboard").await
}

pub async fn queries() -> Result<trawl_api::QueriesResponse, ApiError> {
    telemetry_get("/api/v1/queries").await
}

async fn telemetry_get<T: DeserializeOwned>(url: &str) -> Result<T, ApiError> {
    ordinary(Request::get(url).build()?, &[200]).await
}

pub async fn cancel_query(id: u64) -> Result<trawl_api::CancelResponse, ApiError> {
    let request = Request::delete(&format!("/api/v1/queries/{id}")).build()?;
    ordinary(request, &[200]).await
}
