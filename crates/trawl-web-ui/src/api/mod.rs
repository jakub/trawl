// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Typed helpers for calling the trawl-web proxy from the browser.
//!
//! All calls are same-origin (`/api/auth/...`, `/api/v1/...`) and rely on
//! the httpOnly session cookie being attached automatically by the
//! browser. Never touches `Authorization` — that's the proxy's job.
//!
//! Every ordinary endpoint sends its request once and reads the body
//! once ([`exchange`]), then hands the answer to [`response_policy`]:
//! the endpoint names its success statuses, and every failure goes to
//! the one classifier (ADR-0066). Nothing here reads a status as a
//! failure class or navigates. The investigation writes keep their own
//! send-once paths and classifiers, because their unknown outcome must
//! never read as a refusal.

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

/// Rows per page for the snapshot results table. Defined by the search
/// URL contract (the `page` parameter's offset check is part of it) and
/// re-exported here so request building keeps one path to it.
pub use crate::search_url::PAGE_SIZE;

/// Rows per page in both run browsers.
pub const RUNS_PAGE_SIZE: std::num::NonZeroUsize = std::num::NonZeroUsize::new(20).unwrap();

/// The failure type lives in a pure module so its classifier is tested
/// natively; the transport below is the only part that needs a browser.
pub use crate::api_error::ApiError;
/// What the repin POST answered; built by [`response_policy::repin`].
pub use crate::repin_flow::RepinOutcome;

impl From<gloo_net::Error> for ApiError {
    fn from(e: gloo_net::Error) -> Self {
        Self::Network(e.to_string())
    }
}

/// An answer: its status, and its body read once as text. `None` is a
/// body that could not be read, which leaves the classifier only the
/// status.
struct Answer {
    status: u16,
    body: Option<String>,
}

/// Send `request` once. A request that got no answer at all is
/// [`ApiError::Network`].
async fn send(request: Request) -> Result<Response, ApiError> {
    Ok(request.send().await?)
}

/// Read an answer's body once, as text.
async fn read(resp: &Response) -> Option<String> {
    resp.text().await.ok()
}

/// Send once and read the body once: the one path every ordinary
/// endpoint takes to the server.
async fn exchange(request: Request) -> Result<Answer, ApiError> {
    let resp = send(request).await?;
    Ok(Answer {
        status: resp.status(),
        body: read(&resp).await,
    })
}

/// An ordinary JSON endpoint: [`exchange`], then decode a status in
/// `ok` and classify any other ([`response_policy::answer`]).
async fn ordinary<T: DeserializeOwned>(request: Request, ok: &[u16]) -> Result<T, ApiError> {
    let Answer { status, body } = exchange(request).await?;
    response_policy::answer(status, body.as_deref(), ok)
}

/// A paced read: [`send`], and a 429 is [`Paced::Throttled`] before its
/// body is read ([`response_policy::paced`]). Anything else is read once
/// and decoded as an ordinary 200 answer.
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

/// An HTTP date as unix milliseconds, for `Retry-After`.
fn http_date_ms(date: &str) -> Option<f64> {
    Some(js_sys::Date::parse(date)).filter(|ms| !ms.is_nan())
}

/// `builder` with `body` as its JSON payload.
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

/// POST `/api/auth/login` with the given API key. Returns identity on
/// success, [`ApiError::Unauthorized`] on bad key.
pub async fn login(api_key: &str) -> Result<LoginResponse, ApiError> {
    let request = with_json(Request::post("/api/auth/login"), &LoginRequest { api_key })?;
    ordinary(request, &[200]).await
}

/// GET `/api/auth/me` — read current session identity.
pub async fn me() -> Result<MeResponse, ApiError> {
    ordinary(Request::get("/api/auth/me").build()?, &[200]).await
}

/// POST `/api/auth/logout` — clears the session cookie. Success carries
/// no JSON body ([`response_policy::logout`]).
pub async fn logout() -> Result<(), ApiError> {
    let Answer { status, body } = exchange(Request::post("/api/auth/logout").build()?).await?;
    response_policy::logout(status, body.as_deref())
}

