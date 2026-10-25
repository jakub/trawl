// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The investigation routes (ADR-0059): axum adapters over
//! [`super::service`].
//!
//! They sit on the `authenticated` router, so every request passes bearer
//! authentication, the trawl grant, the per-key rate and the ADR-0054
//! request count before it reaches a handler here. A handler takes its
//! path, query and body as they arrived, rejection included, and hands
//! them to the service only after the service's authority check would
//! pass: a key without the authority gets 403 whatever it sent. A path,
//! query string or body that does not read as its type is then a 400.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use fleet_auth::VerifiedKey;
use serde::Deserialize;
use trawl_api::investigations::{
    AppendEntry, AppendResponse, CreateInvestigation, FindingStatusUpdate, Investigation,
    InvestigationList, InvestigationRead, PatchInvestigation, RetentionExtension,
};

use super::service;
use crate::error::ServerError;
use crate::state::AppState;

/// `GET /api/v1/investigations`: every investigation.
pub(crate) async fn list(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<Json<InvestigationList>, ServerError> {
    service::list(&state, &verified).await.map(Json)
}

/// `PUT /api/v1/investigations/{id}`: create if absent. 201 when this
/// request created it, 200 for a replay.
pub(crate) async fn create(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(id): Path<String>,
    body: Result<Json<CreateInvestigation>, JsonRejection>,
) -> Result<(StatusCode, Json<Investigation>), ServerError> {
    service::authorize_write(&verified)?;
    let Json(req) = body.map_err(|e| invalid("investigation", &e.body_text()))?;
    let (created, investigation) = service::create(&state, &verified, &id, req).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(investigation)))
}

/// The query parameters of `GET /api/v1/investigations/{id}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadParams {
    since: Option<u64>,
    limit: Option<u64>,
}

/// `GET /api/v1/investigations/{id}?since&limit`: the investigation and the
/// entries changed after `since`.
pub(crate) async fn read(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(id): Path<String>,
    params: Result<Query<ReadParams>, QueryRejection>,
) -> Result<Json<InvestigationRead>, ServerError> {
    service::authorize_read(&verified)?;
    let Query(params) =
        params.map_err(|e| invalid("investigation read parameters", &e.body_text()))?;
    service::read(&state, &verified, &id, params.since, params.limit)
        .await
        .map(Json)
}

/// `POST /api/v1/investigations/{id}/entries`: append a keep, a finding or
/// a note.
pub(crate) async fn append(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(id): Path<String>,
    body: Result<Json<AppendEntry>, JsonRejection>,
) -> Result<(StatusCode, Json<AppendResponse>), ServerError> {
    service::authorize_write(&verified)?;
    let Json(entry) = body.map_err(|e| invalid("entry", &e.body_text()))?;
    let response = service::append(&state, &verified, &id, entry).await?;
    Ok((StatusCode::CREATED, Json(response)))
}

/// `PATCH /api/v1/investigations/{id}`: overwrite the title, the question
/// or the status.
pub(crate) async fn patch(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(id): Path<String>,
    body: Result<Json<PatchInvestigation>, JsonRejection>,
) -> Result<Json<Investigation>, ServerError> {
    service::authorize_write(&verified)?;
    let Json(req) = body.map_err(|e| invalid("patch", &e.body_text()))?;
    service::patch(&state, &verified, &id, req).await.map(Json)
}

/// `PUT /api/v1/investigations/{id}/findings/{seq}/status`: overwrite a
/// finding's status.
pub(crate) async fn finding_status(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    path: Result<Path<(String, u64)>, PathRejection>,
    body: Result<Json<FindingStatusUpdate>, JsonRejection>,
) -> Result<Json<AppendResponse>, ServerError> {
    service::authorize_write(&verified)?;
    let Path((id, seq)) = path.map_err(|e| invalid("finding path", &e.body_text()))?;
    let Json(req) = body.map_err(|e| invalid("finding status", &e.body_text()))?;
    service::set_finding_status(&state, &verified, &id, seq, req)
        .await
        .map(Json)
}

/// `PUT /api/v1/investigations/{id}/retention`: keep the evidence longer
/// (`server_manage`).
pub(crate) async fn retention(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(id): Path<String>,
    body: Result<Json<RetentionExtension>, JsonRejection>,
) -> Result<Json<Investigation>, ServerError> {
    service::authorize_manage(&verified)?;
    let Json(req) = body.map_err(|e| invalid("retention extension", &e.body_text()))?;
    service::extend(&state, &verified, &id, req).await.map(Json)
}

/// `DELETE /api/v1/investigations/{id}`: delete it and its evidence
/// (`server_manage`). 204.
pub(crate) async fn delete(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(id): Path<String>,
) -> Result<StatusCode, ServerError> {
    service::delete(&state, &verified, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A 400 for a path, query string or body that does not read: the
/// extractor's reason, which quotes only what the caller sent.
fn invalid(what: &str, reason: &str) -> ServerError {
    ServerError::BadRequest(format!("invalid {what}: {reason}"))
}
