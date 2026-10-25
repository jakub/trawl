// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The captured-results routes (ADR-0057): axum adapters over
//! [`super::service`].
//!
//! They sit on the `authenticated` router, so every request passes bearer
//! authentication, the trawl grant, the per-key rate and the ADR-0054
//! request count before it reaches a handler here, with no control
//! allowance. A handler extracts the request and answers what the service
//! answers. A query parameter the route does not take, or one that does not
//! read as its type, is a 400.
//!
//! No handler binds `Path`, `Query` or `Json` bare: axum would answer its
//! plain-text rejection before the handler ran, and a key without `query`
//! would learn whether its request parsed. Each takes the extractor's
//! `Result`, checks `query` first, then passes the rejection through
//! [`shaped`]. The capture checks `query` with the [`Authorized`]
//! extractor, ahead of its body, so the body of a key without it is never
//! read.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::{Extension, Json};
use fleet_auth::VerifiedKey;
use serde::Deserialize;
use trawl_api::results::{CaptureRequest, CaptureResponse, ResultCell, ResultList, ResultPage};

use super::service;
use crate::error::ServerError;
use crate::policy::{Authorized, needs};
use crate::shape::shaped;
use crate::state::AppState;
use crate::transport::http::RequestId;

/// `POST /api/v1/results`: capture one query's result.
pub(crate) async fn capture(
    State(state): State<AppState>,
    verified: Authorized<needs::Query>,
    Extension(request_id): Extension<RequestId>,
    body: Result<Json<CaptureRequest>, JsonRejection>,
) -> Result<Json<CaptureResponse>, ServerError> {
    let Json(req) = shaped("capture request", body)?;
    service::capture(&state, &verified, request_id.0, req)
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
    id: Result<Path<String>, PathRejection>,
    params: Result<Query<PageParams>, QueryRejection>,
) -> Result<Json<ResultPage>, ServerError> {
    service::require_query(&verified)?;
    let Path(id) = shaped("result path", id)?;
    let Query(params) = shaped("result page parameters", params)?;
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
    service::require_query(&verified)?;
    let Path((id, row, column)) = shaped("cell path", path)?;
    let Query(params) = shaped("cell parameters", params)?;
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
