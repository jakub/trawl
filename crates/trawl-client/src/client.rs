// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTTP client for communicating with trawld.

use std::pin::Pin;

use futures::Stream;
use reqwest::Client;
use zeroize::Zeroizing;

use crate::error::{ClientError, NetworkError, NetworkKind};
use crate::types::{
    CancelResponse, CatalogConflictsResponse, CatalogFieldResponse, CatalogFieldsResponse,
    ClearHistoryResponse, DashboardSnapshot, DeleteSavedResponse, DeleteScheduleResponse, FieldAck,
    FieldValuesResponse, GcPinsResponse, HealthResponse, HistoryResponse, IngestResponse,
    ListAllRunsResponse, ListReportRunsResponse, ListSavedResponse, QueriesResponse, QueryResponse,
    RepinStatusResponse, ReportRunResponse, ReportRunSummary, RunsStatsResponse,
    SavedQueryResponse, ScheduleResponse, SchemaResponse, ServiceSchemaResponse, StatsResponse,
    ValidationResponse, WhoAmIResponse,
};
use crate::types::{
    CreateSavedRequestRef, ErrorResponse, ExportRequestRef, SetScheduleRequestRef, StreamEvent,
    UpdateSavedRequestRef, ValidateRequest,
};

/// Outcome of `POST /api/v1/schema/repin` — the HTTP status decoded.
#[derive(Debug, Clone)]
pub enum RepinStart {
    /// 200 with a `succeeded` row: a dry-run report.
    Report(trawl_api::RepinJobResponse),
    /// 200 with a terminal row that is neither a report nor a
    /// cancellation. Today that is `failed`, reached when the cancel
    /// effect site could not record its request row and downgraded the
    /// outcome (#109): the job stopped, the corpus is untouched, and the
    /// row's own `status` is the verdict. A caller must render that status
    /// and treat it as a failure — reading it as a report is how "dry run"
    /// gets printed over a job that did nothing.
    Failed(trawl_api::RepinJobResponse),
    /// 200 with a `cancelled` row: an operator stopped the job while this
    /// request's own ladder was still running it (#109). The corpus is
    /// untouched and the pin unchanged, so this is never a report.
    Cancelled(trawl_api::RepinJobResponse),
    /// 202: the rewrite is running; poll `schema_repin_status`.
    Started(trawl_api::RepinJobResponse),
    /// 409 with a job body: the scan projected nulled values and no force
    /// flag was passed — the job is the plan the refusal is based on.
    Refused(trawl_api::RepinJobResponse),
}

/// Outcome of `POST /api/v1/schema/repin/cancel` (#109) — the HTTP status
/// decoded, with the body's own `outcome` checked against it.
///
/// Three variants for three status codes, because none of them is an error
/// the caller should meet as an opaque `ClientError`: "too late" and
/// "nothing running" are answers about the corpus, and a CLI has to print
/// them before choosing an exit code.
#[derive(Debug, Clone)]
pub enum RepinCancel {
    /// 202: accepted; the job stops at its next file boundary.
    Cancelling(trawl_api::RepinCancelResponse),
    /// 409: the job latched its point of no return and will complete.
    PastPointOfNoReturn(trawl_api::RepinCancelResponse),
    /// 404: no repin job is running on this node.
    NoJobRunning(trawl_api::RepinCancelResponse),
}

/// The ceilings a repin request states, if any.
///
/// Both absent is the ordinary case: the server derives each number from
/// the job's own scan. A stated ceiling beside `force: false` is a 400 —
/// an unforced repin accepts no loss at all, so there is nothing to bound.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RepinCeilings {
    /// The most rows the rewrite may null.
    pub max_nulled_rows: Option<u64>,
    /// The most dialect-ambiguous numerals it may carry.
    pub max_ambiguous_rows: Option<u64>,
}

/// How a client decides whether to trust the server's TLS certificate.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum TlsTrust {
    /// The platform trust store.
    #[default]
    System,
    /// Only the roots in this PEM bundle. The chain and the hostname are
    /// still verified; no platform or built-in root is trusted.
    PinnedCa(Vec<u8>),
    /// No certificate verification at all.
    AcceptInvalid,
}

/// Hand-written so a pinned bundle shows its size, never its bytes.
impl std::fmt::Debug for TlsTrust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::System => f.write_str("System"),
            Self::PinnedCa(pem) => write!(f, "PinnedCa(<{} bytes>)", pem.len()),
            Self::AcceptInvalid => f.write_str("AcceptInvalid"),
        }
    }
}

