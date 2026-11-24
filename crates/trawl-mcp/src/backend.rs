// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The typed backend the tools call (ADR-0058).
//! Each method returns a `Send` future,
//! because rmcp runs every request on a spawned task.

use std::future::Future;

use trawl_api::CapabilitiesResponse;
use trawl_client::types::ServiceSchemaResponse;
use trawl_client::{
    AppendEntry, AppendResponse, CaptureRequest, CaptureResponse, CatalogFieldResponse,
    CatalogFieldsResponse, ClientError, CompareRequest, ContextRequest, CorrelationResponse,
    CreateInvestigation, DashboardSnapshot, FieldValuesResponse, FindingStatusUpdate,
    HealthResponse, HttpClient, Investigation, InvestigationList, InvestigationRead,
    PatchInvestigation, PatchResponse, QueryResponse, ResultCell, ResultList, ResultPage,
    RetentionExtension, SchemaResponse, SightingsRequest, ValidationResponse, WhoAmIResponse,
};

pub trait Backend: Send + Sync {
    fn whoami(&self) -> impl Future<Output = Result<WhoAmIResponse, ClientError>> + Send;

    /// Daemon health with the HTTP status it came with, always 200 or 503.
    fn health_with_status(
        &self,
    ) -> impl Future<Output = Result<(u16, HealthResponse), ClientError>> + Send;

    fn capabilities(
        &self,
    ) -> impl Future<Output = Result<CapabilitiesResponse, ClientError>> + Send;

    fn dashboard(&self) -> impl Future<Output = Result<DashboardSnapshot, ClientError>> + Send;

    fn schema_services(
        &self,
    ) -> impl Future<Output = Result<ServiceSchemaResponse, ClientError>> + Send;

    fn schema(&self) -> impl Future<Output = Result<SchemaResponse, ClientError>> + Send;

    fn catalog_fields(
        &self,
        service: Option<&str>,
        since_secs: Option<u64>,
        limit: Option<usize>,
    ) -> impl Future<Output = Result<CatalogFieldsResponse, ClientError>> + Send;

    fn catalog_field(
        &self,
        name: &str,
        limit: Option<usize>,
        after: Option<&str>,
    ) -> impl Future<Output = Result<CatalogFieldResponse, ClientError>> + Send;

    fn field_values(
        &self,
        field: &str,
        limit: Option<usize>,
        service: Option<&str>,
    ) -> impl Future<Output = Result<FieldValuesResponse, ClientError>> + Send;

