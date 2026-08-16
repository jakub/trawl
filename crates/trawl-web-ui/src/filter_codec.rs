// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `?f=` filter payload's codec — the layer BELOW percent-encoding.
//!
//! # The decode pipeline
//!
//! ```text
//! browser address bar   "…&f=%2Bhost%3Dweb-01"
//!   -> URLSearchParams  percent-DECODES once
//!   -> use_query_map    hands leptos the decoded payload: "+host=web-01"
//!   -> decode_payload   splits and unescapes THIS module's escapes
//! ```
//!
//! The router's decode is the reason a percent-encoded field name cannot
//! carry structure: `%3D` arrives here as a bare `=`, and the first `=`
//! is again read as the field/value separator. Since a catalog key may
//! contain any byte (ADR-0013 ruling 7), a name carrying `=`, `,` or `&`
//! would corrupt the state a reload replays.
//!
//! So structure is escaped in an alphabet percent-decoding never produces
//! or consumes — a backslash and one letter — and the WHOLE payload is
//! percent-encoded once when written into the URL. The router's single
//! decode returns those escapes intact, and only then does this module
//! split. Two layers, disjoint alphabets, one decode each.
//!
//! Old URLs written before this scheme carry no backslashes at all, so
//! unescaping is the identity and they decode unchanged.

// The only CALLER is the wasm32-gated `state::query`; the codec itself is
// pure and its tests run natively — the `service_card_fmt` arrangement.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// The escape marker. Chosen because percent-decoding neither produces
/// nor consumes it, so it survives the router untouched.
const ESC: char = '\\';

/// Escape the two characters that carry structure in the payload, and the
/// marker itself.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            ESC => out.push_str("\\\\"),
            ',' => out.push_str("\\c"),
            '=' => out.push_str("\\e"),
            _ => out.push(c),
        }
    }
    out
}

/// The inverse. An UNKNOWN escape keeps both characters verbatim rather
/// than dropping either: a payload this module did not write must decode
/// to something, never to nothing.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != ESC {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('c') => out.push(','),
            Some('e') => out.push('='),
            Some(other) if other != ESC => {
                // An escape this module did not write keeps BOTH
                // characters: a payload from elsewhere must decode to
                // something, never to nothing.
                out.push(ESC);
                out.push(other);
            }
            // a doubled marker, or a trailing one at end of input
            Some(_) | None => out.push(ESC),
        }
    }
    out
}

/// Render `(op, field, value)` triples as the payload that goes into
/// `f=`, BEFORE percent-encoding.
pub fn encode_payload<'a>(parts: impl Iterator<Item = (char, &'a str, &'a str)>) -> String {
    parts
        .map(|(op, field, value)| format!("{op}{}={}", escape(field), escape(value)))
        .collect::<Vec<_>>()
        .join(",")
}

/// Parse the payload the ROUTER hands back — already percent-decoded.
///
/// Every component's `,` and `=` are escaped, so a literal one is always
/// structural: split on `,`, take the FIRST `=`, unescape both halves.
/// A piece with no op prefix, no `=`, or an empty field is skipped rather
/// than failing the whole state.
pub fn decode_payload(raw: &str) -> Vec<(char, String, String)> {
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split(',')
        .filter_map(|piece| {
            let mut chars = piece.chars();
            let op = chars.next()?;
            if op != '+' && op != '-' {
                return None;
            }
            let rest = chars.as_str();
            let eq = rest.find('=')?;
            let field = unescape(&rest[..eq]);
            if field.is_empty() {
                return None;
            }
            Some((op, field, unescape(&rest[eq + 1..])))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{decode_payload, encode_payload};

    fn round_trip(field: &str, value: &str) -> (char, String, String) {
        let payload = encode_payload([('+', field, value)].into_iter());
        // The router percent-decodes the payload before we see it; this
        // codec's escapes are not percent escapes, so that decode is the
        // identity over them and feeding `payload` straight in models it
        // exactly.
        let decoded = decode_payload(&payload);
        assert_eq!(decoded.len(), 1, "payload: {payload:?}");
        decoded.into_iter().next().unwrap()
    }

    /// A hostile FIELD name round-trips unchanged and contributes no
    /// extra URL parameters — the defect was a raw `&`/`=` ending the
    /// parameter and injecting its own.
    #[test]
    fn a_hostile_field_name_round_trips_and_injects_nothing() {
        for field in [
            "x&mode=live&junk",
            "a=b",
            "a,b",
            "a\\b",
            "request id",
            "a`b",
            "héllo",
        ] {
            let (op, got_field, got_value) = round_trip(field, "web-01");
            assert_eq!(op, '+');
            assert_eq!(got_field, field);
            assert_eq!(got_value, "web-01");

            // no structural character survives unescaped in the payload
            let payload = encode_payload([('+', field, "web-01")].into_iter());
            let structural = payload.matches('=').count() + payload.matches(',').count();
            assert_eq!(structural, 1, "only the separator is bare: {payload:?}");
        }
    }

    /// Values keep the same guarantee, and both halves stay independent.
    #[test]
    fn hostile_values_round_trip_too() {
        for value in ["a,b", "a=b", "a\\,b", "", "x&y"] {
            let (_, field, got) = round_trip("host", value);
            assert_eq!(field, "host");
            assert_eq!(got, value);
        }
    }

    /// Several filters keep their boundaries.
    #[test]
    fn multiple_filters_keep_their_boundaries() {
        let payload = encode_payload([('+', "a,b", "1=2"), ('-', "host", "web-01")].into_iter());
        assert_eq!(
            decode_payload(&payload),
            vec![
                ('+', "a,b".to_string(), "1=2".to_string()),
                ('-', "host".to_string(), "web-01".to_string()),
            ]
        );
    }

    /// A URL written BEFORE this scheme carries no escapes, so it decodes
    /// unchanged — and a malformed one is skipped, never explosive.
    #[test]
    fn older_and_malformed_payloads_decode_non_explosively() {
        assert_eq!(
            decode_payload("+host=web-01,-source=auth.log"),
            vec![
                ('+', "host".to_string(), "web-01".to_string()),
                ('-', "source".to_string(), "auth.log".to_string()),
            ]
        );
        // an unknown escape keeps both characters rather than vanishing
        assert_eq!(
            decode_payload("+ho\\zst=v"),
            vec![('+', "ho\\zst".to_string(), "v".to_string())]
        );
        // pieces with no op, no `=`, or an empty field are skipped
        assert_eq!(decode_payload("host=v"), vec![]);
        assert_eq!(decode_payload("+hostv"), vec![]);
        assert_eq!(decode_payload("+=v"), vec![]);
        assert_eq!(decode_payload(""), vec![]);
        assert_eq!(decode_payload("+"), vec![]);
    }
}