/// HTTP client for the trawl daemon API.
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
    const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(2);

    /// Create a new client targeting the given daemon URL with an API key.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self, ClientError> {
        Self::build(base_url, token, &TlsTrust::System)
    }

    /// Create a client that accepts self-signed / invalid TLS certificates.
    ///
    /// Use this for development or when connecting to a daemon with an
    /// auto-generated self-signed cert.
    pub fn new_insecure(
        base_url: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<Self, ClientError> {
        Self::build(base_url, token, &TlsTrust::AcceptInvalid)
    }

    /// Create a client that verifies the server against `trust`.
    ///
    /// A [`TlsTrust::PinnedCa`] bundle that holds no parseable certificate
    /// fails here with [`ClientError::InvalidCa`], before any request.
    pub fn with_trust(
        base_url: impl Into<String>,
        token: impl Into<String>,
        trust: &TlsTrust,
    ) -> Result<Self, ClientError> {
        Self::build(base_url, token, trust)
    }

    /// Like [`Self::with_trust`], with `timeout` bounding each request in
    /// place of the long default meant for analytical queries.
    pub fn with_trust_timeout(
        base_url: impl Into<String>,
        token: impl Into<String>,
        trust: &TlsTrust,
        timeout: std::time::Duration,
    ) -> Result<Self, ClientError> {
        let client = reqwest_client(trust, timeout, reqwest::redirect::Policy::none())?;
        Ok(Self::with_client(base_url, token, client))
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
        trust: &TlsTrust,
    ) -> Result<Self, ClientError> {
        let client = reqwest_client(
            trust,
            Self::DEFAULT_TIMEOUT,
            reqwest::redirect::Policy::none(),
        )?;
        Ok(Self::with_client(base_url, token, client))
    }

    /// Build a full URL for an API endpoint.
    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Execute a DSL query with optional pagination and timezone.
    pub async fn query_paginated(
        &self,
        dsl: &str,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<QueryResponse, ClientError> {
        self.query_paginated_tz(dsl, limit, offset, None).await
    }

    /// Execute a DSL query with optional pagination and explicit timezone.
    pub async fn query_paginated_tz(
        &self,
        dsl: &str,
        limit: Option<usize>,
        offset: Option<usize>,
        timezone: Option<String>,
    ) -> Result<QueryResponse, ClientError> {
        let url = self.endpoint("/api/v1/query");
        let body = trawl_api::QueryRequest {
            query: dsl.to_owned(),
            limit,
            offset,
            timezone,
        };
        let req = self.client.post(&url).json(&body);
        self.send_authenticated(req).await
    }

    /// Check daemon health (unauthenticated).
    ///
    /// A 503 whose body is a health body is an answer, not an error: an
    /// unavailable daemon still reports its per-subsystem checks, so it is
    /// returned as `Ok` with `status: unavailable`. Any other failure status,
    /// or a 503 with a foreign body (a proxy's error page), stays a
    /// [`ClientError::Server`]. No body is read past [`HEALTH_BODY_CAP`]: a
    /// body past it is [`ClientError::TooLarge`] whatever the status, and
    /// an error envelope in it is never judged.
    pub async fn health(&self) -> Result<HealthResponse, ClientError> {
        let url = self.endpoint("/api/v1/health");

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(sanitize_reqwest_error)?;

        let status = resp.status();
        if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            let body = read_bounded(resp, HEALTH_BODY_CAP).await?;
            if let Ok(health) = serde_json::from_slice::<HealthResponse>(&body) {
                return Ok(health);
            }
            return Err(server_error(503, &body));
        }
        if !status.is_success() {
            let body = read_bounded(resp, HEALTH_BODY_CAP).await?;
            return Err(server_error(status.as_u16(), &body));
        }
        read_json_capped(resp, HEALTH_BODY_CAP, "health").await
    }

    /// Fetch schema introspection from the daemon.
    pub async fn schema(&self) -> Result<SchemaResponse, ClientError> {
        let url = self.endpoint("/api/v1/schema");
        let req = self.client.get(&url);
        self.send_authenticated(req).await
    }

    /// Fetch rich per-service schema from the daemon's background refresh cache.
    pub async fn schema_services(&self) -> Result<ServiceSchemaResponse, ClientError> {
        let url = self.endpoint("/api/v1/schema/services");
        let req = self.client.get(&url);
        self.send_authenticated(req).await
    }

    /// Fetch active and recent queries from the daemon (needs `query`).
    pub async fn queries(&self) -> Result<QueriesResponse, ClientError> {
        let url = self.endpoint("/api/v1/queries");
        let req = self.client.get(&url);
        self.send_authenticated(req).await
    }

    /// Cancel a running query by ID: `server_manage` cancels any query,
    /// `query_cancel` only the ones this key started.
    pub async fn cancel_query(&self, query_id: u64) -> Result<CancelResponse, ClientError> {
        let url = self.endpoint(&format!("/api/v1/queries/{query_id}"));
        let req = self.client.delete(&url);
        self.send_authenticated(req).await
    }

    /// Validate a DSL query without executing it.
    ///
    /// Checks syntax and semantic rules (function names, arity, regex patterns)
    /// but not field existence.
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

    /// Clear the authenticated key's history and return the deleted row count.
    ///
    /// Concurrent queries may record new history after the clear statement.
    pub async fn clear_history(&self) -> Result<ClearHistoryResponse, ClientError> {
        let url = self.endpoint("/api/v1/history");
        self.send_authenticated(self.client.delete(&url)).await
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
        let body = CreateSavedRequestRef { name, query };
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
        let body = UpdateSavedRequestRef { query, name: None };
        let req = self.client.put(&url).json(&body);
        self.send_authenticated(req).await
    }

    /// Update a saved query's DSL and optionally rename it.
    pub async fn update_saved_with_name(
        &self,
        id: i64,
        query: &str,
        name: Option<&str>,
    ) -> Result<SavedQueryResponse, ClientError> {
        let url = self.endpoint(&format!("/api/v1/saved/{id}"));
        let body = UpdateSavedRequestRef { query, name };
        let req = self.client.put(&url).json(&body);
        self.send_authenticated(req).await
    }

    /// Delete a saved query.
    pub async fn delete_saved(&self, id: i64) -> Result<DeleteSavedResponse, ClientError> {
        let url = self.endpoint(&format!("/api/v1/saved/{id}"));
        let req = self.client.delete(&url);
        self.send_authenticated(req).await
    }

    /// Create or update a schedule for a saved query.
    ///
    /// `window` is `"since_last"` or a duration such as `"2h"`; `None` is
    /// query mode, where the saved DSL runs verbatim. `lag` is the
    /// late-arrival allowance and is only meaningful beside a window.
    pub async fn set_schedule(
        &self,
        saved_id: i64,
        interval: &str,
        max_runs: Option<u64>,
        enabled: bool,
        window: Option<&str>,
        lag: Option<&str>,
    ) -> Result<ScheduleResponse, ClientError> {
        let url = self.endpoint(&format!("/api/v1/saved/{saved_id}/schedule"));
        let body = SetScheduleRequestRef {
            interval,
            max_runs,
            enabled,
            window,
            lag,
        };
        let req = self.client.put(&url).json(&body);
        self.send_authenticated(req).await
    }

    /// Get the schedule for a saved query.
    pub async fn get_schedule(&self, saved_id: i64) -> Result<ScheduleResponse, ClientError> {
        let url = self.endpoint(&format!("/api/v1/saved/{saved_id}/schedule"));
        let req = self.client.get(&url);
        self.send_authenticated(req).await
    }

    /// Delete the schedule for a saved query.
    pub async fn delete_schedule(
        &self,
        saved_id: i64,
    ) -> Result<DeleteScheduleResponse, ClientError> {
        let url = self.endpoint(&format!("/api/v1/saved/{saved_id}/schedule"));
        let req = self.client.delete(&url);
        self.send_authenticated(req).await
    }

    /// List report runs for a saved query with pagination.
    pub async fn list_report_runs(
        &self,
        saved_id: i64,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<ListReportRunsResponse, ClientError> {
        let url = self.endpoint(&format!("/api/v1/saved/{saved_id}/runs"));
        let mut req = self.client.get(&url);
        if let Some(l) = limit {
            req = req.query(&[("limit", l.to_string())]);
        }
        if let Some(o) = offset {
            req = req.query(&[("offset", o.to_string())]);
        }
        self.send_authenticated(req).await
    }

    /// Get a single report run with full result data.
    pub async fn get_report_run(
        &self,
        saved_id: i64,
        run_id: i64,
    ) -> Result<ReportRunResponse, ClientError> {
        let url = self.endpoint(&format!("/api/v1/saved/{saved_id}/runs/{run_id}"));
        let req = self.client.get(&url);
        self.send_authenticated(req).await
    }

    /// Trigger an immediate report run for a saved query.
    pub async fn trigger_run(&self, saved_id: i64) -> Result<ReportRunSummary, ClientError> {
        let url = self.endpoint(&format!("/api/v1/saved/{saved_id}/run"));
        let req = self.client.post(&url);
        self.send_authenticated(req).await
    }

    /// List all runs across all saved queries with pagination.
    pub async fn list_all_runs(
        &self,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<ListAllRunsResponse, ClientError> {
        let url = self.endpoint("/api/v1/runs");
        let mut req = self.client.get(&url);
        if let Some(l) = limit {
            req = req.query(&[("limit", l.to_string())]);
        }
        if let Some(o) = offset {
            req = req.query(&[("offset", o.to_string())]);
        }
        self.send_authenticated(req).await
    }

    /// Get aggregate run statistics.
    pub async fn runs_stats(&self) -> Result<RunsStatsResponse, ClientError> {
        let url = self.endpoint("/api/v1/runs/stats");
        let req = self.client.get(&url);
        self.send_authenticated(req).await
    }

    /// Fetch server stats (needs `server_manage`).
    pub async fn stats(&self) -> Result<StatsResponse, ClientError> {
        let url = self.endpoint("/api/v1/stats");
        let req = self.client.get(&url);
        self.send_authenticated(req).await
    }

    /// Fetch identity and permissions for the current token.
    ///
    /// The daemon answers exactly `200`; any other status, another `2xx`
    /// included, is a [`ClientError::Server`]. No body is read past
    /// [`WHOAMI_BODY_CAP`]: a body past it is [`ClientError::TooLarge`]
    /// whatever the status, and an error envelope in it is never judged.
    pub async fn whoami(&self) -> Result<WhoAmIResponse, ClientError> {
        let url = self.endpoint("/api/v1/whoami");
        let resp = self
            .client
            .get(&url)
            .header("Authorization", self.auth_header_value())
            .send()
            .await
            .map_err(sanitize_reqwest_error)?;
        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            let body = read_bounded(resp, WHOAMI_BODY_CAP).await?;
            return Err(server_error(status.as_u16(), &body));
        }
        read_json_capped(resp, WHOAMI_BODY_CAP, "whoami").await
    }

    /// Fetch the full dashboard snapshot (needs `server_manage`).
    pub async fn dashboard(&self) -> Result<DashboardSnapshot, ClientError> {
        let url = self.endpoint("/api/v1/dashboard");
        let req = self.client.get(&url);
        self.send_authenticated(req).await
    }

    /// List pinned fields with aggregates (`GET /api/v1/schema/fields`).
    ///
    /// `since_secs` windows on the last observation; `limit` is clamped
    /// server-side.
    pub async fn catalog_fields(
        &self,
        service: Option<&str>,
        since_secs: Option<u64>,
        limit: Option<usize>,
    ) -> Result<CatalogFieldsResponse, ClientError> {
        let url = self.endpoint("/api/v1/schema/fields");
        let mut req = self.client.get(&url);
        if let Some(svc) = service {
            req = req.query(&[("service", svc)]);
        }
        if let Some(s) = since_secs {
            req = req.query(&[("since_secs", s.to_string())]);
        }
        if let Some(l) = limit {
            req = req.query(&[("limit", l.to_string())]);
        }
        self.send_authenticated(req).await
    }

    /// Fetch one field's pin, one page of its observations, and its
    /// conflict evidence (`GET /api/v1/schema/field?name=`).
    ///
    /// The name travels as a query parameter — `req.query` percent-encodes
    /// it, so names containing `/`, `?`, or `%` survive intact.
    ///
    /// The service axis is client-chosen and unpruned, so observations are
    /// paged: `limit` is clamped server-side, and `after` takes the previous
    /// response's `services_cursor` to fetch the next page.
    pub async fn catalog_field(
        &self,
        name: &str,
        limit: Option<usize>,
        after: Option<&str>,
    ) -> Result<CatalogFieldResponse, ClientError> {
        let url = self.endpoint("/api/v1/schema/field");
        let mut req = self.client.get(&url).query(&[("name", name)]);
        if let Some(l) = limit {
            req = req.query(&[("limit", l.to_string())]);
        }
        if let Some(cursor) = after {
            req = req.query(&[("after", cursor)]);
        }
        self.send_authenticated(req).await
    }

    /// Acknowledge a degraded verdict
    /// (`POST /api/v1/schema/field/ack?name=`).
    ///
    /// The ack covers the conflict evidence that exists at the moment the
    /// server writes it and nothing beyond, so the badge returns as soon as
    /// the pin shelves another batch. 404 (no such pin) and 409 (the field
    /// is not degraded, so there is no verdict to acknowledge) both surface
    /// as [`ClientError::Server`] carrying the server's own sentence.
    pub async fn schema_field_ack(
        &self,
        name: &str,
        note: Option<&str>,
    ) -> Result<FieldAck, ClientError> {
        let url = self.endpoint("/api/v1/schema/field/ack");
        let req = self
            .client
            .post(&url)
            .query(&[("name", name)])
            .json(&crate::types::FieldAckRequestRef { note });
        self.send_authenticated(req).await
    }

    /// Withdraw an acknowledgement
    /// (`DELETE /api/v1/schema/field/ack?name=`).
    ///
    /// Idempotent by design: the server answers 204 whether or not a row
    /// was there, since "this field is not acknowledged" is the state the
    /// caller asked for either way. Only an unpinned field refuses (404).
    /// There is no body to decode, so nothing is returned.
    pub async fn schema_field_ack_clear(&self, name: &str) -> Result<(), ClientError> {
        let url = self.endpoint("/api/v1/schema/field/ack");
        let resp = self
            .client
            .delete(&url)
            .query(&[("name", name)])
            .header("Authorization", self.auth_header_value())
            .send()
            .await
            .map_err(sanitize_reqwest_error)?;
        check_status(resp).await?;
        Ok(())
    }

    /// List recent type conflicts (`GET /api/v1/schema/conflicts`).
    pub async fn catalog_conflicts(
        &self,
        field: Option<&str>,
        service: Option<&str>,
        since_secs: Option<u64>,
        limit: Option<usize>,
    ) -> Result<CatalogConflictsResponse, ClientError> {
        let url = self.endpoint("/api/v1/schema/conflicts");
        let mut req = self.client.get(&url);
        if let Some(f) = field {
            req = req.query(&[("field", f)]);
        }
        if let Some(svc) = service {
            req = req.query(&[("service", svc)]);
        }
        if let Some(s) = since_secs {
            req = req.query(&[("since_secs", s.to_string())]);
        }
        if let Some(l) = limit {
            req = req.query(&[("limit", l.to_string())]);
        }
        self.send_authenticated(req).await
    }

    /// Trigger a repin (`POST /api/v1/schema/repin`).
    ///
    /// The HTTP status carries the verdict, so this returns a three-way
    /// outcome instead of flattening 409-with-plan into an opaque error:
    /// 200 = dry-run report or a cancelled job, 202 = rewrite started, 409
    /// with a job body = lossy without force (the plan rides back). The 200
    /// pair is told apart by the row's own status, never by the code alone
    /// (see [`decode_repin_start`]). Every other failure —
    /// including the "already running" 409, whose body is the error
    /// envelope — surfaces as [`ClientError::Server`].
    ///
    /// `ceilings` are the bounds a forced repin binds itself to; an absent
    /// one leaves the number to the server's own scan.
    pub async fn schema_repin(
        &self,
        field: &str,
        to: &str,
        dialect: Option<&str>,
        dry_run: bool,
        force: bool,
        ceilings: RepinCeilings,
    ) -> Result<RepinStart, ClientError> {
        let url = self.endpoint("/api/v1/schema/repin");
        let body = trawl_api::RepinRequest {
            field: field.to_owned(),
            to: to.to_owned(),
            dialect: dialect.map(str::to_owned),
            dry_run,
            force,
            max_nulled_rows: ceilings.max_nulled_rows,
            max_ambiguous_rows: ceilings.max_ambiguous_rows,
        };
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .header("Authorization", self.auth_header_value())
            .send()
            .await
            .map_err(sanitize_reqwest_error)?;

        let status = resp.status().as_u16();
        if status == 409 {
            // Two 409 shapes: a refused-needs-force plan (a RepinResponse
            // body) and the already-running error envelope.
            let bytes = resp.bytes().await.map_err(body_read_error)?;
            if let Ok(refused) = serde_json::from_slice::<trawl_api::RepinResponse>(&bytes) {
                return Ok(RepinStart::Refused(refused.job));
            }
            let error = serde_json::from_slice::<ErrorResponse>(&bytes).map_or_else(
                |_| {
                    trawl_api::ErrorEnvelope::simple(
                        trawl_api::ErrorCode::InternalError,
                        "unknown error",
                    )
                },
                |e| e.error,
            );
            return Err(ClientError::Server { status, error });
        }
        let resp = check_status(resp).await?;
        let outcome: trawl_api::RepinResponse = resp.json().await.map_err(body_read_error)?;
        decode_repin_start(status, outcome.job)
    }

    /// Ask the running repin to stop
    /// (`POST /api/v1/schema/repin/cancel`, #109).
    ///
    /// Three statuses are answers rather than failures — 202 accepted, 409
    /// too late, 404 nothing running — so they decode into variants the way
    /// [`Self::schema_repin`] decodes its 409 plan, before the generic
    /// status check turns them into an opaque error. Everything else (401,
    /// 403, 503 on a query-only node, 5xx) stays a [`ClientError::Server`].
    pub async fn schema_repin_cancel(&self) -> Result<RepinCancel, ClientError> {
        let url = self.endpoint("/api/v1/schema/repin/cancel");
        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header_value())
            .send()
            .await
            .map_err(sanitize_reqwest_error)?;

        let status = resp.status().as_u16();
        if !matches!(status, 202 | 409 | 404) {
            // Turns any failure status into a `Server` error; a success
            // status this endpoint never sends falls through as a protocol
            // complaint rather than being guessed at.
            check_status(resp).await?;
            return Err(ClientError::Parse(format!(
                "repin cancel: unexpected HTTP {status}"
            )));
        }
        let bytes = resp.bytes().await.map_err(body_read_error)?;
        let Ok(body) = serde_json::from_slice::<trawl_api::RepinCancelResponse>(&bytes) else {
            // A 404 from a server that predates the route, or a 409 from
            // something else on the path: the error envelope, not a verdict.
            let error = serde_json::from_slice::<ErrorResponse>(&bytes).map_or_else(
                |_| {
                    trawl_api::ErrorEnvelope::simple(
                        trawl_api::ErrorCode::InternalError,
                        "unknown error",
                    )
                },
                |e| e.error,
            );
            return Err(ClientError::Server { status, error });
        };
        decode_repin_cancel(status, body)
    }

    /// Fetch the repin status surface
    /// (`GET /api/v1/schema/repin/status`).
    pub async fn schema_repin_status(&self) -> Result<RepinStatusResponse, ClientError> {
        let url = self.endpoint("/api/v1/schema/repin/status");
        let req = self.client.get(&url);
        self.send_authenticated(req).await
    }

    /// Reclaim pin slots held by dead fields
    /// (`POST /api/v1/schema/gc-pins`).
    ///
    /// A dry run and a real one both answer 200 with the same report, so
    /// there is one return type: `dry_run` and `deleted` on the body say
    /// which happened. Every refusal is the ordinary error envelope, so a
    /// 409 (a repin owns the data root, or the corpus could not be read)
    /// arrives as [`ClientError::Server`] with the server's message and
    /// the caller renders it.
    ///
    /// `older_than_secs` is the requested window; the server floors it at
    /// the retention window and reports both numbers back.
    pub async fn schema_gc_pins(
        &self,
        dry_run: bool,
        older_than_secs: Option<u64>,
    ) -> Result<GcPinsResponse, ClientError> {
        let url = self.endpoint("/api/v1/schema/gc-pins");
        let body = trawl_api::GcPinsRequest {
            dry_run,
            older_than_secs,
        };
        let req = self.client.post(&url).json(&body);
        self.send_authenticated(req).await
    }

    /// Fetch distinct values for a schema field (for autocomplete).
    ///
    /// When `service` is `Some`, values are scoped to that service's files only.
    pub async fn field_values(
        &self,
        field: &str,
        limit: Option<usize>,
        service: Option<&str>,
    ) -> Result<FieldValuesResponse, ClientError> {
        let url = self.endpoint(&format!("/api/v1/schema/values/{field}"));
        let mut req = self.client.get(&url);
        if let Some(l) = limit {
            req = req.query(&[("limit", l.to_string())]);
        }
        if let Some(svc) = service {
            req = req.query(&[("service", svc)]);
        }
        self.send_authenticated(req).await
    }

    /// Ingest log records in ndjson format.
    pub async fn ingest(
        &self,
        records: &[serde_json::Value],
    ) -> Result<IngestResponse, ClientError> {
        let url = self.endpoint("/api/v1/ingest");
        let mut ndjson = String::new();
        for record in records {
            let line = serde_json::to_string(record)
                .map_err(|e| ClientError::Parse(format!("failed to serialize record: {e}")))?;
            ndjson.push_str(&line);
            ndjson.push('\n');
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth_header_value())
            .header("Content-Type", "application/x-ndjson")
            .body(ndjson)
            .send()
            .await
            .map_err(sanitize_reqwest_error)?;

        let resp = check_status(resp).await?;
        resp.json().await.map_err(body_read_error)
    }

    /// Export query results in the specified format.
    ///
    /// Returns raw bytes suitable for writing to a file.
    pub async fn export(
        &self,
        query: &str,
        format: trawl_api::ExportFormat,
        limit: Option<usize>,
    ) -> Result<Vec<u8>, ClientError> {
        let url = self.endpoint("/api/v1/export");
        let body = ExportRequestRef { query, limit };

        let resp = self
            .client
            .post(&url)
            .query(&[("format", format.to_string())])
            .header("Authorization", self.auth_header_value())
            .json(&body)
            .send()
            .await
            .map_err(sanitize_reqwest_error)?;

        let resp = check_status(resp).await?;
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(body_read_error)
    }

    /// Stream live query results as typed events.
    ///
    /// Returns a stream of [`StreamEvent`] values parsed from the SSE
    /// connection. Handles event framing and JSON deserialization.
    pub async fn stream_events(
        &self,
        query: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ClientError>> + Send>>, ClientError>
    {
        let resp = self.stream(query).await?;
        let byte_stream = resp.bytes_stream();

        let event_stream = async_stream::try_stream! {
            futures::pin_mut!(byte_stream);
            let mut buffer = String::new();

            while let Some(chunk) = futures::StreamExt::next(&mut byte_stream).await {
                let chunk = chunk.map_err(sanitize_reqwest_error)?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(pos) = buffer.find("\n\n") {
                    let event_text = buffer[..pos].to_owned();
                    buffer.drain(..pos + 2);

                    let mut event_type = None;
                    let mut event_data = None;

                    for line in event_text.lines() {
                        if let Some(t) = line.strip_prefix("event: ") {
                            event_type = Some(t.to_owned());
                        } else if let Some(d) = line.strip_prefix("data: ") {
                            event_data = Some(d.to_owned());
                        }
                    }

                    match event_type.as_deref() {
                        Some("data") => {
                            if let Some(data) = event_data {
                                let event: serde_json::Map<String, serde_json::Value> =
                                    serde_json::from_str(&data).map_err(|e| {
                                        ClientError::Parse(format!("invalid stream event: {e}"))
                                    })?;
                                yield StreamEvent::Event(event);
                            }
                        }
                        Some("error") => {
                            if let Some(data) = event_data {
                                yield StreamEvent::Error(data);
                            }
                        }
                        Some("snapshot") => {
                            if let Some(data) = event_data {
                                // Parse {"columns":[...],"rows":[...]} payload.
                                #[derive(serde::Deserialize)]
                                struct SnapshotPayload {
                                    columns: Vec<String>,
                                    rows: Vec<serde_json::Map<String, serde_json::Value>>,
                                }
                                if let Ok(s) = serde_json::from_str::<SnapshotPayload>(&data) {
                                    yield StreamEvent::Snapshot {
                                        columns: s.columns,
                                        rows: s.rows,
                                    };
                                }
                            }
                        }
                        Some("lagged") => {
                            if let Some(data) = event_data {
                                // Parse {"missed": N} payload.
                                let missed = serde_json::from_str::<serde_json::Value>(&data)
                                    .ok()
                                    .and_then(|v| v.get("missed")?.as_u64())
                                    .unwrap_or(0);
                                yield StreamEvent::Lagged(missed);
                            }
                        }
                        _ => {} // ignore keep-alive, etc.
                    }
                }
            }
        };

        Ok(Box::pin(event_stream))
    }

    /// Raw SSE stream connection (internal — use [`stream_events`](Self::stream_events) instead).
    async fn stream(&self, query: &str) -> Result<reqwest::Response, ClientError> {
        let url = self.endpoint("/api/v1/stream");

        let resp = self
            .client
            .get(&url)
            .query(&[("query", query)])
            .header("Authorization", self.auth_header_value())
            .send()
            .await
            .map_err(sanitize_reqwest_error)?;

        let resp = check_status(resp).await?;
        Ok(resp)
    }

    /// Build the Authorization header value for authenticated requests.
    fn auth_header_value(&self) -> String {
        format!("Bearer {}", self.token.as_str())
    }

    /// Send an authenticated request, check for errors, and deserialize the response.
    async fn send_authenticated<T: serde::de::DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<T, ClientError> {
        let resp = req
            .header("Authorization", self.auth_header_value())
            .send()
            .await
            .map_err(sanitize_reqwest_error)?;

        let resp = check_status(resp).await?;
        resp.json().await.map_err(body_read_error)
    }
}

/// Classify a failure reading or decoding a response body.
///
/// reqwest reports every failure collecting a body as a decode error, a
/// stall past the request's deadline included. A timeout is a transport
/// failure, not a malformed answer, so it is a [`NetworkKind::BodyTimeout`]
/// with the same sanitized message as a timeout before the headers. Anything
/// else stays [`ClientError::Parse`].
#[allow(clippy::needless_pass_by_value)] // used as `.map_err(body_read_error)`
fn body_read_error(e: reqwest::Error) -> ClientError {
    if e.is_timeout() {
        body_timeout(e)
    } else {
        ClientError::Parse(e.to_string())
    }
}

/// Classify a failure reading one chunk of a response body: a stall past
/// the deadline is a [`NetworkKind::BodyTimeout`], anything else is a
/// [`NetworkKind::BodyRead`]. Either way the headers arrived, so the
/// server answered; the connection itself is not in doubt. The message
/// names the origin at most, never the transport library's reason.
fn body_chunk_error(e: reqwest::Error) -> ClientError {
    if e.is_timeout() {
        return body_timeout(e);
    }
    let message = e.url().map_or_else(
        || "response body broken".to_owned(),
        |url| {
            format!(
                "response body broken for API {}",
                url.origin().ascii_serialization()
            )
        },
    );
    ClientError::Network(NetworkError::new(NetworkKind::BodyRead, message))
}

/// A timeout that struck after the headers arrived: the sanitized timeout,
/// retyped as [`NetworkKind::BodyTimeout`] so a caller can tell that the
/// server answered.
fn body_timeout(e: reqwest::Error) -> ClientError {
    match sanitize_reqwest_error(e) {
        ClientError::Network(error) if error.kind == NetworkKind::Timeout => {
            ClientError::Network(NetworkError::new(NetworkKind::BodyTimeout, error.message))
        }
        other => other,
    }
}

/// Check HTTP response status and return a `Server` error on failure.
async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, ClientError> {
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let error = resp.json::<ErrorResponse>().await.map_or_else(
            |_| {
                trawl_api::ErrorEnvelope::simple(
                    trawl_api::ErrorCode::InternalError,
                    "unknown error",
                )
            },
            |e| e.error,
        );
        return Err(ClientError::Server { status, error });
    }
    Ok(resp)
}

