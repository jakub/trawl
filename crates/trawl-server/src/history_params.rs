// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Raw admission for History. Called only after Query permission is checked.
//!
//! Names and the filter use strict form decoding. Pagination retains Axum's
//! existing Serde admission, but sees only selected numeric pairs after the
//! entire raw request has passed the strict checks. Unknown values are opaque.

use axum::extract::Query;
use axum::http::Uri;

use crate::error::ServerError;
use crate::handlers::HistoryParams;

const MAX_FILTER_BYTES: usize = 32768;
// Axum RawQuery supplies the payload after the URL's separator. Any '?' in
// that payload is literal name/value data and counts toward this cap. The
// http::Uri transport ceiling is 65534 bytes for the whole URI; larger payload
// bounds are also exercised directly through the actual handler in tests.
const MAX_RAW_BYTES: usize = 3 * MAX_FILTER_BYTES + 128;
const MAX_PAIRS: usize = 64;

#[derive(Debug)]
pub(crate) struct AdmittedHistoryParams {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub filter: Option<String>,
}

fn invalid() -> ServerError {
    // Never include the raw name, value, decoder error, or query in a response.
    ServerError::BadRequest("invalid history parameters".into())
}

pub(crate) fn parse(raw: &str) -> Result<AdmittedHistoryParams, ServerError> {
    if raw.len() > MAX_RAW_BYTES
        || raw.split('&').filter(|pair| !pair.is_empty()).count() > MAX_PAIRS
    {
        return Err(invalid());
    }

    let mut filter = None;
    let mut numeric_query = String::from("/?");
    for pair in raw.split('&').filter(|pair| !pair.is_empty()) {
        let (raw_name, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        let name = strict_form_decode(raw_name)?;
        match name.as_str() {
            "filter" => {
                if filter.is_some() {
                    return Err(invalid());
                }
                let value = strict_form_decode(raw_value)?;
                if value.len() > MAX_FILTER_BYTES {
                    return Err(invalid());
                }
                filter = Some(value);
            }
            "limit" | "offset" => {
                // Retain duplicates and the untouched value representation.
                // The legacy extractor owns numeric grammar and single decode.
                numeric_query.push_str(&name);
                numeric_query.push('=');
                numeric_query.push_str(raw_value);
                numeric_query.push('&');
            }
            _ => {} // Unknown values must not enter any decoder.
        }
    }

    let uri: Uri = numeric_query.parse().map_err(|_| invalid())?;
    let Query(pagination) = Query::<HistoryParams>::try_from_uri(&uri).map_err(|_| invalid())?;
    Ok(AdmittedHistoryParams {
        limit: pagination.limit,
        offset: pagination.offset,
        filter: filter.filter(|value| !value.is_empty()),
    })
}

fn strict_form_decode(raw: &str) -> Result<String, ServerError> {
    let mut decoded = Vec::with_capacity(raw.len());
    let mut bytes = raw.bytes();
    while let Some(byte) = bytes.next() {
        let byte = match byte {
            b'+' => b' ',
            b'%' => {
                let hi = bytes.next().and_then(hex_digit).ok_or_else(invalid)?;
                let lo = bytes.next().and_then(hex_digit).ok_or_else(invalid)?;
                (hi << 4) | lo
            }
            byte => byte,
        };
        if byte == 0 {
            return Err(invalid());
        }
        decoded.push(byte);
    }
    String::from_utf8(decoded).map_err(|_| invalid())
}

const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_filter_decodes_once_and_preserves_literals() {
        for (raw, expected) in [
            ("", None),
            ("filter", None),
            ("filter=", None),
            ("?filter=x", None),
            ("%3Ffilter=x", None),
            ("?%66ilter=x", None),
            ("filter=+", Some(" ")),
            ("filter=a++b", Some("a  b")),
            ("filter=%2B%25%27%5C_%2E%2A", Some("+%'\\_.*")),
            ("%66ilter=%2531", Some("%31")),
            ("filter=%E6%97%A5%E6%9C%AC%E8%AA%9E", Some("日本語")),
            ("filter=hello=world", Some("hello=world")),
            ("unknown=%FF%00%GG&filter=ok", Some("ok")),
            ("&&filter=ok&&", Some("ok")),
        ] {
            assert_eq!(parse(raw).unwrap().filter.as_deref(), expected, "{raw}");
        }
    }

    #[test]
    fn history_filter_refuses_encoding_duplicates_and_nul() {
        for raw in [
            "filter=a&filter=b",
            "filter=&%66ilter=b",
            "filter&filter",
            "filter=%",
            "filter=%1",
            "filter=%GG",
            "filter=%FF",
            "filter=%E6%97",
            "filter=%C0%80",
            "filter=%00",
            "filter=\0",
            "%GG=ignored",
            "%FF=ignored",
            "%00=ignored",
            "bad\0=ignored",
        ] {
            assert!(parse(raw).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn history_filter_raw_and_decoded_byte_boundaries_are_distinct() {
        let filter = "x".repeat(MAX_FILTER_BYTES);
        assert_eq!(
            parse(&format!("filter={filter}")).unwrap().filter,
            Some(filter)
        );
        assert!(parse(&format!("filter={}x", "x".repeat(MAX_FILTER_BYTES))).is_err());
        // Three-byte characters measure UTF-8 bytes, not character count.
        let unicode = format!("{}xx", "日".repeat(MAX_FILTER_BYTES / 3));
        assert_eq!(unicode.len(), MAX_FILTER_BYTES);
        assert!(parse(&format!("filter={unicode}")).is_ok());
        assert!(parse(&format!("filter={unicode}x")).is_err());
        assert_eq!(
            parse(&format!("filter={}", "%61".repeat(MAX_FILTER_BYTES)))
                .unwrap()
                .filter
                .unwrap()
                .len(),
            MAX_FILTER_BYTES
        );

        let boundary = format!("unknown={}", "x".repeat(MAX_RAW_BYTES - "unknown=".len()));
        assert!(parse(&boundary).is_ok());
        assert!(parse(&format!("?{}", &boundary[..boundary.len() - 1])).is_ok());
        assert!(parse(&format!("?{boundary}")).is_err());
        assert!(parse(&format!("{boundary}x")).is_err());
        assert!(parse(&format!("?{boundary}x")).is_err());
    }

    #[test]
    fn history_filter_pair_limit_counts_only_nonempty_pairs() {
        let boundary = vec!["unknown=%FF"; MAX_PAIRS].join("&");
        assert!(parse(&boundary).is_ok());
        assert!(parse(&format!("&&{boundary}&&")).is_ok());
        assert!(parse(&format!("{boundary}&filter=x")).is_err());
    }

    #[test]
    fn history_filter_keeps_legacy_numeric_admission() {
        let mut cases: Vec<String> = [
            "",
            "?limit=5",
            "?filter=x",
            "%3Flimit=5",
            "%3Ffilter=x",
            "?%6cimit=5",
            "limit=0&offset=001",
            "%6cimit=%31&off%73et=2",
            "limit=%2B1",
            "limit=+1",
            "limit=",
            "limit",
            "offset=-1",
            "offset=%201",
            "offset=1%20",
            "limit=1&limit=2",
            "limit=1&%6cimit=1",
            "offset=1&offset=1",
            "limit=%2531",
            "limit=%GG",
            "limit=%FF",
            "limit=1=2",
            "offset=184467440737095516160",
            "unknown=%FF&limit=2",
            "filter=x&offset=3",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        cases.push(format!("limit={}&offset={}", usize::MAX, usize::MAX));
        for raw in cases {
            let uri = format!("/api/v1/history?{raw}").parse().unwrap();
            let legacy = Query::<HistoryParams>::try_from_uri(&uri)
                .ok()
                .map(|Query(params)| (params.limit, params.offset));
            let admitted = parse(&raw).ok().map(|params| (params.limit, params.offset));
            assert_eq!(admitted, legacy, "{raw}");
        }
    }
}
