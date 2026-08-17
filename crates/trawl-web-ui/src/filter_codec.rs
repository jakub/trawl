// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Versioned, opaque encoding for the search URL's structured filters.
//!
//! The router percent-decodes query parameters before state sees them. A
//! structural comma/equals codec therefore cannot safely carry arbitrary
//! catalog names. JSON gives the payload one schema; unpadded base64url gives
//! it an alphabet `URLSearchParams` leaves untouched.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use base64ct::{Base64UrlUnpadded, Encoding as _};
use serde::{Deserialize, Serialize};

const VERSION: &str = "v1.";

#[derive(Serialize, Deserialize)]
struct WireFilter {
    op: char,
    field: String,
    value: String,
}

pub fn encode_payload<'a>(parts: impl Iterator<Item = (char, &'a str, &'a str)>) -> String {
    let filters: Vec<WireFilter> = parts
        .map(|(op, field, value)| WireFilter {
            op,
            field: field.to_string(),
            value: value.to_string(),
        })
        .collect();
    let json = serde_json::to_vec(&filters).expect("filter strings always serialize as JSON");
    format!("{VERSION}{}", Base64UrlUnpadded::encode_string(&json))
}

/// Decode a payload written by this codec. `None` means it is an older URL;
/// callers retain the legacy reader for those links. A malformed versioned
/// payload is recognized but yields no filters, never a legacy reinterpretation.
pub fn decode_payload(raw: &str) -> Option<Vec<(char, String, String)>> {
    let encoded = raw.strip_prefix(VERSION)?;
    let decoded = Base64UrlUnpadded::decode_vec(encoded).ok();
    let filters: Vec<WireFilter> = decoded
        .as_deref()
        .and_then(|bytes| serde_json::from_slice(bytes).ok())
        .unwrap_or_default();
    Some(
        filters
            .into_iter()
            .filter(|f| matches!(f.op, '+' | '-') && !f.field.is_empty())
            .map(|f| (f.op, f.field, f.value))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_names_and_values_round_trip_without_url_structure() {
        for field in [
            "x&mode=live&junk",
            "a=b",
            "a,b",
            "a\\b",
            "request id",
            "a`b",
            "日本語",
        ] {
            let payload = encode_payload([('+', field, "a,b=c&d\\e")].into_iter());
            assert!(payload.starts_with(VERSION));
            assert!(
                payload.strip_prefix(VERSION).is_some_and(|body| body
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))),
                "{payload}"
            );
            assert_eq!(
                decode_payload(&payload),
                Some(vec![('+', field.to_string(), "a,b=c&d\\e".to_string())])
            );
        }
    }

    #[test]
    fn legacy_and_malformed_payloads_are_distinguished() {
        assert_eq!(decode_payload("+host=web-01"), None);
        assert_eq!(decode_payload("v1.not_base64!"), Some(Vec::new()));
    }
}
