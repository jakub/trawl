// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A recording [`Backend`] over fixtures, and the fixtures the tool tests
//! share.
//!
//! Each method answers from a handler the test set, and records the call
//! with its arguments. A method with no handler panics, so a test that
//! does not expect a call fails loudly when it happens.

#![allow(
    dead_code,
    clippy::missing_panics_doc,
    clippy::unnecessary_wraps,
    clippy::unused_async_trait_impl,
    clippy::unused_self
)]

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::json;
use trawl_api::value::{Column, ColumnType, QueryResult, Value};
use trawl_api::{
    CapabilitiesResponse, CapabilityBounds, CaptureAdmission, CaptureQuotas, DslCapabilities,
    HealthStatus,
};
use trawl_client::types::ServiceSchemaResponse;
use trawl_client::{
    CaptureRequest, CaptureResponse, CapturedResult, CatalogFieldResponse, CatalogFieldsResponse,
    ClientError, ColumnFacts, DashboardSnapshot, ErrorCode, ErrorEnvelope, FieldValuesResponse,
    HealthResponse, PaginationMeta, QueryResponse, ResultCell, ResultColumn, ResultList,
    ResultPage, ResultShape, ResultSource, ResultState, SchemaResponse, StoredAs,
    ValidationResponse, WhoAmIResponse,
};
use trawl_mcp::Backend;

type Handler<T> = Box<dyn Fn() -> Result<T, ClientError> + Send + Sync>;

/// A backend answering from per-method handlers and recording each call.
#[derive(Default)]
pub struct Fake {
    pub calls: Mutex<Vec<String>>,
    pub whoami: Option<Handler<WhoAmIResponse>>,
    pub health: Option<Handler<(u16, HealthResponse)>>,
    pub capabilities: Option<Handler<CapabilitiesResponse>>,
    pub dashboard: Option<Handler<DashboardSnapshot>>,
    pub schema_services: Option<Handler<ServiceSchemaResponse>>,
    pub schema: Option<Handler<SchemaResponse>>,
    pub catalog_fields: Option<Handler<CatalogFieldsResponse>>,
    /// By field name; a name without an entry answers 404 `not_found`.
    pub catalog_field: HashMap<String, CatalogFieldResponse>,
    pub field_values: Option<Handler<FieldValuesResponse>>,
    pub query: Option<Handler<QueryResponse>>,
    pub validate: Option<Handler<ValidationResponse>>,
    pub capture: Option<Handler<CaptureResponse>>,
    pub list_results: Option<Handler<ResultList>>,
    pub result_page: Option<Handler<ResultPage>>,
    pub result_cell: Option<Handler<ResultCell>>,
}

impl Fake {
    /// Every call so far, in order, each as `method arg...`.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// The method names called so far, in order.
    pub fn methods(&self) -> Vec<String> {
        self.calls()
            .iter()
            .map(|call| call.split(' ').next().unwrap_or("").to_owned())
            .collect()
    }

    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    fn answer<T>(&self, method: &str, handler: Option<&Handler<T>>) -> Result<T, ClientError> {
        let handler = handler.unwrap_or_else(|| panic!("unexpected call to {method}"));
        handler()
    }
}

/// A handler answering `value` every time.
pub fn ok<T: Clone + Send + Sync + 'static>(value: T) -> Option<Handler<T>> {
    Some(Box::new(move || Ok(value.clone())))
}

/// A handler refusing every time with `status` and `code`.
pub fn refuse<T>(status: u16, code: ErrorCode, message: &str) -> Option<Handler<T>> {
    let message = message.to_owned();
    Some(Box::new(move || {
        Err(server_error(status, code.clone(), &message))
    }))
}

/// A refusal as trawl-client returns it.
pub fn server_error(status: u16, code: ErrorCode, message: &str) -> ClientError {
    ClientError::Server {
        status,
        error: ErrorEnvelope::simple(code, message),
        request_id: Some("req-0001".to_owned()),
        retry_after_secs: None,
    }
}

impl Backend for Fake {
    async fn whoami(&self) -> Result<WhoAmIResponse, ClientError> {
        self.record("whoami".to_owned());
        self.answer("whoami", self.whoami.as_ref())
    }

