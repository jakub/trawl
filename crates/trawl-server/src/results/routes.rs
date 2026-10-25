// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The captured-results routes (ADR-0057): axum adapters over
//! [`super::service`] and the correlation reads beside it (ADR-0058).
//!
//! They sit on the `authenticated` router, so every request passes bearer
//! authentication, the trawl grant, the per-key rate and the ADR-0054
//! request count before it reaches a handler here, with no control
//! allowance. A handler extracts the request and answers what the service
//! answers. A query parameter the route does not take, or one that does not
//! read as its type, is a 400.

use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::{Extension, Json};
use fleet_auth::VerifiedKey;
use serde::Deserialize;
use trawl_api::results::{
    CaptureRequest, CaptureResponse, CompareRequest, ContextRequest, CorrelationResponse,
    ResultCell, ResultList, ResultPage, SightingsRequest,
};

use super::{
    compare as compare_read, context as context_read, service, sightings as sightings_read,
};
use crate::error::ServerError;
use crate::state::AppState;
use crate::transport::http::RequestId;

/// `POST /api/v1/results`: capture one query's result.
pub(crate) async fn capture(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Extension(request_id): Extension<RequestId>,
    Json(req): Json<CaptureRequest>,
) -> Result<Json<CaptureResponse>, ServerError> {
    service::capture(&state, &verified, request_id.0, req)
        .await
        .map(Json)
}

/// `POST /api/v1/results/context`: the rows around a focus row, captured
/// (ADR-0058).
pub(crate) async fn context(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Extension(request_id): Extension<RequestId>,
    Json(req): Json<ContextRequest>,
) -> Result<Json<CorrelationResponse>, ServerError> {
    context_read::context(&state, &verified, request_id.0, req)
        .await
        .map(Json)
}

/// `POST /api/v1/results/sightings`: where a literal appears, counted
/// over every match and captured (ADR-0058).
pub(crate) async fn sightings(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Extension(request_id): Extension<RequestId>,
    Json(req): Json<SightingsRequest>,
) -> Result<Json<CorrelationResponse>, ServerError> {
    sightings_read::sightings(&state, &verified, request_id.0, req)
        .await
        .map(Json)
}

/// `POST /api/v1/results/compare`: a filter's group rates across two
/// windows, from one statement, captured (ADR-0058).
pub(crate) async fn compare(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Extension(request_id): Extension<RequestId>,
    Json(req): Json<CompareRequest>,
) -> Result<Json<CorrelationResponse>, ServerError> {
    compare_read::compare(&state, &verified, request_id.0, req)
        .await
        .map(Json)
}

/// `GET /api/v1/results`: the caller's live captures, newest first.
pub(crate) async fn list(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<Json<ResultList>, ServerError> {
    service::list(&state, &verified).await.map(Json)
}

/// The query parameters of `GET /api/v1/results/{id}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PageParams {
    offset: Option<u64>,
    limit: Option<u64>,
    /// Display names, separated by commas.
    columns: Option<String>,
}

/// `GET /api/v1/results/{id}?offset&limit&columns`: one page of rows.
pub(crate) async fn page(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(id): Path<String>,
    params: Result<Query<PageParams>, QueryRejection>,
) -> Result<Json<ResultPage>, ServerError> {
    let Query(params) = params.map_err(|e| invalid("result page parameters", &e.body_text()))?;
    service::page(
        &state,
        &verified,
        &id,
        params.offset,
        params.limit,
        params.columns.as_deref(),
    )
    .await
    .map(Json)
}

/// The query parameters of `GET /api/v1/results/{id}/cells/{row}/{column}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CellParams {
    offset: Option<u64>,
    length: Option<u64>,
}

/// `GET /api/v1/results/{id}/cells/{row}/{column}?offset&length`: a byte
/// range of one cell.
pub(crate) async fn cell(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    path: Result<Path<(String, u64, usize)>, PathRejection>,
    params: Result<Query<CellParams>, QueryRejection>,
) -> Result<Json<ResultCell>, ServerError> {
    let Path((id, row, column)) = path.map_err(|e| invalid("cell path", &e.body_text()))?;
    let Query(params) = params.map_err(|e| invalid("cell parameters", &e.body_text()))?;
    service::cell(
        &state,
        &verified,
        &id,
        row,
        column,
        params.offset,
        params.length,
    )
    .await
    .map(Json)
}

/// A 400 for a path or query string that does not read: the extractor's
/// reason, which quotes only what the caller sent.
fn invalid(what: &str, reason: &str) -> ServerError {
    ServerError::BadRequest(format!("invalid {what}: {reason}"))
}