/// The most a health body may hold and still be read as one. The
/// daemon's is a few hundred bytes; anything this large is not its body.
const HEALTH_BODY_CAP: usize = 64 * 1024;

/// The most a whoami body may hold. The daemon's names one key, its roles
/// and its permissions, well under a kilobyte.
const WHOAMI_BODY_CAP: usize = 64 * 1024;

/// Read a success body of at most `cap` bytes and parse it as JSON. A body
/// past the cap is [`ClientError::TooLarge`], and the rest is never read.
///
/// A body that does not decode is a [`ClientError::Parse`] naming only
/// `what`, never serde's reason: that reason quotes the offending value
/// (``unknown variant `…` ``), and the value is whatever the server sent,
/// an echoed key included.
async fn read_json_capped<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
    cap: usize,
    what: &str,
) -> Result<T, ClientError> {
    let body = read_bounded(resp, cap).await?;
    serde_json::from_slice(&body)
        .map_err(|_| ClientError::Parse(format!("response is not valid JSON for {what}")))
}

/// A `Server` error for `status`, carrying the body's error envelope when
/// it holds one.
fn server_error(status: u16, body: &[u8]) -> ClientError {
    let error = serde_json::from_slice::<ErrorResponse>(body).map_or_else(
        |_| trawl_api::ErrorEnvelope::simple(trawl_api::ErrorCode::InternalError, "unknown error"),
        |e| e.error,
    );
    ClientError::Server { status, error }
}