    fn query_paginated(
        &self,
        dsl: &str,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> impl Future<Output = Result<QueryResponse, ClientError>> + Send;

    fn validate(
        &self,
        dsl: &str,
    ) -> impl Future<Output = Result<ValidationResponse, ClientError>> + Send;

    fn capture(
        &self,
        request: &CaptureRequest,
    ) -> impl Future<Output = Result<CaptureResponse, ClientError>> + Send;

    fn context(
        &self,
        request: &ContextRequest,
    ) -> impl Future<Output = Result<CorrelationResponse, ClientError>> + Send;

    fn sightings(
        &self,
        request: &SightingsRequest,
    ) -> impl Future<Output = Result<CorrelationResponse, ClientError>> + Send;

    fn compare(
        &self,
        request: &CompareRequest,
    ) -> impl Future<Output = Result<CorrelationResponse, ClientError>> + Send;

    fn list_results(&self) -> impl Future<Output = Result<ResultList, ClientError>> + Send;

    fn result_page(
        &self,
        id: &str,
        offset: u64,
        limit: u64,
        columns: Option<&[&str]>,
    ) -> impl Future<Output = Result<ResultPage, ClientError>> + Send;

    /// A byte range of one captured cell's wire JSON text.
    fn result_cell(
        &self,
        id: &str,
        row: u64,
        column: usize,
        offset: Option<u64>,
        length: Option<u64>,
    ) -> impl Future<Output = Result<ResultCell, ClientError>> + Send;

    fn list_investigations(
        &self,
    ) -> impl Future<Output = Result<InvestigationList, ClientError>> + Send;

    /// Create an investigation if absent; `true` when this call created it.
    fn create_investigation(
        &self,
        id: &str,
        request: &CreateInvestigation,
    ) -> impl Future<Output = Result<(bool, Investigation), ClientError>> + Send;

    /// An investigation and its entries changed after `since`.
    fn read_investigation(
        &self,
        id: &str,
        since: u64,
        limit: Option<u64>,
    ) -> impl Future<Output = Result<InvestigationRead, ClientError>> + Send;

    fn append_entry(
        &self,
        id: &str,
        entry: &AppendEntry,
    ) -> impl Future<Output = Result<AppendResponse, ClientError>> + Send;

    fn patch_investigation(
        &self,
        id: &str,
        request: &PatchInvestigation,
    ) -> impl Future<Output = Result<PatchResponse, ClientError>> + Send;

    fn set_finding_status(
        &self,
        id: &str,
        finding_seq: u64,
        request: &FindingStatusUpdate,
    ) -> impl Future<Output = Result<AppendResponse, ClientError>> + Send;

    fn extend_retention(
        &self,
        id: &str,
        request: &RetentionExtension,
    ) -> impl Future<Output = Result<Investigation, ClientError>> + Send;

    fn delete_investigation(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<(), ClientError>> + Send;
}

impl Backend for HttpClient {
    async fn whoami(&self) -> Result<WhoAmIResponse, ClientError> {
        HttpClient::whoami(self).await
    }

    async fn health_with_status(&self) -> Result<(u16, HealthResponse), ClientError> {
        HttpClient::health_with_status(self).await
    }

    async fn capabilities(&self) -> Result<CapabilitiesResponse, ClientError> {
        HttpClient::capabilities(self).await
    }

    async fn dashboard(&self) -> Result<DashboardSnapshot, ClientError> {
        HttpClient::dashboard(self).await
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

    async fn query_paginated(
        &self,
        dsl: &str,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<QueryResponse, ClientError> {
        HttpClient::query_paginated(self, dsl, limit, offset).await
    }

    async fn validate(&self, dsl: &str) -> Result<ValidationResponse, ClientError> {
        HttpClient::validate(self, dsl).await
    }

    async fn capture(&self, request: &CaptureRequest) -> Result<CaptureResponse, ClientError> {
        HttpClient::capture(self, request).await
    }

    async fn context(&self, request: &ContextRequest) -> Result<CorrelationResponse, ClientError> {
        HttpClient::context(self, request).await
    }

    async fn sightings(
        &self,
        request: &SightingsRequest,
    ) -> Result<CorrelationResponse, ClientError> {
        HttpClient::sightings(self, request).await
    }

    async fn compare(&self, request: &CompareRequest) -> Result<CorrelationResponse, ClientError> {
        HttpClient::compare(self, request).await
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

    async fn list_investigations(&self) -> Result<InvestigationList, ClientError> {
        HttpClient::list_investigations(self).await
    }

    async fn create_investigation(
        &self,
        id: &str,
        request: &CreateInvestigation,
    ) -> Result<(bool, Investigation), ClientError> {
        HttpClient::create_investigation(self, id, request).await
    }

    async fn read_investigation(
        &self,
        id: &str,
        since: u64,
        limit: Option<u64>,
    ) -> Result<InvestigationRead, ClientError> {
        HttpClient::read_investigation(self, id, since, limit).await
    }

    async fn append_entry(
        &self,
        id: &str,
        entry: &AppendEntry,
    ) -> Result<AppendResponse, ClientError> {
        HttpClient::append_entry(self, id, entry).await
    }

    async fn patch_investigation(
        &self,
        id: &str,
        request: &PatchInvestigation,
    ) -> Result<PatchResponse, ClientError> {
        HttpClient::patch_investigation(self, id, request).await
    }

    async fn set_finding_status(
        &self,
        id: &str,
        finding_seq: u64,
        request: &FindingStatusUpdate,
    ) -> Result<AppendResponse, ClientError> {
        HttpClient::set_finding_status(self, id, finding_seq, request).await
    }

    async fn extend_retention(
        &self,
        id: &str,
        request: &RetentionExtension,
    ) -> Result<Investigation, ClientError> {
        HttpClient::extend_retention(self, id, request).await
    }

    async fn delete_investigation(&self, id: &str) -> Result<(), ClientError> {
        HttpClient::delete_investigation(self, id).await
    }
}