/// GET /api/v1/health — health/version probe. trawld serves it without
/// auth, but the browser reaches it through the trawl-web `/api/v1/*` proxy,
/// which requires a session; `/healthz` is the public probe.
///
/// A 503 is a degraded report only when its body is one
/// ([`response_policy::health`]).
pub async fn health() -> Result<HealthResponse, ApiError> {
    let Answer { status, body } = exchange(Request::get("/api/v1/health").build()?).await?;
    response_policy::health(status, body.as_deref())
}

/// GET /api/v1/history — paginated query history for the current session.
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

/// POST /api/v1/saved — create a named saved query ("net"). A name
/// refusal keeps its code; the form words it with
/// [`crate::api_error::saved_name_refusal`].
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

/// GET /api/v1/schema/services — rich per-service metadata (columns,
/// stats, daily event counts). Cached server-side; see
/// `schema_refresh.rs`. Used by the Schema page to render per-service
/// cards and their drawer inspectors.
pub async fn schema_services() -> Result<ServiceSchemaResponse, ApiError> {
    ordinary(Request::get("/api/v1/schema/services").build()?, &[200]).await
}

/// GET `/api/v1/schema/field?name=` — one field's pin, one page of its
/// per-service observations (page with the response's `services_cursor`
/// as `after`), its retained conflict evidence, and the analyzer's
/// verdict when the pin is degraded.
///
/// A field with no pin answers 404, a failure whose
/// [`ApiError::http_status`] is 404 — the case file maps that to
/// fleet-ui's neutral `LoadState::Missing`, never to an error banner.
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

/// POST `/api/v1/schema/repin` — plan (`dry_run`) or start a repin.
///
/// `SchemaWrite`-gated server-side, which is the only enforcement: this
/// call is issued exactly as written whatever the SPA believes about the
/// session's permissions.
///
/// The 409 is double-shaped — a refusal carries the plan, a held slot
/// carries the error envelope — and the two are told apart by decoding
/// the body ([`response_policy::repin`]), never by matching on error
/// text. Any other failure keeps the server's envelope, so the modal can
/// show an operator-facing refusal ("not a pinned field", "repin
/// requires an ingest-enabled node") in the server's words.
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
/// since it was claimed. `Ok(None)` is the route's 404: it holds no job
/// with that id.
///
/// Gated server-side on `schema_write` or `schema_read`, so the session
/// that started a job can always follow it.
pub async fn repin_job_status(id: i64) -> Result<Option<RepinJobResponse>, ApiError> {
    let request = Request::get(&format!("/api/v1/schema/repin/status?id={id}")).build()?;
    let Answer { status, body } = exchange(request).await?;
    response_policy::repin_job_status(status, body.as_deref())
}

/// One URL query-parameter value.
fn encode(raw: &str) -> String {
    js_sys::encode_uri_component(raw)
        .as_string()
        .unwrap_or_else(|| raw.to_string())
}

/// POST /api/v1/query — execute a DSL query over the window its
/// [`FetchPlan`] asks for.
///
/// The plan owns the arithmetic (ADR-0037): a raw-event query reads one
/// [`PAGE_SIZE`] page, an aggregation reads the whole result once. This
/// transport does none of its own, so there is a single place where a
/// page number becomes a `(limit, offset)` pair.
///
/// A page of a raw-event query also asks for the whole result's
/// histogram, which the strip above the table draws. An aggregation's
/// `Whole` read asks for none, so its request carries no key.
pub async fn query(q: &str, plan: FetchPlan) -> Result<QueryResponse, ApiError> {
    let (limit, offset) = plan.window();
    let body = QueryRequest {
        query: q.to_owned(),
        limit: Some(limit),
        offset: Some(offset),
        histogram: matches!(plan, FetchPlan::Page { .. }),
    };
    // A refusal of the query text arrives as the whole envelope, spans
    // and all, for the query error notice (ADR-0039).
    ordinary(with_json(Request::post("/api/v1/query"), &body)?, &[200]).await
}

