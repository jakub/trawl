// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The fixed tool table and dispatch (ADR-0058).
//!
//! [`schema::tools`] is the table, in the order `tools/list` sends it:
//! `orient`, `search`, `read`, `fields`. [`dispatch`] routes one
//! `tools/call` to its tool after decoding the arguments into the tool's
//! input type. Each tool module has a `run` function that takes the
//! backend, the decoded input and the instant it treats as now, so a test
//! can call it with a fixed clock.

pub mod orient;
pub mod schema;

use chrono::Utc;
use rmcp::ErrorData;
use rmcp::model::{CallToolResult, ContentBlock, JsonObject};
use serde::de::DeserializeOwned;
use trawl_client::ClientError;

use crate::backend::Backend;
use schema::{FIELDS, FieldsInput, ORIENT, OrientInput, READ, ReadInput, SEARCH, SearchInput};

/// Run the tool `name` with `arguments` against `backend`, printing times
/// in `display_zone` where a tool prints a display time.
///
/// Arguments that do not decode into the tool's input are a tool error the
/// model can read and correct, not a protocol error. An unknown tool name
/// is a protocol error, `-32602`.
pub async fn dispatch<B: Backend>(
    name: &str,
    arguments: Option<JsonObject>,
    backend: &B,
    display_zone: &str,
) -> Result<CallToolResult, ErrorData> {
    match name {
        ORIENT => Ok(match decode::<OrientInput>(name, arguments) {
            Ok(_input) => orient::run(backend, display_zone, Utc::now()).await,
            Err(refusal) => refusal,
        }),
        SEARCH => Ok(match decode::<SearchInput>(name, arguments) {
            Ok(_input) => not_implemented(name),
            Err(refusal) => refusal,
        }),
        READ => Ok(match decode::<ReadInput>(name, arguments) {
            Ok(_input) => not_implemented(name),
            Err(refusal) => refusal,
        }),
        FIELDS => Ok(match decode::<FieldsInput>(name, arguments) {
            Ok(_input) => not_implemented(name),
            Err(refusal) => refusal,
        }),
        _ => Err(ErrorData::invalid_params(
            "unknown tool; the tools are orient, search, read and fields",
            None,
        )),
    }
}

/// Decode a call's arguments; absent arguments are an empty object.
fn decode<I: DeserializeOwned>(
    name: &str,
    arguments: Option<JsonObject>,
) -> Result<I, CallToolResult> {
    serde_json::from_value(serde_json::Value::Object(arguments.unwrap_or_default()))
        .map_err(|error| crate::render::refusal::invalid_arguments(name, &error))
}

/// The placeholder each tool answers until its implementation lands.
fn not_implemented(name: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!(
        "{name} is not implemented in this checkpoint"
    ))])
}

/// A refusal or failure in a few words, for a tool that carries on without
/// the part that failed: the error code and HTTP status trawld sent, or
/// the class of a failure on the way. Never a library reason or a path.
pub(crate) fn failure_summary(error: &ClientError) -> String {
    match error {
        ClientError::Server {
            status,
            error: envelope,
            ..
        } => {
            let code = serde_json::to_value(&envelope.code)
                .ok()
                .and_then(|code| code.as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".to_owned());
            format!("{code} (HTTP {status})")
        }
        ClientError::Network(_) => "the request to trawld failed in transport".to_owned(),
        ClientError::Parse { .. } => "trawld's response did not parse".to_owned(),
        ClientError::TooLarge { cap, .. } => {
            format!("trawld's response was larger than the {cap}-byte cap")
        }
        ClientError::InvalidCa(_) | ClientError::InvalidUrl(_) => {
            "the profile's server settings are not usable".to_owned()
        }
    }
}
