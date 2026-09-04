// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTTP client for communicating with trawld.

use std::pin::Pin;

use futures::Stream;
use reqwest::Client;
use zeroize::Zeroizing;

use crate::error::ClientError;
use crate::types::{
    CancelResponse, CatalogConflictsResponse, CatalogFieldResponse, CatalogFieldsResponse,
    DashboardSnapshot, DeleteSavedResponse, DeleteScheduleResponse, FieldValuesResponse,
    HealthResponse, HistoryResponse, IngestResponse, ListAllRunsResponse, ListReportRunsResponse,
    ListSavedResponse, QueriesResponse, QueryResponse, RepinStatusResponse, ReportRunResponse,
    ReportRunSummary, RunsStatsResponse, SavedQueryResponse, ScheduleResponse, SchemaResponse,
    ServiceSchemaResponse, StatsResponse, ValidationResponse, WhoAmIResponse,
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
        // Ensure ring is available as the rustls crypto provider.
        // Idempotent — returns Err if already installed, which we ignore.
        let _ = rustls::crypto::ring::default_provider().install_default();

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
    pub async fn set_schedule(
        &self,
        saved_id: i64,
        interval: &str,
        max_runs: Option<u64>,
        enabled: bool,
    ) -> Result<ScheduleResponse, ClientError> {
        let url = self.endpoint(&format!("/api/v1/saved/{saved_id}/schedule"));
        let body = SetScheduleRequestRef {
            interval,
            max_runs,
            enabled,
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
    pub async fn whoami(&self) -> Result<WhoAmIResponse, ClientError> {
        let url = self.endpoint("/api/v1/whoami");
        let req = self.client.get(&url);
        self.send_authenticated(req).await
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
    pub async fn schema_repin(
        &self,
        field: &str,
        to: &str,
        dialect: Option<&str>,
        dry_run: bool,
        force: bool,
    ) -> Result<RepinStart, ClientError> {
        let url = self.endpoint("/api/v1/schema/repin");
        let body = trawl_api::RepinRequest {
            field: field.to_owned(),
            to: to.to_owned(),
            dialect: dialect.map(str::to_owned),
            dry_run,
            force,
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
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| ClientError::Parse(e.to_string()))?;
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
        let outcome: trawl_api::RepinResponse = resp
            .json()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))?;
        decode_repin_start(status, outcome.job)
    }

    /// Ask the running repin to stop
    /// (`POST /api/v1/schema/repin/cancel`, #109).
    ///
    /// Three statuses are answers rather than failures — 202 accepted, 409
    /// too late, 404 nothing running — so they decode into variants the way
    /// [`Self::schema_repin`] decodes its 409 plan, before the generic
    /// status check turns them into an opaque error. Everything else (401,
    /// 503 on a query-only node, 5xx) stays a [`ClientError::Server`].
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
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))?;
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
        resp.json()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))
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
            .map_err(|e| ClientError::Parse(e.to_string()))
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
        resp.json()
            .await
            .map_err(|e| ClientError::Parse(e.to_string()))
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
        let json = r#"{"status": "ok"}"#;
        let resp: HealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.status, trawl_api::HealthStatus::Ok);
        assert!(resp.checks.is_none());
    }

    #[test]
    fn health_response_deserializes_degraded() {
        let json = r#"{"status":"degraded","checks":{"duckdb":"ok","auth_db":"error: db locked","data_path":"ok"}}"#;
        let resp: HealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.status, trawl_api::HealthStatus::Degraded);
        let checks = resp.checks.unwrap();
        assert_eq!(checks["duckdb"], "ok");
        assert!(checks["auth_db"].starts_with("error:"));
    }

    #[test]
    fn health_response_deserializes_unavailable() {
        let json = r#"{"status":"unavailable","checks":{"duckdb":"error: connection lost","auth_db":"ok","data_path":"ok"}}"#;
        let resp: HealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.status, trawl_api::HealthStatus::Unavailable);
        let checks = resp.checks.unwrap();
        assert!(checks["duckdb"].starts_with("error:"));
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
                "query": "service=nginx",
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
}
