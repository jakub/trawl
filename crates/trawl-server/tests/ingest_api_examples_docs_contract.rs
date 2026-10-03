// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The API reference's ingest response examples are wire values. Each
//! JSON example under "Ingest events" and "Preview ingest" must decode as
//! the `trawl_api` type the route answers with and encode back to the same
//! JSON, so a renamed, misspelled, or invented field fails here. Every
//! `reason` an example shows must be a `RejectReason` wire code, and every
//! repair an example shows a `RepairCode`, imported rather than copied.
//!
//! The preview example was captured from a real trawld; only `arrival`
//! and `_ingested` vary between runs, and both are free-form strings to
//! the wire type.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use trawl_api::IngestResponse;
use trawl_api::ingest_preview::{PreviewEvent, PreviewResponse};
use trawl_server::ingest::envelope::{RejectReason, RepairCode};

const API: &str = include_str!("../../../docs/src/content/docs/reference/api.md");

/// The text under the `### {heading}` section, up to the next heading of
/// level 2 or 3.
fn section(heading: &str) -> &'static str {
    let marker = format!("\n### {heading}\n");
    let start = API
        .find(&marker)
        .unwrap_or_else(|| panic!("the API reference has a {heading:?} section"))
        + marker.len();
    let rest = &API[start..];
    let end = ["\n## ", "\n### "]
        .iter()
        .filter_map(|next| rest.find(next))
        .min()
        .unwrap_or(rest.len());
    &rest[..end]
}

/// Every fenced `json` block in `text`, in document order.
fn json_blocks(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("```json\n") {
        let body = &rest[open + "```json\n".len()..];
        let close = body.find("\n```").expect("a closed json block");
        blocks.push(&body[..close]);
        rest = &body[close..];
    }
    blocks
}

/// Decode `example` as `T` and check it encodes back to the same JSON.
/// Serde ignores unknown fields when decoding, so the round trip is what
/// catches a field the wire type does not have.
fn decode_exactly<T: DeserializeOwned + Serialize>(heading: &str, example: &str) -> T {
    let documented: Value = serde_json::from_str(example)
        .unwrap_or_else(|e| panic!("{heading:?} example is not JSON: {e}\n{example}"));
    let decoded: T = serde_json::from_value(documented.clone()).unwrap_or_else(|e| {
        panic!("{heading:?} example does not decode as the wire type: {e}\n{example}")
    });
    let encoded = serde_json::to_value(&decoded).expect("encode the wire type");
    assert_eq!(
        encoded, documented,
        "{heading:?} example has fields the wire type drops or spells differently"
    );
    decoded
}

fn assert_reject_reason(heading: &str, reason: &str) {
    assert!(
        RejectReason::ALL
            .iter()
            .any(|known| known.as_str() == reason),
        "{heading:?} example shows reason {reason:?}, which is not a RejectReason wire code"
    );
}

fn assert_repair_code(heading: &str, code: &str) {
    assert!(
        RepairCode::ALL.iter().any(|known| known.as_str() == code),
        "{heading:?} example shows repair {code:?}, which is not a RepairCode wire code"
    );
}

#[test]
fn ingest_response_example_is_the_wire_type_with_real_reasons() {
    let heading = "Ingest events";
    let examples = json_blocks(section(heading));
    assert!(!examples.is_empty(), "no JSON example under {heading:?}");
    let mut reasons = 0;
    for example in examples {
        let response: IngestResponse = decode_exactly(heading, example);
        for error in &response.errors {
            assert_reject_reason(heading, &error.reason);
            reasons += 1;
        }
    }
    assert!(reasons > 0, "the {heading:?} examples show no reason");
}

#[test]
fn preview_response_example_is_the_wire_type_with_real_codes() {
    let heading = "Preview ingest";
    let examples = json_blocks(section(heading));
    assert!(!examples.is_empty(), "no JSON example under {heading:?}");
    for example in examples {
        let response: PreviewResponse = decode_exactly(heading, example);
        let mut accepted = 0;
        for event in &response.events {
            match event {
                PreviewEvent::Accepted {
                    event,
                    repairs,
                    lineage,
                    ..
                } => {
                    accepted += 1;
                    for code in repairs {
                        assert_repair_code(heading, code);
                    }
                    for change in &lineage.fields {
                        if let Some(code) = &change.code {
                            assert_repair_code(heading, code);
                        }
                    }
                    assert_eq!(
                        event.get("_repairs").and_then(Value::as_str),
                        Some(repairs.join(",").as_str()).filter(|joined| !joined.is_empty()),
                        "{heading:?} example's repairs disagree with its event's _repairs"
                    );
                    assert_eq!(
                        event.get("_ingested").and_then(Value::as_str),
                        Some(response.arrival.as_str()),
                        "{heading:?} example's arrival is not its events' _ingested"
                    );
                }
                PreviewEvent::Rejected { reason, .. } => assert_reject_reason(heading, reason),
            }
        }
        assert_eq!(
            (response.accepted, response.rejected),
            (accepted, response.events.len() - accepted),
            "{heading:?} example's counts disagree with its events"
        );
    }
}
