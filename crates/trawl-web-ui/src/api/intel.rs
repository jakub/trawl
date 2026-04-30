// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use coastwatch_api_types::claim::ClaimEvidenceView;
use coastwatch_api_types::pagination::{ItemBody, PaginatedBody};
use coastwatch_api_types::story::{
    StoryClaimView, StoryRelationView, StoryView, TimelineEventView,
};
use gloo_net::http::Request;
use js_sys::encode_uri_component;

use super::ApiError;

const BASE: &str = "/api/intel/v1";

fn encode_cursor(cursor: &str) -> String {
    encode_uri_component(cursor).into()
}

pub async fn list_stories(cursor: Option<&str>) -> Result<PaginatedBody<StoryView>, ApiError> {
    let url = match cursor {
        Some(c) => format!("{BASE}/stories?cursor={}&limit=25", encode_cursor(c)),
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
        Some(c) => format!(
            "{BASE}/stories/{id}/timeline?cursor={}&limit=20",
            encode_cursor(c)
        ),
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
        Some(c) => format!(
            "{BASE}/stories/{id}/claims?cursor={}&limit=20",
            encode_cursor(c)
        ),
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
        Some(c) => format!(
            "{BASE}/claims/{claim_id}/evidence?cursor={}&limit=20",
            encode_cursor(c)
        ),
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