/// Read a whole response body of at most `cap` bytes. A body that holds
/// more is [`ClientError::TooLarge`], and the rest is never read.
///
/// This is the only way the client reads a capped body, and it never hands
/// back a prefix: whatever the status, a caller that judges the bytes (an
/// error envelope, a health body, a probe answer) judges all of them or
/// none.
async fn read_bounded(mut resp: reqwest::Response, cap: usize) -> Result<Vec<u8>, ClientError> {
    // A chunk's error carries no URL; restore it so the sanitized message
    // names the origin as a failure before the headers does.
    let url = resp.url().clone();
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| body_chunk_error(e.with_url(url.clone())))?
    {
        if chunk.len() > cap - body.len() {
            return Err(ClientError::TooLarge { cap });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Build the `reqwest` client for `trust`, with `timeout` bounding each
/// request and `redirect` deciding what a 3xx does.
fn reqwest_client(
    trust: &TlsTrust,
    timeout: std::time::Duration,
    redirect: reqwest::redirect::Policy,
) -> Result<Client, ClientError> {
    // Ensure ring is available as the rustls crypto provider.
    // Idempotent — returns Err if already installed, which we ignore.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut builder = Client::builder().redirect(redirect).timeout(timeout);

    match trust {
        TlsTrust::System => {}
        TlsTrust::PinnedCa(pem) => {
            let roots = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|_| ClientError::InvalidCa("the bundle is not valid PEM".into()))?;
            if roots.is_empty() {
                return Err(ClientError::InvalidCa(
                    "the bundle holds no PEM certificate".into(),
                ));
            }
            // Only these roots: no platform or built-in store is
            // consulted, and hostname verification stays on. A plain
            // `http://` URL would skip the pin entirely, so refuse it.
            builder = builder.tls_certs_only(roots).https_only(true);
        }
        TlsTrust::AcceptInvalid => {
            builder = builder.danger_accept_invalid_certs(true);
        }
    }

    builder.build().map_err(|e| match trust {
        // The only input a pinned build adds is the roots, and the root
        // store rejects a certificate whose DER does not parse.
        TlsTrust::PinnedCa(_) if e.is_builder() => {
            ClientError::InvalidCa("a certificate in the bundle does not parse".into())
        }
        _ => sanitize_reqwest_error(e),
    })
}

/// A status and a capped body from an [`OriginProbe`] request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResponse {
    /// HTTP status code.
    pub status: u16,
    /// The whole body, at most [`OriginProbe::BODY_CAP`] bytes.
    pub body: Vec<u8>,
}

/// Unauthenticated probes of a browser origin, the address `trawl-web`
/// serves (ADR-0047).
///
/// It holds no key and has no way to send one: a doctor probes the browser
/// origin precisely when that address may be wrong. Redirects are refused
/// as [`NetworkKind::Redirect`], so a probe reaches only the origin named.
/// An origin carrying userinfo is refused at construction, because the
/// HTTP client would send it as an `Authorization: Basic` header.
#[derive(Clone)]
pub struct OriginProbe {
    /// `scheme://host[:port]`, as the URL parser serializes the origin.
    origin: String,
    client: Client,
}

impl std::fmt::Debug for OriginProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginProbe")
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

impl OriginProbe {
    /// The most body bytes a probe reads. `trawl-web`'s answers to both
    /// probes are a few dozen bytes.
    pub const BODY_CAP: usize = 4096;

