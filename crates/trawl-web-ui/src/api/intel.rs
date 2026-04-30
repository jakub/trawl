// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use coastwatch_api_types::claim::ClaimEvidenceView;
use coastwatch_api_types::pagination::{ItemBody, PaginatedBody};
use coastwatch_api_types::story::{
    StoryClaimView, StoryRelationView, StoryView, TimelineEventView,
};
use gloo_net::http::Request;

use super::ApiError;

const BASE: &str = "/api/intel/v1";

pub async fn list_stories(cursor: Option<&str>) -> Result<PaginatedBody<StoryView>, ApiError> {
    let url = match cursor {
        Some(c) => format!("{BASE}/stories?cursor={c}&limit=25"),
        None => format!("{BASE}/stories?limit=25"),
    };
    let resp = Request::get(&url).send().await?;
    match resp.status() {
        200 => resp
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

pub async fn get_story(id: &str) -> Result<ItemBody<StoryView>, ApiError> {
    let resp = Request::get(&format!("{BASE}/stories/{id}")).send().await?;
    match resp.status() {
        200 => resp
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

pub async fn story_timeline(
    id: &str,
    cursor: Option<&str>,
) -> Result<PaginatedBody<TimelineEventView>, ApiError> {
    let url = match cursor {
        Some(c) => format!("{BASE}/stories/{id}/timeline?cursor={c}&limit=20"),
        None => format!("{BASE}/stories/{id}/timeline?limit=20"),
    };
    let resp = Request::get(&url).send().await?;
    match resp.status() {
        200 => resp
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

pub async fn story_claims(
    id: &str,
    cursor: Option<&str>,
) -> Result<PaginatedBody<StoryClaimView>, ApiError> {
    let url = match cursor {
        Some(c) => format!("{BASE}/stories/{id}/claims?cursor={c}&limit=20"),
        None => format!("{BASE}/stories/{id}/claims?limit=20"),
    };
    let resp = Request::get(&url).send().await?;
    match resp.status() {
        200 => resp
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

pub async fn story_relations(id: &str) -> Result<PaginatedBody<StoryRelationView>, ApiError> {
    let resp = Request::get(&format!("{BASE}/stories/{id}/relations"))
        .send()
        .await?;
    match resp.status() {
        200 => resp
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}

pub async fn claim_evidence(
    claim_id: &str,
    cursor: Option<&str>,
) -> Result<PaginatedBody<ClaimEvidenceView>, ApiError> {
    let url = match cursor {
        Some(c) => format!("{BASE}/claims/{claim_id}/evidence?cursor={c}&limit=20"),
        None => format!("{BASE}/claims/{claim_id}/evidence?limit=20"),
    };
    let resp = Request::get(&url).send().await?;
    match resp.status() {
        200 => resp
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string())),
        401 => Err(ApiError::Unauthorized),
        s => Err(ApiError::Status(s)),
    }
}
