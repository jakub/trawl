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

/// Why a payload could not be decoded. The two cases stay apart because
/// they are different claims about the link: one was never written by
/// this codec, the other says it was and is not (ADR-0027). Neither is
/// "zero filters" — a payload that decodes to nothing would run a wider
/// query than the link says it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadError {
    /// No `v1.` prefix: not a payload this codec ever wrote.
    NotVersioned,
    /// Versioned, but the base64 or the JSON inside it did not decode.
    Undecodable,
}

/// Decode a payload written by this codec.
pub fn decode_payload(raw: &str) -> Result<Vec<(char, String, String)>, PayloadError> {
    let encoded = raw
        .strip_prefix(VERSION)
        .ok_or(PayloadError::NotVersioned)?;
    let decoded = Base64UrlUnpadded::decode_vec(encoded).map_err(|_| PayloadError::Undecodable)?;
    let filters: Vec<WireFilter> =
        serde_json::from_slice(&decoded).map_err(|_| PayloadError::Undecodable)?;
    Ok(filters
        .into_iter()
        .filter(|f| matches!(f.op, '+' | '-') && !f.field.is_empty())
        .map(|f| (f.op, f.field, f.value))
        .collect())
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
                Ok(vec![('+', field.to_string(), "a,b=c&d\\e".to_string())])
            );
        }
    }

    /// The three answers stay apart: an unversioned payload, a
    /// versioned one that does not decode, and a payload that decodes to
    /// no filters at all. Only the third is an empty filter set, and the
    /// URL reader refuses to run the first two rather than treating them
    /// as one (ADR-0027).
    #[test]
    fn legacy_and_malformed_payloads_are_distinguished() {
        assert_eq!(
            decode_payload("+host=web-01"),
            Err(PayloadError::NotVersioned)
        );
        assert_eq!(decode_payload("nonsense"), Err(PayloadError::NotVersioned));
        assert_eq!(
            decode_payload("v1.not_base64!"),
            Err(PayloadError::Undecodable)
        );
        // Valid base64url whose bytes are not the JSON schema.
        assert_eq!(
            decode_payload("v1.bm90anNvbg"),
            Err(PayloadError::Undecodable)
        );
        assert_eq!(decode_payload("v1."), Err(PayloadError::Undecodable));
        // …and an honestly empty list.
        assert_eq!(
            decode_payload(&encode_payload(std::iter::empty())),
            Ok(Vec::new())
        );
    }
}