    async fn health_with_status(&self) -> Result<(u16, HealthResponse), ClientError> {
        self.record("health".to_owned());
        self.answer("health", self.health.as_ref())
    }

    async fn capabilities(&self) -> Result<CapabilitiesResponse, ClientError> {
        self.record("capabilities".to_owned());
        self.answer("capabilities", self.capabilities.as_ref())
    }

    async fn dashboard(&self) -> Result<DashboardSnapshot, ClientError> {
        self.record("dashboard".to_owned());
        self.answer("dashboard", self.dashboard.as_ref())
    }

    async fn schema_services(&self) -> Result<ServiceSchemaResponse, ClientError> {
        self.record("schema_services".to_owned());
        self.answer("schema_services", self.schema_services.as_ref())
    }

    async fn schema(&self) -> Result<SchemaResponse, ClientError> {
        self.record("schema".to_owned());
        self.answer("schema", self.schema.as_ref())
    }

    async fn catalog_fields(
        &self,
        service: Option<&str>,
        since_secs: Option<u64>,
        limit: Option<usize>,
    ) -> Result<CatalogFieldsResponse, ClientError> {
        self.record(format!(
            "catalog_fields {service:?} {since_secs:?} {limit:?}"
        ));
        self.answer("catalog_fields", self.catalog_fields.as_ref())
    }

    async fn catalog_field(
        &self,
        name: &str,
        limit: Option<usize>,
        after: Option<&str>,
    ) -> Result<CatalogFieldResponse, ClientError> {
        self.record(format!("catalog_field {name} {limit:?} {after:?}"));
        self.catalog_field
            .get(name)
            .cloned()
            .ok_or_else(|| server_error(404, ErrorCode::NotFound, &format!("no pin for {name}")))
    }

    async fn field_values(
        &self,
        field: &str,
        limit: Option<usize>,
        service: Option<&str>,
    ) -> Result<FieldValuesResponse, ClientError> {
        self.record(format!("field_values {field} {limit:?} {service:?}"));
        self.answer("field_values", self.field_values.as_ref())
    }

    async fn query_paginated_tz(
        &self,
        dsl: &str,
        limit: Option<usize>,
        offset: Option<usize>,
        timezone: Option<String>,
    ) -> Result<QueryResponse, ClientError> {
        self.record(format!("query {limit:?} {offset:?} {timezone:?} {dsl}"));
        self.answer("query", self.query.as_ref())
    }

    async fn validate(&self, dsl: &str) -> Result<ValidationResponse, ClientError> {
        self.record(format!("validate {dsl}"));
        self.answer("validate", self.validate.as_ref())
    }

    async fn capture(&self, request: &CaptureRequest) -> Result<CaptureResponse, ClientError> {
        self.record(format!("capture {}", request.query));
        self.answer("capture", self.capture.as_ref())
    }

    async fn list_results(&self) -> Result<ResultList, ClientError> {
        self.record("list_results".to_owned());
        self.answer("list_results", self.list_results.as_ref())
    }

    async fn result_page(
        &self,
        id: &str,
        offset: u64,
        limit: u64,
        columns: Option<&[&str]>,
    ) -> Result<ResultPage, ClientError> {
        self.record(format!("result_page {id} {offset} {limit} {columns:?}"));
        self.answer("result_page", self.result_page.as_ref())
    }

    async fn result_cell(
        &self,
        id: &str,
        row: u64,
        column: usize,
        offset: Option<u64>,
        length: Option<u64>,
    ) -> Result<ResultCell, ClientError> {
        self.record(format!(
            "result_cell {id} {row} {column} {offset:?} {length:?}"
        ));
        self.answer("result_cell", self.result_cell.as_ref())
    }
}

// -- fixtures -----------------------------------------------------------------

pub const RESULT_ID: &str = "0123456789abcdef0123456789abcdef";
pub const ORIGIN: &str = "https://trawl.example.com";

/// A key holding `permissions`.
pub fn whoami(permissions: &[&str]) -> WhoAmIResponse {
    serde_json::from_value(json!({
        "prefix": "abcd1234",
        "name": "agent",
        "kind": "service",
        "roles": ["agent"],
        "permissions": permissions,
    }))
    .unwrap()
}

