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

use std::fmt;

use base64ct::{Base64UrlUnpadded, Encoding as _};
use serde::de::value::MapAccessDeserializer;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

const VERSION: &str = "v1.";

#[derive(Serialize)]
struct WireFilter {
    op: char,
    field: String,
    value: String,
}

/// The record's fields, in the one shape this codec writes. Separate
/// from [`WireFilter`] only so the manual reader below can borrow serde's
/// derived field handling without also inheriting the derived
/// deserializer's positional-array arm.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFilterFields {
    op: char,
    field: String,
    value: String,
}

/// A record is an OBJECT, never a JSON array.
///
/// serde's derived deserializer accepts both shapes: `{"op":"+",…}` and
/// the positional `["+","host","web-01"]`. The array form is three to
/// four times denser than the object form this codec writes, so a
/// payload inside the raw `f` cap could decode into a filter set whose
/// canonical re-encoding is well past it — a link the reader accepts and
/// the producer cannot write back. Asking for a map closes that gap at
/// the shape, and `decode_filters` re-checks the canonical size anyway.
impl<'de> Deserialize<'de> for WireFilter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectOnly;

        impl<'de> Visitor<'de> for ObjectOnly {
            type Value = WireFilter;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a filter object with op, field and value")
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                let fields = WireFilterFields::deserialize(MapAccessDeserializer::new(map))?;
                Ok(WireFilter {
                    op: fields.op,
                    field: fields.field,
                    value: fields.value,
                })
            }
        }

        deserializer.deserialize_map(ObjectOnly)
    }
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
    /// Decoded, but a record is not one this codec writes: an operator
    /// other than `+`/`-`, or an empty field name. Dropping the record
    /// and returning the rest would run a query the link does not
    /// describe, which is the whole thing ADR-0027 refuses.
    InvalidRecord,
}

/// Decode a payload written by this codec.
pub fn decode_payload(raw: &str) -> Result<Vec<(char, String, String)>, PayloadError> {
    let encoded = raw
        .strip_prefix(VERSION)
        .ok_or(PayloadError::NotVersioned)?;
    let decoded = Base64UrlUnpadded::decode_vec(encoded).map_err(|_| PayloadError::Undecodable)?;
    let filters: Vec<WireFilter> =
        serde_json::from_slice(&decoded).map_err(|_| PayloadError::Undecodable)?;
    if filters
        .iter()
        .any(|f| !matches!(f.op, '+' | '-') || f.field.is_empty())
    {
        return Err(PayloadError::InvalidRecord);
    }
    Ok(filters
        .into_iter()
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

    /// The positional array form serde's derived reader would have
    /// accepted. It is not a shape this codec writes, and it is dense
    /// enough that a payload inside the raw cap decodes into filters
    /// whose object-shaped re-encoding is far past it.
    #[test]
    fn a_positional_array_record_is_not_a_filter() {
        for json in [
            r#"[["+","host","web-01"]]"#,
            // Mixed shapes fail whole, like every other bad record.
            r#"[{"op":"+","field":"host","value":"web-01"},["-","source","auth.log"]]"#,
        ] {
            let payload = format!(
                "{VERSION}{}",
                Base64UrlUnpadded::encode_string(json.as_bytes())
            );
            assert_eq!(
                decode_payload(&payload),
                Err(PayloadError::Undecodable),
                "{json}"
            );
        }
    }

    /// A record this codec would never write fails the WHOLE payload.
    /// Skipping it and returning the rest was the same lie by another
    /// route: the link says two filters, the query carries one.
    #[test]
    fn an_unwritable_record_fails_the_whole_payload() {
        for parts in [
            vec![('x', "host", "prod")],
            vec![('+', "", "prod")],
            // …including when the other records are perfectly good.
            vec![('+', "host", "web-01"), ('x', "source", "auth.log")],
            vec![('+', "host", "web-01"), ('-', "", "auth.log")],
        ] {
            let payload = encode_payload(parts.iter().copied());
            assert_eq!(
                decode_payload(&payload),
                Err(PayloadError::InvalidRecord),
                "{parts:?}"
            );
        }
    }
}
