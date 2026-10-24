// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The fixed tool table and dispatch (ADR-0058).
//!
//! [`schema::tools`] is the table, in the order `tools/list` sends it:
//! `orient`, `search`, `read`, `fields`. [`dispatch`] routes one
//! `tools/call` to its tool after decoding the arguments into the tool's
//! input type.

pub mod schema;

use rmcp::ErrorData;
use rmcp::model::{CallToolResult, ContentBlock, JsonObject};
use serde::de::DeserializeOwned;

use crate::backend::Backend;
use schema::{FIELDS, FieldsInput, ORIENT, OrientInput, READ, ReadInput, SEARCH, SearchInput};

/// Run the tool `name` with `arguments` against `backend`.
///
/// Arguments that do not decode into the tool's input are a tool error the
/// model can read and correct, not a protocol error. An unknown tool name
/// is a protocol error, `-32602`.
#[expect(
    clippy::unused_async,
    reason = "every tool is a placeholder that awaits nothing until the tools land"
)]
pub async fn dispatch<B: Backend>(
    name: &str,
    arguments: Option<JsonObject>,
    backend: &B,
) -> Result<CallToolResult, ErrorData> {
    let _ = backend;
    match name {
        ORIENT => Ok(match decode::<OrientInput>(name, arguments) {
            Ok(_input) => not_implemented(name),
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