/// A healthy report.
pub fn health() -> HealthResponse {
    HealthResponse {
        status: HealthStatus::Ok,
        checks: Some(
            [("duckdb", "ok"), ("corpus", "ok")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        ),
        version: Some("0.9.3".to_owned()),
    }
}

/// Capabilities with `link_origin`.
pub fn capabilities(link_origin: Option<&str>) -> CapabilitiesResponse {
    CapabilitiesResponse {
        version: "0.9.3".to_owned(),
        dsl: DslCapabilities {
            stages: vec!["stats".to_owned(), "where".to_owned(), "sort".to_owned()],
            functions: vec!["count".to_owned(), "max".to_owned()],
            max_query_bytes: 65_536,
            max_pipeline_stages: 128,
        },
        bounds: CapabilityBounds {
            default_window_secs: 900,
            max_span_secs: 604_800,
            deadline_secs: 15,
            max_rows: 5000,
            max_wire_bytes: 4_194_304,
            page_max_rows: 500,
            ttl_secs: 900,
            query_timeout_secs: 30,
            max_result_rows: 10_000,
        },
        quotas: CaptureQuotas {
            per_key_results: 64,
            per_key_bytes: 67_108_864,
            installation_results: 512,
            installation_bytes: 536_870_912,
        },
        admission: CaptureAdmission {
            per_key_running: 1,
            installation_running: 2,
            queue_depth: 4,
            rate_rpm: Some(600),
        },
        link_origin: link_origin.map(str::to_owned),
        health: health(),
    }
}

/// Two services, one with a degraded field.
pub fn schema_services() -> ServiceSchemaResponse {
    serde_json::from_value(json!({
        "services": [
            {
                "name": "api",
                "columns": [
                    {"name": "status", "type": "BIGINT", "null_count": 3, "total_count": 100,
                     "compressed_bytes": 10},
                    {"name": "duration", "type": "DOUBLE", "null_count": 0, "total_count": 100,
                     "compressed_bytes": 10}
                ],
                "file_count": 1, "total_bytes": 10, "total_events": 100,
                "degraded_fields": ["status"]
            },
            {
                "name": "db",
                "columns": [],
                "file_count": 1, "total_bytes": 10, "total_events": 5
            }
        ],
        "cached": true
    }))
    .unwrap()
}

/// A query answer with `columns` and `rows`, `total` rows produced.
pub fn query_response(columns: Vec<Column>, rows: Vec<Vec<Value>>, total: usize) -> QueryResponse {
    let returned = rows.len();
    QueryResponse {
        execution: None,
        result: QueryResult { columns, rows },
        pagination: PaginationMeta {
            limit: 200,
            offset: 0,
            returned,
            total,
        },
        degraded_fields: vec![],
        severity_columns: vec![],
    }
}

/// A string cell.
pub fn s(text: &str) -> Value {
    Value::String(text.to_owned())
}

/// A captured result over `columns` with `facts`, live, event-shaped.
pub fn captured(
    query: &str,
    columns: &[(&str, ColumnType)],
    facts: Vec<ColumnFacts>,
    captured_rows: u64,
    total_rows: u64,
) -> CapturedResult {
    CapturedResult {
        id: RESULT_ID.to_owned(),
        query: query.to_owned(),
        // The bounds splice in front, as trawld splices them; the imposed
        // order follows on its own line, so a trailing comment in the
        // caller's text cannot swallow it.
        executed_query: format!(
            "earliest=\"2026-03-14T03:00:00.250000Z\" \
             latest=\"2026-03-14T04:00:00.750000Z\" {query}\n| sort -_time, -_ingested"
        ),
        source: ResultSource::Live,
        shape: ResultShape::Events,
        earliest: Some("2026-03-14T03:00:00.250000Z".to_owned()),
        latest: Some("2026-03-14T04:00:00.750000Z".to_owned()),
        created_at: "2026-03-14T04:00:01.000000Z".to_owned(),
        expires_at: Some("2026-03-14T04:15:01.000000Z".to_owned()),
        state: ResultState::Temporary,
        columns: columns
            .iter()
            .map(|(name, ty)| ResultColumn {
                name: (*name).to_owned(),
                ty: ty.clone(),
                stored_as: StoredAs::Native,
            })
            .collect(),
        captured_rows,
        total_rows,
        wire_bytes: 1000,
        file_bytes: 1000,
        truncated: captured_rows < total_rows,
        column_facts: facts,
        format_version: 1,
        kind: trawl_client::CaptureKind::Search,
        correlation: None,
    }
}

/// Facts for a column with no listed values.
pub fn no_values(distinct: u64) -> ColumnFacts {
    ColumnFacts {
        nulls: 0,
        distinct,
        distinct_capped: distinct > 5,
        values: None,
        min: None,
        max: None,
    }
}

/// Facts for a column holding `values` with their counts.
pub fn values(values: &[(Value, u64)]) -> ColumnFacts {
    ColumnFacts {
        nulls: 0,
        distinct: values.len() as u64,
        distinct_capped: false,
        values: Some(
            values
                .iter()
                .map(|(value, count)| trawl_client::ValueCount {
                    value: value.clone(),
                    count: *count,
                })
                .collect(),
        ),
        min: None,
        max: None,
    }
}

/// A page of `result` holding `rows` from `offset`, every column.
pub fn page(result: &CapturedResult, rows: Vec<Vec<Value>>, offset: u64) -> ResultPage {
    let returned = rows.len();
    ResultPage {
        positions: (0..result.columns.len()).collect(),
        page: QueryResult {
            columns: result
                .columns
                .iter()
                .map(|c| Column::new(c.name.clone(), c.ty.clone()))
                .collect(),
            rows,
        },
        pagination: PaginationMeta {
            limit: 50,
            offset: usize::try_from(offset).unwrap(),
            returned,
            total: usize::try_from(result.captured_rows).unwrap(),
        },
        result: result.clone(),
    }
}

/// A dashboard whose capacity lists `environments`.
pub fn dashboard(environments: Vec<trawl_api::EnvironmentCapacity>) -> DashboardSnapshot {
    use trawl_api::{
        Capacity, HeadroomReading, PressureEvidence, StorageMeasurement, StorageMeasurementStatus,
    };
    let complete = StorageMeasurement {
        status: StorageMeasurementStatus::Complete,
        sample_age_secs: Some(2),
    };
    DashboardSnapshot {
        hostname: "trawl-host".into(),
        listen_addr: "127.0.0.1:5514".into(),
        uptime_secs: 60,
        version: "0.9.3".into(),
        healthy: true,
        pool_capacity: 4,
        pool_active: 0,
        pool_retained: 0,
        hot_buffer_events: 0,
        hot_buffer_max_events: 100_000,
        hot_buffer_bytes: 0,
        hot_buffer_max_bytes: 104_857_600,
        hot_buffer_batches: 0,
        total_queries: 0,
        query_rate: 0.0,
        query_errors: 0,
        query_timeouts: 0,
        ingest_events: 0,
        ingest_rate: 0.0,
        ingest_rejected: 0,
        syslog_enabled: false,
        syslog_events_udp: 0,
        syslog_events_tcp: 0,
        syslog_rate: 0.0,
        syslog_parse_errors: 0,
        syslog_dropped: 0,
        syslog_tcp_connections: 0,
        wal_files: 0,
        wal_bytes: 0,
        wal_measurement: complete,
        last_compaction_secs: None,
        compaction_runs: 0,
        compaction_errors: 0,
        parquet_files: 0,
        parquet_bytes: 0,
        parquet_measurement: complete,
        capacity: Capacity {
            headroom: HeadroomReading {
                measurement: complete,
                filesystems: vec![],
            },
            pressure: PressureEvidence {
                removals_age: 0,
                removals_disk_pressure: 0,
                pressure_attempts: 0,
                last_sweep: None,
            },
            environments,
            growth_excluded: vec![],
        },
        sse_active: 0,
        sse_max: 32,
        scheduler_enabled: false,
        scheduler_schedules: 0,
        recent_queries: vec![],
        active_queries: vec![],
    }
}
