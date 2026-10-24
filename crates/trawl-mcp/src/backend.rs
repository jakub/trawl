// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The typed backend the tools call (ADR-0058).
//!
//! [`Backend`] has one method per trawl-client call a tool uses, with the
//! same arguments and return types, so a tool can only ask trawld what the
//! HTTP API answers. Its one implementation is [`HttpClient`]; tests
//! substitute a recording backend. Each method returns a `Send` future,
//! because rmcp runs every request on a spawned task.

use std::future::Future;

use trawl_client::types::ServiceSchemaResponse;
use trawl_client::{
    CaptureRequest, CaptureResponse, CatalogFieldResponse, CatalogFieldsResponse, ClientError,
    FieldValuesResponse, HealthResponse, HttpClient, QueryResponse, ResultCell, ResultList,
    ResultPage, SchemaResponse, ValidationResponse, WhoAmIResponse,
};

/// The trawld calls the MCP tools make, typed as trawl-client types them.
///
/// Every error is a [`ClientError`]: a refusal from trawld keeps its code,
/// message, request id and `Retry-After` wait for the tool to render.
pub trait Backend: Send + Sync {
    /// The key's identity and effective permissions (`GET /api/v1/whoami`).
    fn whoami(&self) -> impl Future<Output = Result<WhoAmIResponse, ClientError>> + Send;

    /// Daemon health with the HTTP status it came with, always 200 or 503
    /// (`GET /api/v1/health`).
    fn health_with_status(
        &self,
    ) -> impl Future<Output = Result<(u16, HealthResponse), ClientError>> + Send;

    /// The rich per-service schema from the daemon's refresh cache
    /// (`GET /api/v1/schema/services`).
    fn schema_services(
        &self,
    ) -> impl Future<Output = Result<ServiceSchemaResponse, ClientError>> + Send;

    /// The installation's column schema (`GET /api/v1/schema`).
    fn schema(&self) -> impl Future<Output = Result<SchemaResponse, ClientError>> + Send;

    /// Pinned fields with aggregates, optionally for one service
    /// (`GET /api/v1/schema/fields`).
    fn catalog_fields(
        &self,
        service: Option<&str>,
        since_secs: Option<u64>,
        limit: Option<usize>,
    ) -> impl Future<Output = Result<CatalogFieldsResponse, ClientError>> + Send;

    /// One field's pin, a page of its observations and its conflict
    /// evidence (`GET /api/v1/schema/field`).
    fn catalog_field(
        &self,
        name: &str,
        limit: Option<usize>,
        after: Option<&str>,
    ) -> impl Future<Output = Result<CatalogFieldResponse, ClientError>> + Send;

    /// Distinct values of a field, optionally scoped to one service
    /// (`GET /api/v1/schema/values/{field}`).
    fn field_values(
        &self,
        field: &str,
        limit: Option<usize>,
        service: Option<&str>,
    ) -> impl Future<Output = Result<FieldValuesResponse, ClientError>> + Send;

    /// Execute a DSL query with pagination and an explicit display zone
    /// (`POST /api/v1/query`).
    fn query_paginated_tz(
        &self,
        dsl: &str,
        limit: Option<usize>,
        offset: Option<usize>,
        timezone: Option<String>,
    ) -> impl Future<Output = Result<QueryResponse, ClientError>> + Send;

    /// Validate a DSL query without executing it (`POST /api/v1/validate`).
    fn validate(
        &self,
        dsl: &str,
    ) -> impl Future<Output = Result<ValidationResponse, ClientError>> + Send;

    /// Capture a search as an immutable, expiring result
    /// (`POST /api/v1/results`, ADR-0057).
    fn capture(
        &self,
        request: &CaptureRequest,
    ) -> impl Future<Output = Result<CaptureResponse, ClientError>> + Send;

    /// The key's own unexpired captures, newest first
    /// (`GET /api/v1/results`).
    fn list_results(&self) -> impl Future<Output = Result<ResultList, ClientError>> + Send;

    /// One page of a captured result (`GET /api/v1/results/{id}`).
    fn result_page(
        &self,
        id: &str,
        offset: u64,
        limit: u64,
        columns: Option<&[&str]>,
    ) -> impl Future<Output = Result<ResultPage, ClientError>> + Send;

    /// A byte range of one captured cell's wire JSON text
    /// (`GET /api/v1/results/{id}/cells/{row}/{column}`).
    fn result_cell(
        &self,
        id: &str,
        row: u64,
        column: usize,
        offset: Option<u64>,
        length: Option<u64>,
    ) -> impl Future<Output = Result<ResultCell, ClientError>> + Send;
}

impl Backend for HttpClient {
    async fn whoami(&self) -> Result<WhoAmIResponse, ClientError> {
        HttpClient::whoami(self).await
    }

    async fn health_with_status(&self) -> Result<(u16, HealthResponse), ClientError> {
        HttpClient::health_with_status(self).await
    }

    async fn schema_services(&self) -> Result<ServiceSchemaResponse, ClientError> {
        HttpClient::schema_services(self).await
    }

    async fn schema(&self) -> Result<SchemaResponse, ClientError> {
        HttpClient::schema(self).await
    }

    async fn catalog_fields(
        &self,
        service: Option<&str>,
        since_secs: Option<u64>,
        limit: Option<usize>,
    ) -> Result<CatalogFieldsResponse, ClientError> {
        HttpClient::catalog_fields(self, service, since_secs, limit).await
    }

    async fn catalog_field(
        &self,
        name: &str,
        limit: Option<usize>,
        after: Option<&str>,
    ) -> Result<CatalogFieldResponse, ClientError> {
        HttpClient::catalog_field(self, name, limit, after).await
    }

    async fn field_values(
        &self,
        field: &str,
        limit: Option<usize>,
        service: Option<&str>,
    ) -> Result<FieldValuesResponse, ClientError> {
        HttpClient::field_values(self, field, limit, service).await
    }

    async fn query_paginated_tz(
        &self,
        dsl: &str,
        limit: Option<usize>,
        offset: Option<usize>,
        timezone: Option<String>,
    ) -> Result<QueryResponse, ClientError> {
        HttpClient::query_paginated_tz(self, dsl, limit, offset, timezone).await
    }

    async fn validate(&self, dsl: &str) -> Result<ValidationResponse, ClientError> {
        HttpClient::validate(self, dsl).await
    }

    async fn capture(&self, request: &CaptureRequest) -> Result<CaptureResponse, ClientError> {
        HttpClient::capture(self, request).await
    }

    async fn list_results(&self) -> Result<ResultList, ClientError> {
        HttpClient::list_results(self).await
    }

    async fn result_page(
        &self,
        id: &str,
        offset: u64,
        limit: u64,
        columns: Option<&[&str]>,
    ) -> Result<ResultPage, ClientError> {
        HttpClient::result_page(self, id, offset, limit, columns).await
    }

    async fn result_cell(
        &self,
        id: &str,
        row: u64,
        column: usize,
        offset: Option<u64>,
        length: Option<u64>,
    ) -> Result<ResultCell, ClientError> {
        HttpClient::result_cell(self, id, row, column, offset, length).await
    }
}
