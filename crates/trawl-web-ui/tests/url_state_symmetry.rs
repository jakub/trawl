// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native contract test for the `?f=` filter parameter's encoding
//! symmetry (issue #78).
//!
//! `state::query` is wasm32-gated — it encodes through `js_sys` — so the
//! round trip cannot RUN natively. What can be pinned natively is the
//! invariant that made it wrong: the field name went into the URL raw
//! while the value was encoded, so a catalog key carrying `&` or `=`
//! (spellable since ADR-0013 ruling 7) ended the parameter and injected
//! its own, and a reload executed a different state than the facet added.
//!
//! Same arrangement as `css_move_invariant.rs`: read the source, assert
//! the property the code must keep.

const SOURCE: &str = include_str!("../src/state/query.rs");

/// Both halves of a filter go through the SAME codec, in both directions.
#[test]
fn filter_field_and_value_share_one_codec() {
    let encode = SOURCE
        .split_once("fn encode_filters")
        .expect("encode_filters exists")
        .1;
    let encode = &encode[..encode.find("\n}").expect("function ends")];
    assert_eq!(
        encode.matches("encode_component(").count(),
        2,
        "the field and the value must both be encoded:\n{encode}"
    );
    let formatted = encode
        .lines()
        .find(|l| l.contains("format!("))
        .expect("the parameter is built with format!");
    assert!(
        !formatted.contains("f.field"),
        "the field must not reach the URL raw: {formatted}"
    );

    let decode = SOURCE
        .split_once("fn decode_filters")
        .expect("decode_filters exists")
        .1;
    let decode = &decode[..decode.find("\n}").expect("function ends")];
    assert_eq!(
        decode.matches("decode_component(").count(),
        2,
        "the decode must be symmetric:\n{decode}"
    );
}

/// The codec escapes the two characters that carry structure — `,`
/// between filters and `=` between a field and its value — and decoding
/// a malformed escape falls back to the raw text rather than dropping
/// the filter, so a URL written before this encoding still reads back.
#[test]
fn the_codec_is_structural_and_non_explosive() {
    let codec = SOURCE
        .split_once("fn encode_component")
        .expect("encode_component exists")
        .1;
    assert!(
        codec.contains("encode_uri_component") && codec.contains(r#".replace(',', "%2C")"#),
        "the encoder must escape the separators"
    );
    let decoder = SOURCE
        .split_once("fn decode_component")
        .expect("decode_component exists")
        .1;
    let decoder = &decoder[..decoder.find("\n}").expect("function ends")];
    assert!(
        decoder.contains("unwrap_or_else(|| raw.to_string())"),
        "a malformed escape must decode to itself, not drop the filter:\n{decoder}"
    );
}