/// GET /api/v1/saved — list all saved queries ("nets") for the session.
pub async fn list_saved() -> Result<ListSavedResponse, ApiError> {
    ordinary(Request::get("/api/v1/saved").build()?, &[200]).await
}

/// PUT /api/v1/saved/{id} — update a saved query's DSL.
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

/// DELETE /api/v1/saved/{id} — delete a saved query.
pub async fn delete_saved(id: i64) -> Result<DeleteSavedResponse, ApiError> {
    ordinary(
        Request::delete(&format!("/api/v1/saved/{id}")).build()?,
        &[200],
    )
    .await
}

/// PUT /api/v1/saved/{id}/schedule — create or update a schedule.
///
/// The PUT takes the schedule's whole shape, so `window` and `lag` are
/// arguments rather than constants: omitting a window is not "leave it
/// alone", it is query mode. The pair comes from
/// `schedule_edit::WindowDraft::to_request`, which is what the form is
/// showing.
///
/// The server refuses several window and lag combinations by name, and
/// the refusal keeps its envelope, so the operator reads the server's
/// sentence rather than a bare status.
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

/// DELETE /api/v1/saved/{id}/schedule — remove a schedule.
pub async fn delete_schedule(saved_id: i64) -> Result<DeleteScheduleResponse, ApiError> {
    let request = Request::delete(&format!("/api/v1/saved/{saved_id}/schedule")).build()?;
    ordinary(request, &[200]).await
}

/// GET /api/v1/saved/{id}/runs — paginated runs for a saved query.
pub async fn list_runs(
    saved_id: i64,
    limit: usize,
    offset: usize,
) -> Result<ListReportRunsResponse, ApiError> {
    let url = format!("/api/v1/saved/{saved_id}/runs?limit={limit}&offset={offset}");
    ordinary(Request::get(&url).build()?, &[200]).await
}

/// GET `/api/v1/saved/{id}/runs/{run_id}` — single run with result data.
/// Needs `query` beside `saved_query` (ADR-0018 as amended on 2026-11-11),
/// so a session without `query` never builds the preview that calls it.
///
/// A non-2xx keeps its envelope: a run whose stored result file is gone
/// answers 409 with a sentence naming the run (issue #227), and the drawer
/// shows that sentence rather than inventing copy from a bare status.
pub async fn get_run(saved_id: i64, run_id: i64) -> Result<ReportRunResponse, ApiError> {
    let url = format!("/api/v1/saved/{saved_id}/runs/{run_id}");
    ordinary(Request::get(&url).build()?, &[200]).await
}

/// GET /api/v1/runs — paginated runs across all saved queries.
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

/// POST /api/v1/export?format={fmt} — export query results as binary.
///
/// Returns raw bytes and a suggested filename from `Content-Disposition`.
/// The body is read once, as bytes; a failure is classified from those
/// same bytes ([`response_policy::export`]).
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
    let resp = send(with_json(Request::post(&url), &body)?).await?;
    let filename =
        parse_content_disposition(resp.headers().get("content-disposition").as_deref(), format);
    let bytes = response_policy::export(resp.status(), resp.binary().await.ok())?;
    Ok((bytes, filename))
}

/// PUT /api/v1/saved/{id} — update a saved query's DSL and optionally its name.
/// A name refusal keeps its code, as [`create_saved`]'s does.
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
///
/// Needs `query` beside `saved_query`, so a session without `query` is
/// offered no Run now. A refusal (this key's manual run or the server's
/// manual runs already going, a run already in progress, the max-runs
/// cap, nothing new to read, no schedule) is an operator-facing sentence,
/// and the toast shows it rather than a bare status.
pub async fn trigger_run(saved_id: i64) -> Result<ReportRunSummary, ApiError> {
    let request = Request::post(&format!("/api/v1/saved/{saved_id}/run")).build()?;
    ordinary(request, &[200]).await
}

/// GET /api/v1/runs/stats — aggregate run statistics.
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