    /// A probe of `origin` (`scheme://host[:port]`, a trailing `/` allowed),
    /// verifying TLS under `trust`, with `timeout` bounding each request.
    ///
    /// Refused with [`ClientError::InvalidUrl`], naming none of the input:
    /// any userinfo (a user name alone included), a scheme other than
    /// `http` or `https`, and a path, query or fragment.
    pub fn new(
        origin: &str,
        trust: &TlsTrust,
        timeout: std::time::Duration,
    ) -> Result<Self, ClientError> {
        let url = reqwest::Url::parse(origin)
            .map_err(|_| ClientError::InvalidUrl("the origin is not a URL".into()))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ClientError::InvalidUrl(
                "the origin carries credentials".into(),
            ));
        }
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ClientError::InvalidUrl(
                "the origin is not http or https".into(),
            ));
        }
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err(ClientError::InvalidUrl(
                "the origin has a path, query or fragment".into(),
            ));
        }
        let refuse = reqwest::redirect::Policy::custom(|attempt| attempt.error("redirect refused"));
        Ok(Self {
            origin: url.origin().ascii_serialization(),
            client: reqwest_client(trust, timeout, refuse)?,
        })
    }

    /// `GET /healthz`: `trawl-web` answers a static `ok`.
    pub async fn healthz(&self) -> Result<ProbeResponse, ClientError> {
        let req = self.client.get(format!("{}/healthz", self.origin));
        Self::send(req).await
    }

    /// `POST /api/auth/login` with `Origin: origin_header` and an empty
    /// `api_key`. `trawl-web` checks the origin before the key, so its
    /// answer says whether it accepts that origin, and no key is sent.
    pub async fn login_probe(&self, origin_header: &str) -> Result<ProbeResponse, ClientError> {
        let req = self
            .client
            .post(format!("{}/api/auth/login", self.origin))
            .header(reqwest::header::ORIGIN, origin_header)
            .json(&serde_json::json!({ "api_key": "" }));
        Self::send(req).await
    }

    /// Send `req` and read the whole body. A body past
    /// [`Self::BODY_CAP`] is [`ClientError::TooLarge`], never a prefix:
    /// a caller matches the body exactly, and a prefix of a foreign body
    /// can be exactly what it looks for.
    async fn send(req: reqwest::RequestBuilder) -> Result<ProbeResponse, ClientError> {
        let resp = req.send().await.map_err(sanitize_reqwest_error)?;
        let status = resp.status().as_u16();
        let body = read_bounded(resp, Self::BODY_CAP).await?;
        Ok(ProbeResponse { status, body })
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

/// Categorize a reqwest error without exposing raw details or URL secrets.
/// Connection diagnostics identify only the API origin, never URL userinfo,
/// paths, query parameters, or fragments. A certificate rustls refused is
/// named as such, without the certificate or the verifier's reason.
#[allow(clippy::needless_pass_by_value)] // used as `.map_err(sanitize_reqwest_error)`
fn sanitize_reqwest_error(e: reqwest::Error) -> ClientError {
    let (kind, message) = if e.is_connect() && rejected_certificate(&e) {
        let origin = e.url().map_or_else(String::new, |url| {
            format!(" of API {}", url.origin().ascii_serialization())
        });
        (
            NetworkKind::UntrustedCertificate,
            format!(
                "TLS: the server certificate{origin} is not trusted (check ca_cert or the trial's CA)"
            ),
        )
    } else if e.is_timeout() || e.is_connect() {
        let (kind, reason) = if e.is_timeout() {
            (NetworkKind::Timeout, "request timed out")
        } else {
            (NetworkKind::Connect, "connection failed")
        };
        let message = e.url().map_or_else(
            || reason.to_owned(),
            |url| format!("{reason} for API {}", url.origin().ascii_serialization()),
        );
        (kind, message)
    } else if e.is_builder() {
        (NetworkKind::Other, "invalid request configuration".into())
    } else if e.is_redirect() {
        (NetworkKind::Redirect, "unexpected redirect".into())
    } else if e.is_decode() {
        (NetworkKind::Other, "response decode error".into())
    } else if e.is_body() {
        (NetworkKind::Other, "request body error".into())
    } else {
        (NetworkKind::Other, "request failed".into())
    };
    ClientError::Network(NetworkError::new(kind, message))
}

/// Whether `e` failed because rustls did not accept the server's
/// certificate: an unknown issuer, a name it does not cover, an expired
/// certificate, and the like.
///
/// A typed check over the source chain, never the Display text. The TLS
/// connector reports a handshake failure as a [`rustls::Error`] inside one
/// or more [`std::io::Error`]s, and `io::Error::source` skips the error it
/// wraps, so an `io::Error` is stepped into with `get_ref` instead.
fn rejected_certificate(e: &reqwest::Error) -> bool {
    let mut next: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = next {
        if matches!(
            err.downcast_ref::<rustls::Error>(),
            Some(rustls::Error::InvalidCertificate(_))
        ) {
            return true;
        }
        next = match err.downcast_ref::<std::io::Error>() {
            Some(io) => io.get_ref().map(|inner| inner as _),
            None => err.source(),
        };
    }
    false
}

/// Decode a successful repin trigger response: the status code plus the
/// job row's own status.
///
/// The status code alone is not the verdict on this route. A 200 carries
/// either a dry-run report or a job an operator cancelled mid-ladder
/// (#109), and those are opposite answers: one describes work that was
/// measured, the other work that never happened. Reading the row's status
/// is what tells them apart.
///
/// A 200 naming a `running` job contradicts the route's contract — both
/// 200 shapes are terminal rows — so it is a protocol error rather than a
/// report a caller would print as a plan. A 202 is left alone: the server
/// reads the row back after spawning the rewrite, so a fast job can
/// legitimately be terminal by the time it is serialised.
///
/// Every success arm names the status it decodes, and there is no wildcard
/// into `Report`. There used to be, and a `failed` row fell through it: a
/// cancel whose request row could not be written downgrades the job to
/// `failed` and the route still answers 200, which the CLI then printed as
/// "dry run" and exited 0 over. An unrecognised status is
/// [`RepinStart::Failed`], which no caller can read as success.
fn decode_repin_start(
    status: u16,
    job: trawl_api::RepinJobResponse,
) -> Result<RepinStart, ClientError> {
    match (status, job.status.as_str()) {
        (202, _) => Ok(RepinStart::Started(job)),
        (_, "running") => Err(ClientError::Parse(format!(
            "repin: HTTP {status} carried a job still marked running, which \
             this route only answers with a terminal row"
        ))),
        (_, "succeeded") => Ok(RepinStart::Report(job)),
        (_, "cancelled") => Ok(RepinStart::Cancelled(job)),
        _ => Ok(RepinStart::Failed(job)),
    }
}

/// Pair the cancel endpoint's status code with the body's own `outcome`.
///
/// The server writes both from one table, so a disagreement means the
/// response did not come from a server that shares this table — a proxy
/// rewriting a status, or a version skew. Trusting either half silently
/// would let a "202 accepted" be printed over a body that says nothing was
/// running, so the mismatch is a protocol error instead.
fn decode_repin_cancel(
    status: u16,
    body: trawl_api::RepinCancelResponse,
) -> Result<RepinCancel, ClientError> {
    use trawl_api::RepinCancelOutcome as O;
    match (status, body.outcome) {
        (202, O::Cancelling) => Ok(RepinCancel::Cancelling(body)),
        (409, O::PastPointOfNoReturn) => Ok(RepinCancel::PastPointOfNoReturn(body)),
        (404, O::NoJobRunning) => Ok(RepinCancel::NoJobRunning(body)),
        (_, outcome) => Err(ClientError::Parse(format!(
            "repin cancel: HTTP {status} disagrees with the body's outcome {outcome:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    /// A pinned bundle that yields no trust anchor must fail at build time,
    /// never fall back to another store or reach the network.
    #[test]
    fn pinned_ca_without_a_usable_certificate_is_refused() {
        let key_only = b"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n";
        let bad_der = b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        let bad_pem = b"-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n";
        for pem in [&b""[..], b"not pem at all\n", key_only, bad_der, bad_pem] {
            let err = HttpClient::with_trust(
                "https://127.0.0.1:1",
                "tok",
                &TlsTrust::PinnedCa(pem.to_vec()),
            )
            .expect_err("an unusable bundle must not build a client");
            assert!(
                matches!(err, ClientError::InvalidCa(_)),
                "{:?} gave {err:?}",
                String::from_utf8_lossy(pem)
            );
        }
    }

    #[test]
    fn tls_trust_debug_never_prints_the_bundle() {
        let trust = TlsTrust::PinnedCa(b"-----BEGIN CERTIFICATE-----secret".to_vec());
        let shown = format!("{trust:?}");
        assert_eq!(shown, "PinnedCa(<33 bytes>)");
        assert_eq!(format!("{:?}", TlsTrust::System), "System");
        assert_eq!(format!("{:?}", TlsTrust::AcceptInvalid), "AcceptInvalid");
    }

    #[tokio::test]
    async fn connection_error_identifies_api_origin_without_url_secrets() {
        init();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let url = format!(
            "https://private-user:private-password@{address}/private-token-path?query=private-dsl&token=private-query-token#private-fragment"
        );
        let client = HttpClient::with_client(
            url,
            "private-bearer-token",
            Client::builder().no_proxy().build().unwrap(),
        );
        // Close the accepted socket during TLS setup to produce a real
        // connection error without racing another process for a closed port.
        let (result, ()) = tokio::join!(
            client.query_paginated("private-query-text", None, None),
            async {
                let (socket, _) = listener.accept().await.unwrap();
                drop(socket);
            }
        );
        let error = result.unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("network error: connection failed for API https://{address}")
        );
        assert_eq!(error.network_kind(), Some(NetworkKind::Connect));
        assert!(!format!("{error:?}").contains("private-"));
    }

    #[tokio::test]
    async fn timeout_identifies_only_api_origin() {
        init();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_millis(50))
            .build()
            .unwrap();
        let error = client
            .get(format!("http://private-user:private-password@{address}/private-path?query=private-dsl#private-fragment"))
            .send()
            .await
            .unwrap_err();
        assert!(error.is_timeout());
        let error = sanitize_reqwest_error(error);
        assert_eq!(
            error.to_string(),
            format!("network error: request timed out for API http://{address}")
        );
        assert_eq!(error.network_kind(), Some(NetworkKind::Timeout));
    }

    /// Nothing listening is `Connect`, not a timeout or a certificate.
    #[tokio::test]
    async fn refused_connection_is_connect_kind() {
        init();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let client = HttpClient::with_trust_timeout(
            format!("http://{address}"),
            "tok",
            &TlsTrust::System,
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        let error = client.health().await.unwrap_err();
        assert_eq!(
            error.network_kind(),
            Some(NetworkKind::Connect),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!("network error: connection failed for API http://{address}")
        );
    }

    /// `with_trust_timeout` bounds each request by its own timeout.
    #[tokio::test]
    async fn with_trust_timeout_bounds_the_request() {
        init();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = HttpClient::with_trust_timeout(
            format!("http://{address}"),
            "tok",
            &TlsTrust::System,
            std::time::Duration::from_millis(100),
        )
        .unwrap();
        // Accept the connection and never answer.
        let (result, _socket) =
            tokio::join!(client.health(), async { listener.accept().await.unwrap() });
        let error = result.unwrap_err();
        assert_eq!(
            error.network_kind(),
            Some(NetworkKind::Timeout),
            "{error:?}"
        );
    }

    /// Accept one connection, read the request head, send `200` headers
    /// promising a body plus its first byte, then hold the connection open
    /// without sending the rest. Hands back the socket so it stays open
    /// until the caller drops it.
    async fn stall_after_headers(listener: tokio::net::TcpListener) -> tokio::net::TcpStream {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0, "connection closed mid-head");
            request.extend_from_slice(&buf[..n]);
        }
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100\r\n\r\n{",
            )
            .await
            .unwrap();
        socket.flush().await.unwrap();
        socket
    }

    /// A body that stalls after the headers is a body timeout: the same
    /// message as a stall before them, not a malformed answer, and a kind
    /// of its own, because the server did answer. Both body readers the
    /// doctor uses are covered: the unkeyed health and the keyed whoami.
    #[tokio::test]
    async fn body_stall_after_headers_is_a_timeout() {
        init();
        for keyed in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let client = HttpClient::with_trust_timeout(
                format!("http://{address}"),
                "tok",
                &TlsTrust::System,
                std::time::Duration::from_millis(300),
            )
            .unwrap();
            let request = async {
                if keyed {
                    client.whoami().await.map(|_| ())
                } else {
                    client.health().await.map(|_| ())
                }
            };
            let (result, _socket) = tokio::join!(request, stall_after_headers(listener));
            let error = result.unwrap_err();
            assert_eq!(
                error.network_kind(),
                Some(NetworkKind::BodyTimeout),
                "keyed={keyed}: {error:?}"
            );
            assert_eq!(
                error.to_string(),
                format!("network error: request timed out for API http://{address}"),
                "keyed={keyed}"
            );
        }
    }

    /// Headers that declare 100 bytes of body, then 10 of them, then the
    /// connection closes.
    const CUT_SHORT: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100\r\nconnection: close\r\n\r\n{\"status\":";

    /// A body the peer cuts short after the headers is a body read
    /// failure, a kind of its own: the server answered, so it is neither a
    /// failed connection nor a timeout. Every capped reader is covered: the
    /// unkeyed health, the keyed whoami, and both origin probes. The
    /// message names the origin and nothing the server sent.
    #[tokio::test]
    async fn body_cut_short_after_headers_is_a_body_read_failure() {
        init();
        for endpoint in ["health", "whoami", "healthz", "login"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let client = HttpClient::with_trust_timeout(
                format!("http://{address}"),
                "tok",
                &TlsTrust::System,
                std::time::Duration::from_secs(10),
            )
            .unwrap();
            let web = probe(address);
            let request = async {
                match endpoint {
                    "health" => client.health().await.map(|_| ()),
                    "whoami" => client.whoami().await.map(|_| ()),
                    "healthz" => web.healthz().await.map(|_| ()),
                    _ => web.login_probe("http://example.test").await.map(|_| ()),
                }
            };
            let (result, _) = tokio::join!(request, serve_once(listener, CUT_SHORT.to_vec()));
            let error = result.unwrap_err();
            assert_eq!(
                error.network_kind(),
                Some(NetworkKind::BodyRead),
                "{endpoint}: {error:?}"
            );
            assert_eq!(
                error.to_string(),
                format!("network error: response body broken for API http://{address}"),
                "{endpoint}"
            );
        }
    }

    /// A complete body that is not the expected JSON is still a parse error.
    #[tokio::test]
    async fn complete_foreign_body_stays_a_parse_error() {
        let err = health_answered_with(http_response("200 OK", "text/html", b"<html>hi</html>"))
            .await
            .expect_err("html is not a health body");
        assert!(matches!(err, ClientError::Parse(_)), "{err:?}");
    }

    // ── real listeners ──────────────────────────────────────────────────

    /// Accept one connection on `listener`, read one request (head and a
    /// `content-length` body), answer with `response`, and hand back the
    /// raw request.
    async fn serve_once(listener: tokio::net::TcpListener, response: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        let head_end = loop {
            if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0, "connection closed mid-head");
            request.extend_from_slice(&buf[..n]);
        };
        let head = String::from_utf8_lossy(&request[..head_end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .map_or(0, |v| v.trim().parse::<usize>().unwrap());
        while request.len() < head_end + length {
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0, "connection closed mid-body");
            request.extend_from_slice(&buf[..n]);
        }
        socket.write_all(&response).await.unwrap();
        socket.shutdown().await.unwrap();
        String::from_utf8(request).unwrap()
    }

    fn http_response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    async fn health_answered_with(response: Vec<u8>) -> Result<HealthResponse, ClientError> {
        init();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = HttpClient::with_trust_timeout(
            format!("http://{address}"),
            "tok",
            &TlsTrust::System,
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        let (result, request) = tokio::join!(client.health(), serve_once(listener, response));
        assert!(request.starts_with("GET /api/v1/health "), "{request}");
        result
    }

    /// An unavailable daemon answers 503 with its per-check body. That body
    /// is the answer, so it survives instead of becoming an opaque error.
    #[tokio::test]
    async fn health_503_keeps_check_body() {
        let body = br#"{"status":"unavailable","checks":{"duckdb":"error","corpus":"recovering"},"version":"0.9.0"}"#;
        let health = health_answered_with(http_response(
            "503 Service Unavailable",
            "application/json",
            body,
        ))
        .await
        .expect("a 503 health body is an answer");
        assert_eq!(health.status, trawl_api::HealthStatus::Unavailable);
        assert_eq!(
            health.checks,
            Some(std::collections::HashMap::from([
                ("duckdb".to_owned(), "error".to_owned()),
                ("corpus".to_owned(), "recovering".to_owned()),
            ]))
        );
        assert_eq!(health.version.as_deref(), Some("0.9.0"));
    }

    /// Only a 503 carrying a health body is kept. A 502 is an error whatever
    /// its body says, and a 503 from something else (a proxy's page, the
    /// error envelope) stays an error too. A 503 body past the cap is
    /// too large: its status never decides, because none of it was judged.
    #[tokio::test]
    async fn health_foreign_bodies_still_error() {
        let health = br#"{"status":"ok","checks":{"duckdb":"ok"}}"#;
        let err =
            health_answered_with(http_response("502 Bad Gateway", "application/json", health))
                .await
                .expect_err("a 502 is never a health answer");
        assert!(
            matches!(err, ClientError::Server { status: 502, .. }),
            "{err:?}"
        );

        let err = health_answered_with(http_response(
            "502 Bad Gateway",
            "text/html",
            b"<html>bad gateway</html>",
        ))
        .await
        .expect_err("a proxy page is not a health answer");
        assert!(
            matches!(err, ClientError::Server { status: 502, .. }),
            "{err:?}"
        );

        let err = health_answered_with(http_response(
            "503 Service Unavailable",
            "text/html",
            b"<html>maintenance</html>",
        ))
        .await
        .expect_err("a foreign 503 page is not a health answer");
        assert!(
            matches!(err, ClientError::Server { status: 503, .. }),
            "{err:?}"
        );

        let envelope =
            br#"{"error":{"code":"internal_error","message":"shutting down","details":[]}}"#;
        let err = health_answered_with(http_response(
            "503 Service Unavailable",
            "application/json",
            envelope,
        ))
        .await
        .expect_err("an error envelope is not a health answer");
        assert_eq!(
            err.error_envelope().map(|e| e.message.as_str()),
            Some("shutting down")
        );

        // A health body padded past the cap is not read as one.
        let mut oversized =
            br#"{"status":"unavailable","checks":{"duckdb":"error"},"pad":""#.to_vec();
        oversized.extend(std::iter::repeat_n(b'x', HEALTH_BODY_CAP));
        oversized.extend_from_slice(br#""}"#);
        let err = health_answered_with(http_response(
            "503 Service Unavailable",
            "application/json",
            &oversized,
        ))
        .await
        .expect_err("a body past the cap is not a health answer");
        assert!(
            matches!(err, ClientError::TooLarge { cap } if cap == HEALTH_BODY_CAP),
            "{err:?}"
        );
    }

    async fn whoami_answered_with(response: Vec<u8>) -> Result<WhoAmIResponse, ClientError> {
        init();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = HttpClient::with_trust_timeout(
            format!("http://{address}"),
            "tok",
            &TlsTrust::System,
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        let (result, request) = tokio::join!(client.whoami(), serve_once(listener, response));
        assert!(request.starts_with("GET /api/v1/whoami "), "{request}");
        result
    }

    /// `prefix` (an object missing its closing brace) padded past `cap`
    /// bytes and closed: still valid JSON, so only the cap can refuse it.
    fn padded(prefix: &[u8], cap: usize) -> Vec<u8> {
        let mut body = prefix.to_vec();
        body.extend_from_slice(br#","pad":""#);
        body.extend(std::iter::repeat_n(b'x', cap));
        body.extend_from_slice(br#""}"#);
        body
    }

    /// A 200 health body past the cap is refused as too large, not parsed.
    #[tokio::test]
    async fn health_200_past_the_cap_is_too_large() {
        let body = padded(
            br#"{"status":"ok","checks":{"duckdb":"ok"}"#,
            HEALTH_BODY_CAP,
        );
        let err = health_answered_with(http_response("200 OK", "application/json", &body))
            .await
            .expect_err("a body past the cap is not read");
        assert!(
            matches!(err, ClientError::TooLarge { cap } if cap == HEALTH_BODY_CAP),
            "{err:?}"
        );
    }

    /// A valid error envelope with `code`, padded with whitespace to
    /// exactly `cap` bytes and followed by trailing garbage. Its first `cap`
    /// bytes parse as the envelope, so only the cap can refuse it.
    fn envelope_past_the_cap(code: &str, cap: usize) -> Vec<u8> {
        let mut body =
            format!(r#"{{"error":{{"code":"{code}","message":"slow down","details":[]}}}}"#)
                .into_bytes();
        assert!(
            serde_json::from_slice::<ErrorResponse>(&body).is_ok(),
            "{code}"
        );
        body.resize(cap, b' ');
        body.extend_from_slice(b"trailing garbage");
        body
    }

    /// A non-success health body past the cap is too large, whatever its
    /// status: the error envelope in its first bytes is never judged, so an
    /// oversized 429 is not read as rate limiting.
    #[tokio::test]
    async fn health_error_past_the_cap_is_too_large() {
        for (status, code) in [
            ("429 Too Many Requests", "rate_limited"),
            ("500 Internal Server Error", "internal_error"),
            ("502 Bad Gateway", "internal_error"),
        ] {
            let body = envelope_past_the_cap(code, HEALTH_BODY_CAP);
            let err = health_answered_with(http_response(status, "application/json", &body))
                .await
                .expect_err("a body past the cap is not an answer");
            assert!(
                matches!(err, ClientError::TooLarge { cap } if cap == HEALTH_BODY_CAP),
                "{status}: {err:?}"
            );
        }
    }

    /// A non-200 whoami body past the cap is too large, whatever its
    /// status, and its error envelope is never judged.
    #[tokio::test]
    async fn whoami_error_past_the_cap_is_too_large() {
        for (status, code) in [
            ("401 Unauthorized", "auth_error"),
            ("403 Forbidden", "forbidden"),
            ("429 Too Many Requests", "rate_limited"),
        ] {
            let body = envelope_past_the_cap(code, WHOAMI_BODY_CAP);
            let err = whoami_answered_with(http_response(status, "application/json", &body))
                .await
                .expect_err("a body past the cap is not an answer");
            assert!(
                matches!(err, ClientError::TooLarge { cap } if cap == WHOAMI_BODY_CAP),
                "{status}: {err:?}"
            );
        }
    }

    const WHOAMI: &[u8] =
        br#"{"prefix":"abcd1234","name":"ops","kind":"service","roles":[],"permissions":["query"]}"#;

    /// A 200 whoami body past the cap is refused as too large, not parsed;
    /// one within it parses.
    #[tokio::test]
    async fn whoami_200_past_the_cap_is_too_large() {
        let who = whoami_answered_with(http_response("200 OK", "application/json", WHOAMI))
            .await
            .expect("a whoami body parses");
        assert_eq!(who.name, "ops");

        let body = padded(&WHOAMI[..WHOAMI.len() - 1], WHOAMI_BODY_CAP);
        let err = whoami_answered_with(http_response("200 OK", "application/json", &body))
            .await
            .expect_err("a body past the cap is not read");
        assert!(
            matches!(err, ClientError::TooLarge { cap } if cap == WHOAMI_BODY_CAP),
            "{err:?}"
        );
    }

    /// A body that does not decode names only the endpoint. serde's reason
    /// quotes the offending value, so a server that echoes the bearer key
    /// into a typed field (here `kind`) would otherwise put it in the
    /// error, and from there into the TUI's log and the trial's failure
    /// message, which both render this error.
    #[tokio::test]
    async fn parse_errors_never_quote_the_response() {
        const KEY: &str = "flt_Ab3dEf9hIjKlMnOpQrStUvWxYz0123456789";
        let body = format!(
            r#"{{"prefix":"abcd1234","name":"ops","kind":"{KEY}","roles":[],"permissions":[]}}"#
        );
        let err =
            whoami_answered_with(http_response("200 OK", "application/json", body.as_bytes()))
                .await
                .expect_err("an unknown kind does not decode");
        assert!(matches!(err, ClientError::Parse(_)), "{err:?}");
        assert_eq!(
            err.to_string(),
            "response parse error: response is not valid JSON for whoami"
        );
        let debug = format!("{err:?}");
        for piece in KEY.as_bytes().chunks(4) {
            let piece = std::str::from_utf8(piece).unwrap();
            assert!(!err.to_string().contains(piece), "{err}");
            assert!(!debug.contains(piece), "{debug}");
        }

        let err = health_answered_with(http_response(
            "200 OK",
            "application/json",
            format!(r#"{{"status":"{KEY}"}}"#).as_bytes(),
        ))
        .await
        .expect_err("an unknown status does not decode");
        assert_eq!(
            err.to_string(),
            "response parse error: response is not valid JSON for health"
        );
    }

    /// whoami answers exactly 200: another 2xx carrying a valid body is a
    /// server error that keeps its status.
    #[tokio::test]
    async fn whoami_other_2xx_is_an_error() {
        for status in ["201 Created", "202 Accepted"] {
            let err = whoami_answered_with(http_response(status, "application/json", WHOAMI))
                .await
                .expect_err("only 200 is a whoami answer");
            let code: u16 = status[..3].parse().unwrap();
            assert!(
                matches!(err, ClientError::Server { status, .. } if status == code),
                "{err:?}"
            );
        }
    }

    fn probe(address: std::net::SocketAddr) -> OriginProbe {
        init();
        OriginProbe::new(
            &format!("http://{address}/"),
            &TlsTrust::System,
            std::time::Duration::from_secs(10),
        )
        .unwrap()
    }

    /// Neither probe carries a key: no `Authorization` header, no cookie,
    /// and the login body's `api_key` is empty.
    #[tokio::test]
    async fn origin_probe_sends_no_authorization_header() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let web = probe(address);
        let (result, request) = tokio::join!(
            web.healthz(),
            serve_once(listener, http_response("200 OK", "text/plain", b"ok"))
        );
        assert_eq!(
            result.unwrap(),
            ProbeResponse {
                status: 200,
                body: b"ok".to_vec()
            }
        );
        assert!(request.starts_with("GET /healthz "), "{request}");
        let lower = request.to_ascii_lowercase();
        assert!(!lower.contains("\r\nauthorization:"), "{request}");
        assert!(!lower.contains("\r\ncookie:"), "{request}");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let answer = br#"{"error":"bad request"}"#;
        let web = probe(address);
        let (result, request) = tokio::join!(
            web.login_probe("https://logs.example"),
            serve_once(
                listener,
                http_response("400 Bad Request", "application/json", answer)
            )
        );
        assert_eq!(
            result.unwrap(),
            ProbeResponse {
                status: 400,
                body: answer.to_vec()
            }
        );
        assert!(request.starts_with("POST /api/auth/login "), "{request}");
        let lower = request.to_ascii_lowercase();
        assert!(
            lower.contains("\r\norigin: https://logs.example\r\n"),
            "{request}"
        );
        assert!(
            lower.contains("\r\ncontent-type: application/json\r\n"),
            "{request}"
        );
        assert!(!lower.contains("\r\nauthorization:"), "{request}");
        assert!(!lower.contains("\r\ncookie:"), "{request}");
        assert!(request.ends_with("\r\n\r\n{\"api_key\":\"\"}"), "{request}");
    }

    /// Wait up to `wait` for a connection on `listener`, and return its
    /// request head if one arrives.
    async fn head_within(
        listener: &tokio::net::TcpListener,
        wait: std::time::Duration,
    ) -> Option<String> {
        use tokio::io::AsyncReadExt;
        let (mut socket, _) = tokio::time::timeout(wait, listener.accept())
            .await
            .ok()?
            .unwrap();
        let mut head = Vec::new();
        let mut buf = [0u8; 1024];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = socket.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            head.extend_from_slice(&buf[..n]);
        }
        Some(String::from_utf8_lossy(&head).into_owned())
    }

    /// URL userinfo becomes an `Authorization: Basic` header, so a probe
    /// refuses any: a user name alone as much as a user and password. The
    /// refusal quotes none of it, the probe's `Debug` shows the origin at
    /// most, and the listener never sees a request.
    #[tokio::test]
    async fn origin_probe_refuses_userinfo() {
        init();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        // The control: a plain reqwest client does send the userinfo, so the
        // listener below can see the header it guards against.
        let plain = reqwest::Client::new();
        let send = plain
            .get(format!("http://tr4wluser:pa55word@{address}/healthz"))
            .send();
        let (_, head) = tokio::join!(
            tokio::time::timeout(std::time::Duration::from_secs(2), send),
            head_within(&listener, std::time::Duration::from_secs(10))
        );
        let head = head
            .expect("the control request arrives")
            .to_ascii_lowercase();
        assert!(head.contains("\r\nauthorization: basic "), "{head}");

        for (form, origin) in [
            ("user name only", format!("http://tr4wluser@{address}")),
            (
                "user name and password",
                format!("http://tr4wluser:pa55word@{address}"),
            ),
            (
                "empty user name, password",
                format!("http://:pa55word@{address}/"),
            ),
        ] {
            let error = OriginProbe::new(
                &origin,
                &TlsTrust::System,
                std::time::Duration::from_secs(10),
            )
            .expect_err(form);
            assert!(
                matches!(error, ClientError::InvalidUrl(_)),
                "{form}: {error:?}"
            );
            for shown in [error.to_string(), format!("{error:?}")] {
                assert!(
                    !shown.contains("tr4wluser") && !shown.contains("pa55word"),
                    "{form}: {shown}"
                );
            }
        }
        assert!(
            head_within(&listener, std::time::Duration::from_millis(300))
                .await
                .is_none(),
            "a refused probe reached the listener"
        );

        // A probe of a clean origin shows only the origin in its Debug.
        let web = OriginProbe::new(
            &format!("http://{address}/"),
            &TlsTrust::System,
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        let shown = format!("{web:?}");
        assert_eq!(
            shown,
            format!("OriginProbe {{ origin: \"http://{address}\", .. }}")
        );
    }

    /// Only an origin is accepted: another scheme, a path, a query or a
    /// fragment is refused before anything is built.
    #[test]
    fn origin_probe_accepts_only_an_origin() {
        init();
        for origin in [
            "not a url",
            "ftp://127.0.0.1:21",
            "http://127.0.0.1:1/prefix",
            "http://127.0.0.1:1/?q=1",
            "http://127.0.0.1:1/#f",
        ] {
            let error = OriginProbe::new(
                origin,
                &TlsTrust::System,
                std::time::Duration::from_secs(10),
            )
            .expect_err(origin);
            assert!(
                matches!(error, ClientError::InvalidUrl(_)),
                "{origin}: {error:?}"
            );
        }
    }

    /// A probe reads at most `BODY_CAP` bytes. A body past it is too
    /// large, never its prefix: a prefix of a foreign body can be exactly
    /// the answer a caller matches. A body of exactly `BODY_CAP` bytes is
    /// whole.
    #[tokio::test]
    async fn origin_probe_caps_the_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let web = probe(address);
        let mut big = br#"{"error":"bad request"}"#.to_vec();
        big.resize(OriginProbe::BODY_CAP, b' ');
        big.extend_from_slice(b"trailing garbage");
        let (result, _) = tokio::join!(
            web.login_probe("http://example.test"),
            serve_once(
                listener,
                http_response("400 Bad Request", "application/json", &big)
            )
        );
        assert!(
            matches!(result, Err(ClientError::TooLarge { cap }) if cap == OriginProbe::BODY_CAP),
            "{result:?}"
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let web = probe(listener.local_addr().unwrap());
        let whole = vec![b'a'; OriginProbe::BODY_CAP];
        let (result, _) = tokio::join!(
            web.healthz(),
            serve_once(listener, http_response("200 OK", "text/plain", &whole))
        );
        let response = result.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body.len(), OriginProbe::BODY_CAP);
    }

    /// A redirect is refused as `Redirect`, and its target never sees a
    /// connection.
    #[tokio::test]
    async fn origin_probe_refuses_redirects() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let elsewhere = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = elsewhere.local_addr().unwrap();
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nlocation: http://{target}/healthz\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        )
        .into_bytes();
        let probe = probe(address);
        let served = tokio::spawn(serve_once(listener, redirect));
        tokio::select! {
            result = probe.healthz() => {
                let error = result.expect_err("a redirect must be refused");
                assert_eq!(error.network_kind(), Some(NetworkKind::Redirect), "{error:?}");
                assert_eq!(error.to_string(), "network error: unexpected redirect");
            }
            _ = elsewhere.accept() => panic!("the probe followed a redirect"),
        }
        assert!(served.await.unwrap().starts_with("GET /healthz "));
    }

    #[tokio::test]
    async fn invalid_url_error_does_not_expose_input() {
        init();
        let error = Client::new()
            .get("private-invalid-url")
            .send()
            .await
            .unwrap_err();
        assert_eq!(
            sanitize_reqwest_error(error).to_string(),
            "network error: invalid request configuration"
        );
    }

    // ── endpoint URL construction ───────────────────────────────────────

    #[test]
    fn endpoint_no_trailing_slash() {
        init();
        let client = HttpClient::with_client("https://localhost:8443", "tok", Client::new());
        assert_eq!(
            client.endpoint("/api/v1/query"),
            "https://localhost:8443/api/v1/query"
        );
    }

    #[test]
    fn endpoint_strips_trailing_slash() {
        init();
        let client = HttpClient::with_client("https://localhost:8443/", "tok", Client::new());
        assert_eq!(
            client.endpoint("/api/v1/query"),
            "https://localhost:8443/api/v1/query"
        );
    }

    #[test]
    fn endpoint_strips_multiple_trailing_slashes() {
        init();
        let client = HttpClient::with_client("https://localhost:8443///", "tok", Client::new());
        assert_eq!(
            client.endpoint("/api/v1/query"),
            "https://localhost:8443/api/v1/query"
        );
    }

    // ── debug redaction ─────────────────────────────────────────────────

    #[test]
    fn debug_redacts_token() {
        init();
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

    // ── serde: QueryRequest ────────────────────────────────────────────

    #[test]
    fn query_request_serializes() {
        let req = trawl_api::QueryRequest {
            query: "service=nginx | stats count()".to_string(),
            limit: Some(10),
            offset: Some(5),
            timezone: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["query"], "service=nginx | stats count()");
        assert_eq!(json["limit"], 10);
        assert_eq!(json["offset"], 5);
    }

    #[test]
    fn query_request_omits_none() {
        let req = trawl_api::QueryRequest {
            query: "service=nginx".to_string(),
            limit: None,
            offset: None,
            timezone: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["query"], "service=nginx");
        assert!(json.get("limit").is_none());
        assert!(json.get("offset").is_none());
    }

    // ── serde: HealthResponse ────────────────────────────────────────────

    #[test]
    fn health_response_deserializes_ok() {
        let json = r#"{"status":"ok","checks":{"duckdb":"ok","auth_db":"ok","storage_db":"ok","data_path":"ok"}}"#;
        let resp: HealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.status, trawl_api::HealthStatus::Ok);
        assert_eq!(
            resp.checks,
            Some(std::collections::HashMap::from([
                ("duckdb".to_owned(), "ok".to_owned()),
                ("auth_db".to_owned(), "ok".to_owned()),
                ("storage_db".to_owned(), "ok".to_owned()),
                ("data_path".to_owned(), "ok".to_owned()),
            ]))
        );
    }

    #[test]
    fn health_response_deserializes_degraded() {
        let json = r#"{"status":"degraded","checks":{"duckdb":"ok","auth_db":"error","storage_db":"ok","data_path":"ok"}}"#;
        let resp: HealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.status, trawl_api::HealthStatus::Degraded);
        assert_eq!(
            resp.checks,
            Some(std::collections::HashMap::from([
                ("duckdb".to_owned(), "ok".to_owned()),
                ("auth_db".to_owned(), "error".to_owned()),
                ("storage_db".to_owned(), "ok".to_owned()),
                ("data_path".to_owned(), "ok".to_owned()),
            ]))
        );
    }

    #[test]
    fn health_response_deserializes_unavailable() {
        let json = r#"{"status":"unavailable","checks":{"duckdb":"error","auth_db":"ok","storage_db":"ok","data_path":"ok"}}"#;
        let resp: HealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.status, trawl_api::HealthStatus::Unavailable);
        assert_eq!(
            resp.checks,
            Some(std::collections::HashMap::from([
                ("duckdb".to_owned(), "error".to_owned()),
                ("auth_db".to_owned(), "ok".to_owned()),
                ("storage_db".to_owned(), "ok".to_owned()),
                ("data_path".to_owned(), "ok".to_owned()),
            ]))
        );
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
                "running_ms": 150,
                "own": true
            }],
            "recent": [{
                "id": 2,
                "user": "analyst",
                "query": "service=nginx",
                "duration_ms": 42,
                "rows": 100,
                "error": null,
                "timed_out": false,
                "own": false
            }]
        }"#;
        let resp: QueriesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.active.len(), 1);
        assert!(resp.active[0].own);
        assert!(!resp.recent[0].own);
        assert_eq!(resp.active[0].snapshot.user, "admin");
        assert_eq!(resp.recent.len(), 1);
        assert_eq!(resp.recent[0].snapshot.rows, Some(100));
        assert!(!resp.recent[0].snapshot.timed_out);
    }

    // ── repin start decode (#109) ───────────────────────────────────────

    fn start_job(status: &str) -> trawl_api::RepinJobResponse {
        trawl_api::RepinJobResponse {
            id: 1,
            field: "status".to_owned(),
            from_type: "BIGINT".to_owned(),
            to_type: "VARCHAR".to_owned(),
            dry_run: true,
            force: false,
            status: status.to_owned(),
            requested_by: Some("ops".to_owned()),
            started_at: "2026-09-03T10:00:00Z".to_owned(),
            finished_at: None,
            error: None,
            files_total: 0,
            rows_carrying: 0,
            projected_nulls: 0,
            resurrectable: 0,
            affected_bytes: 0,
            files_done: 0,
            rows_rewritten: 0,
            rows_nulled: 0,
            rows_resurrected: 0,
            dialect: None,
            ambiguous_numerals: 0,
            unmapped_samples: Vec::new(),
            liveness: None,
            requires_force: None,
            requires_force_reason: None,
            max_nulled_rows: None,
            max_ambiguous_rows: None,
            accepted_max_nulled_rows: None,
            accepted_max_ambiguous_rows: None,
            cancel_requested_at: None,
            cancelled_by: None,
        }
    }

    /// A 200 carrying a cancelled job is not a report. The two shapes share
    /// a status code on purpose (409 already means refused-needs-force to a
    /// body-sniffing decoder), so the row's own status is what separates
    /// "here is your plan" from "somebody stopped this".
    #[test]
    fn a_cancelled_job_decodes_as_cancelled_not_as_a_report() {
        assert!(matches!(
            decode_repin_start(200, start_job("cancelled")),
            Ok(RepinStart::Cancelled(_))
        ));
        assert!(matches!(
            decode_repin_start(200, start_job("succeeded")),
            Ok(RepinStart::Report(_))
        ));
        assert!(matches!(
            decode_repin_start(202, start_job("running")),
            Ok(RepinStart::Started(_))
        ));
    }

    /// A 200 carrying a `failed` row is not a report either.
    ///
    /// The path is real: a cancel whose request row cannot be written
    /// downgrades the job to `failed`, and that row still rides out on the
    /// original POST's 200. Under the old wildcard arm the CLI printed
    /// "dry run" and exited 0 over a repin that never ran (#109 review
    /// R2-3).
    #[test]
    fn a_failed_job_under_a_200_never_decodes_as_a_report() {
        for status in ["failed", "blocked", "refused_needs_force"] {
            let decoded = decode_repin_start(200, start_job(status));
            assert!(
                matches!(decoded, Ok(RepinStart::Failed(ref job)) if job.status == status),
                "{status} decoded as {decoded:?}"
            );
        }
    }

    /// Both 200 shapes are terminal rows, so a 200 naming a running job is
    /// a contract violation and stays an error instead of being printed as
    /// a plan.
    #[test]
    fn a_running_job_under_a_200_is_a_protocol_error() {
        let err = decode_repin_start(200, start_job("running"))
            .expect_err("200 + running must not decode");
        assert!(matches!(err, ClientError::Parse(_)), "{err:?}");
    }

    // ── repin cancel decode (#109) ──────────────────────────────────────

    fn cancel_body(outcome: trawl_api::RepinCancelOutcome) -> trawl_api::RepinCancelResponse {
        trawl_api::RepinCancelResponse {
            outcome,
            detail: "words".to_owned(),
            job: None,
        }
    }

    /// The three status codes the server sends decode into the three
    /// variants, one for one. Asserted from this end as well as from the
    /// server's `CancelVerdict::wire` table, so the two cannot drift apart
    /// without one of the two tests failing.
    #[test]
    fn repin_cancel_status_codes_decode_to_their_variants() {
        use trawl_api::RepinCancelOutcome as O;
        assert!(matches!(
            decode_repin_cancel(202, cancel_body(O::Cancelling)),
            Ok(RepinCancel::Cancelling(_))
        ));
        assert!(matches!(
            decode_repin_cancel(409, cancel_body(O::PastPointOfNoReturn)),
            Ok(RepinCancel::PastPointOfNoReturn(_))
        ));
        assert!(matches!(
            decode_repin_cancel(404, cancel_body(O::NoJobRunning)),
            Ok(RepinCancel::NoJobRunning(_))
        ));
    }

    /// A status code and a body discriminant that disagree are a protocol
    /// error. Printing "accepted" over a body saying nothing was running
    /// would tell an operator a job is stopping when none exists.
    #[test]
    fn repin_cancel_refuses_a_status_the_body_contradicts() {
        use trawl_api::RepinCancelOutcome as O;
        for (status, outcome) in [
            (202, O::NoJobRunning),
            (202, O::PastPointOfNoReturn),
            (409, O::Cancelling),
            (404, O::Cancelling),
            (200, O::Cancelling),
        ] {
            let err = decode_repin_cancel(status, cancel_body(outcome))
                .expect_err("mismatch must not decode");
            assert!(
                matches!(err, ClientError::Parse(_)),
                "{status} + {outcome:?} gave {err:?}"
            );
        }
    }

    /// `snake_case` on the wire, and the outcome round-trips.
    #[test]
    fn repin_cancel_outcome_spells_snake_case() {
        let json = serde_json::to_value(cancel_body(
            trawl_api::RepinCancelOutcome::PastPointOfNoReturn,
        ))
        .unwrap();
        assert_eq!(json["outcome"], "past_point_of_no_return");
        assert!(json.get("job").is_none(), "an absent job is omitted");
        let back: trawl_api::RepinCancelResponse = serde_json::from_value(json).unwrap();
        assert_eq!(
            back.outcome,
            trawl_api::RepinCancelOutcome::PastPointOfNoReturn
        );
    }

    // ── serde: gc-pins ──────────────────────────────────────────────────

    #[test]
    fn gc_pins_request_omits_an_unset_window() {
        let req = trawl_api::GcPinsRequest {
            dry_run: true,
            older_than_secs: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["dry_run"], true);
        assert!(
            json.get("older_than_secs").is_none(),
            "an omitted window lets the server pick its default: {json}"
        );

        let req = trawl_api::GcPinsRequest {
            dry_run: false,
            older_than_secs: Some(0),
        };
        let json = serde_json::to_value(&req).unwrap();
        // Zero is a request, not an absence: the footer axis is the other
        // half of the proof, so it is accepted literally.
        assert_eq!(json["older_than_secs"], 0);
    }

    #[test]
    fn gc_pins_dry_run_response_deserializes() {
        let json = r#"{"dry_run":true,"decided_at":"2026-09-04T12:00:00Z",
            "requested_older_than_secs":604800,"retention_floor_secs":7776000,
            "effective_older_than_secs":7776000,"pins_examined":3,
            "files_scanned":12,"deleted":0,
            "candidates":[{"field":"retired","type":"BIGINT",
              "last_seen":"2026-01-02T03:04:05Z","services":2},
             {"field":"typo","type":"VARCHAR","services":0}]}"#;
        let resp: GcPinsResponse = serde_json::from_str(json).unwrap();
        assert!(resp.dry_run);
        assert_eq!(resp.deleted, 0);
        assert_eq!(resp.retention_floor_secs, Some(7_776_000));
        assert_eq!(resp.candidates.len(), 2);
        assert_eq!(resp.candidates[0].data_type, "BIGINT");
        assert_eq!(
            resp.candidates[0].last_seen.as_deref(),
            Some("2026-01-02T03:04:05Z")
        );
        // A never-observed pin is a candidate with no observation instant,
        // and the wire omits the field rather than sending null.
        assert!(resp.candidates[1].last_seen.is_none());
    }

    #[test]
    fn gc_pins_executed_response_deserializes_without_a_floor() {
        let json = r#"{"dry_run":false,"decided_at":"2026-09-04T12:00:00Z",
            "requested_older_than_secs":2592000,
            "effective_older_than_secs":2592000,"pins_examined":1,
            "files_scanned":40,"candidates":[{"field":"gone","type":"DOUBLE",
              "services":1}],"deleted":1}"#;
        let resp: GcPinsResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.dry_run);
        assert_eq!(resp.deleted, 1);
        // Retention disabled: no floor, so the requested window stands.
        assert_eq!(resp.retention_floor_secs, None);
        assert_eq!(resp.effective_older_than_secs, 2_592_000);
    }

    /// A gc refusal has one body shape, the error envelope, so the client
    /// hands the caller the server's own sentence to render.
    #[test]
    fn gc_pins_conflict_decodes_to_a_server_error() {
        let body = r#"{"error":{"code":"internal_error",
            "message":"a repin owns the data root; pin gc deleted nothing",
            "details":[]}}"#;
        let envelope: ErrorResponse = serde_json::from_str(body).unwrap();
        let err = ClientError::Server {
            status: 409,
            error: envelope.error,
        };
        assert_eq!(
            err.error_envelope().map(|e| e.message.as_str()),
            Some("a repin owns the data root; pin gc deleted nothing")
        );
        assert!(err.to_string().contains("HTTP 409"), "{err}");
    }
}