/// GET `/api/v1/capabilities` — the bounds the server runs a query and a
/// capture under, among the rest of what a client plans around (ADR-0058).
/// Any key with a trawl grant reads it.
pub async fn capabilities() -> Result<trawl_api::capabilities::CapabilitiesResponse, ApiError> {
    ordinary(Request::get("/api/v1/capabilities").build()?, &[200]).await
}

/// GET `/api/v1/investigations` — every investigation, open ones first,
/// each group most recently changed first (ADR-0059). Needs
/// `investigation_read` and `query`; a refusal keeps its envelope.
pub async fn list_investigations() -> Result<InvestigationList, ApiError> {
    ordinary(Request::get("/api/v1/investigations").build()?, &[200]).await
}

/// GET `/api/v1/investigations/{id}?since=&limit=` — the investigation
/// and at most `limit` of its entries changed after `since`, in
/// `updated_seq` order (ADR-0059). The id is typed, so only its exact
/// 32-digit shape reaches the path. 404 for an id the installation does
/// not hold, kept as an envelope like every other refusal.
///
/// The board reads this every few seconds, so a 429 is not a failure to
/// show: it comes back as [`Paced::Throttled`] with the pause its
/// `Retry-After` asks for. `signal` aborts the request when a newer read
/// replaced it or the page that asked is gone.
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

/// POST `/api/v1/investigations/{id}/entries` — keep one captured row of
/// `result_id` with `reason`, under the session's key (ADR-0059). An
/// append carries no expected sequence, and the board sends no
/// `seen_seq`: it learns what changed from its own next read, never from
/// this answer.
///
/// An append is not idempotent, so a request whose answer never arrived,
/// arrived unreadable, or was a server error ([`write_refusal`]), is
/// [`WriteFailure::Unknown`]: the caller must
/// not send it again by itself.
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

/// PATCH `/api/v1/investigations/{id}` — close or reopen it, if its
/// sequence is still `expected_seq` (ADR-0059). One field per request.
/// The answer's investigation, as the write left it, is the result. No
/// `seen_seq`: the board's cursor moves only with its own reads.
/// A sequence that moved is [`WriteFailure::Stale`]; a close of an
/// investigation already closed is [`WriteFailure::Closed`]. Nothing is
/// retried here, and an answer that never arrived, or a server error, is
/// [`WriteFailure::Unknown`].
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

/// POST `/api/v1/results` — run `parts` as a capture attached to
/// `investigation`, under the session's key, stating `purpose`
/// (ADR-0057, ADR-0059). The server runs the query and appends the step,
/// so the browser names the evidence and never supplies it. No
/// `seen_seq`: the board learns what changed from its own reads.
///
/// A capture is a write. A request whose answer never arrived, arrived
/// unreadable, or was a server error, is [`SendFailure::Unknown`], and nothing here
/// sends it again; a 429 is [`SendFailure::Busy`] with the pause its
/// `Retry-After` asks for, which the person acts on.
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

/// GET `/api/v1/results/{id}?offset=&limit=` — one page of a captured
/// result (ADR-0057): a row an investigation cites, or a page of the
/// evidence viewer. Without `limit` the server answers its own page size,
/// `[capture] page_max_rows`, and says so in the page's `pagination`. A
/// result whose evidence file was pruned or expired answers 410
/// `result_gone`, one whose file is missing 409 `result_unavailable`;
/// the caller tells them apart by status. `id` must already have a
/// result id's shape ([`crate::investigation_log::is_result_id`]); this
/// refuses anything else before the request.
///
/// Every read here shares the reader's rate limit with the board's polls,
/// so a 429 is not an answer about the rows: it comes back as
/// [`Paced::Throttled`] with the pause its `Retry-After` asks for.
/// `signal` aborts the request when the page or viewer that asked is gone.
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

async fn telemetry_get<T: DeserializeOwned>(url: &str) -> Result<T, ApiError> {
    ordinary(Request::get(url).build()?, &[200]).await
}

/// Request cancellation without inferring success from an HTTP status alone.
pub async fn cancel_query(id: u64) -> Result<trawl_api::CancelResponse, ApiError> {
    let request = Request::delete(&format!("/api/v1/queries/{id}")).build()?;
    ordinary(request, &[200]).await
}
