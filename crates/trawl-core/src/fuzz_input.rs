// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `parse_emit` fuzz wire encoding, and the pin map it derives.
//!
//! The pin-aware fuzz target feeds one byte string into two places at once:
//! the DSL text the parser reads, and a selector that invents a catalog pin
//! for every name [`crate::field_refs::referenced_fields`] reports out of
//! the parsed query. Both halves are decoded here so the target and the
//! PREPARE fixture in trawl-engine cannot drift: the fixture replays
//! committed cases through this same decoder and asserts they cover every
//! entry of [`CanonicalType::ALL`].
//!
//! Why a production crate carries fuzz plumbing: the fuzz package declares
//! its own `[workspace]` and the root workspace `exclude`s it, so no
//! workspace member can dev-depend on it.
//!
//! # The wire encoding
//!
//! `query \0 selector`, split at the first NUL. With no NUL anywhere, the
//! whole input plays both parts: the query text, and the selector bytes.
//!
//! That fallback is what keeps the committed parse corpus usable as a seed
//! corpus here, though only part of it. Of the files under
//! `crates/trawl-core/fuzz/corpus/parse/`, 1072 of 3842 were both valid
//! UTF-8 and NUL-free when measured on 2026-08-25; the rest are libFuzzer
//! mutation output rather than DSL anyone typed. A NUL-free valid-UTF-8
//! seed decodes to itself byte for byte and gains a pin map derived from
//! its own text, which is real DSL enough to seed the target with.
//! `the_parse_corpus_decodes_within_the_encoding_contract` re-measures on
//! every test run and asserts a floor, not an exact count.

use crate::ast::Query;
use crate::schema::{CanonicalType, FieldTypes};

/// One decoded fuzz input.
#[derive(Debug, Clone, Copy)]
pub struct DecodedCase<'a> {
    /// The DSL text to parse.
    pub query: &'a str,
    /// The bytes that pick a pin per bound field.
    pub selector: &'a [u8],
}

#[must_use]
pub fn decode_case(input: &str) -> DecodedCase<'_> {
    match input.find('\0') {
        // NUL is ASCII, so the split index is always a char boundary.
        Some(at) => DecodedCase {
            query: &input[..at],
            selector: &input.as_bytes()[at + 1..],
        },
        None => DecodedCase {
            query: input,
            selector: input.as_bytes(),
        },
    }
}

/// No admission policy is applied, deliberately. The target's claim is that
/// emission is total over every pin map, not just the reachable ones.
#[must_use]
pub fn derive_field_types(query: &Query, selector: &[u8]) -> FieldTypes {
    let mut pins = FieldTypes::new();
    for (i, field) in crate::field_refs::referenced_fields(query)
        .iter()
        .enumerate()
    {
        let byte = if selector.is_empty() {
            0
        } else {
            selector[i % selector.len()]
        };
        let choice = usize::from(byte) % (CanonicalType::ALL.len() + 1);
        if choice > 0 {
            pins.insert(field, CanonicalType::ALL[choice - 1]);
        }
    }
    pins
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(dsl: &str) -> Query {
        crate::parser::parse(dsl).expect("test DSL should parse")
    }

    /// The count assertion is a floor rather than equality. Committing more
    /// mutation output is a legitimate thing to do to a fuzz corpus.
    #[test]
    fn the_parse_corpus_decodes_within_the_encoding_contract() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fuzz")
            .join("corpus")
            .join("parse");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            // The fuzz package declares its own workspace and the root
            // excludes it, so a sparse checkout or a packaged crate can
            // arrive without this tree at all. Absent is not drift.
            eprintln!("skipping corpus audit: no directory at {}", dir.display());
            return;
        };

        let mut total = 0usize;
        let mut utf8 = 0usize;
        let mut nul_free = 0usize;

        for entry in entries {
            let path = entry.expect("corpus entry should be readable").path();
            if !path.is_file() {
                continue;
            }
            let bytes = std::fs::read(&path).expect("corpus file should be readable");
            total += 1;
            let Ok(text) = std::str::from_utf8(&bytes) else {
                // Never reachable as fuzz input: both targets take `&str`,
                // so libFuzzer discards these before any decoding.
                continue;
            };
            utf8 += 1;

            let name = path.display();
            let decoded = decode_case(text);
            assert!(
                bytes.starts_with(decoded.query.as_bytes()),
                "{name}: the decoded query is not a byte prefix of the file"
            );
            if !bytes.contains(&0) {
                nul_free += 1;
                assert_eq!(decoded.query, text, "{name}: a NUL-free query moved");
                assert_eq!(
                    decoded.selector,
                    bytes.as_slice(),
                    "{name}: a NUL-free selector moved"
                );
            }
        }

        assert!(
            total > 0,
            "corpus directory holds no files: {}",
            dir.display()
        );
        assert!(
            nul_free >= 1000,
            "only {nul_free} of {total} corpus files ({utf8} valid UTF-8) decode to \
             themselves; the module doc measured 1072 and the rationale needs a bulk \
             of real DSL on the no-NUL path"
        );
    }

    #[test]
    fn the_first_nul_separates_the_halves() {
        let decoded = decode_case("service=nginx\0\x01\x02");
        assert_eq!(decoded.query, "service=nginx");
        assert_eq!(decoded.selector, &[1, 2]);
    }

    #[test]
    fn a_leading_nul_leaves_an_empty_query() {
        let decoded = decode_case("\0abc");
        assert_eq!(decoded.query, "");
        assert_eq!(decoded.selector, b"abc");
    }

    #[test]
    fn a_trailing_nul_leaves_an_empty_selector() {
        let decoded = decode_case("service=nginx\0");
        assert_eq!(decoded.query, "service=nginx");
        assert!(decoded.selector.is_empty());
    }

    #[test]
    fn later_nuls_are_ordinary_selector_bytes() {
        let decoded = decode_case("a=1\0\x03\0\x04");
        assert_eq!(decoded.query, "a=1");
        assert_eq!(decoded.selector, &[3, 0, 4]);
    }

    #[test]
    fn an_empty_input_decodes_to_two_empty_halves() {
        let decoded = decode_case("");
        assert_eq!(decoded.query, "");
        assert!(decoded.selector.is_empty());
    }

    #[test]
    fn a_query_binding_no_fields_yields_an_empty_catalog() {
        let pins = derive_field_types(&parse("error last=1h"), b"\x01\x02\x03");
        assert_eq!(pins.iter().count(), 0);
    }

    #[test]
    fn an_empty_selector_leaves_every_field_unpinned() {
        let pins = derive_field_types(&parse("a=1 b=2 c=3"), b"");
        assert_eq!(pins.iter().count(), 0);
    }

    #[test]
    fn a_short_selector_is_reused_cyclically() {
        let pins = derive_field_types(&parse("a=1 b=2 c=3"), &[2]);
        assert_eq!(pins.get("a"), Some(CanonicalType::ALL[1]));
        assert_eq!(pins.get("b"), Some(CanonicalType::ALL[1]));
        assert_eq!(pins.get("c"), Some(CanonicalType::ALL[1]));

        // Two bytes over three sorted fields: a, b, a again.
        let pins = derive_field_types(&parse("a=1 b=2 c=3"), &[1, 0]);
        assert_eq!(pins.get("a"), Some(CanonicalType::ALL[0]));
        assert_eq!(pins.get("b"), None);
        assert_eq!(pins.get("c"), Some(CanonicalType::ALL[0]));
    }

    #[test]
    fn fields_take_selector_bytes_in_sorted_folded_order() {
        let pins = derive_field_types(&parse("Zebra=1 alpha=2 middle=3"), &[1, 2, 3]);
        assert_eq!(pins.get("alpha"), Some(CanonicalType::ALL[0]));
        assert_eq!(pins.get("middle"), Some(CanonicalType::ALL[1]));
        assert_eq!(pins.get("zebra"), Some(CanonicalType::ALL[2]));
    }

    #[test]
    fn every_canonical_type_is_reachable_from_some_byte() {
        let query = parse("a=1");
        for (index, expected) in CanonicalType::ALL.into_iter().enumerate() {
            let byte = u8::try_from(index + 1).expect("ALL is far shorter than 255");
            let pins = derive_field_types(&query, &[byte]);
            assert_eq!(pins.get("a"), Some(expected), "byte {byte}");
        }
        assert_eq!(derive_field_types(&query, &[0]).get("a"), None);

        let modulus = u8::try_from(CanonicalType::ALL.len() + 1)
            .expect("the canonical vocabulary is far shorter than 255");
        assert_eq!(derive_field_types(&query, &[modulus]).get("a"), None);
        assert_eq!(
            derive_field_types(&query, &[modulus + 1]).get("a"),
            Some(CanonicalType::ALL[0])
        );

        let choice = usize::from(255 % modulus);
        let expected = if choice == 0 {
            None
        } else {
            Some(CanonicalType::ALL[choice - 1])
        };
        assert_eq!(derive_field_types(&query, &[255]).get("a"), expected);
    }

    #[test]
    fn decoding_and_deriving_compose_into_one_case() {
        let decoded = decode_case("status>=400 service=nginx\0\x05\x01");
        let query = parse(decoded.query);
        let pins = derive_field_types(&query, decoded.selector);
        // Sorted: service takes byte 5, status takes byte 1.
        assert_eq!(pins.get("service"), Some(CanonicalType::ALL[4]));
        assert_eq!(pins.get("status"), Some(CanonicalType::ALL[0]));
    }

    #[test]
    fn no_admission_policy_is_applied() {
        let pins = derive_field_types(&parse("host=a _severity=error"), &[6]);
        assert_eq!(pins.get("host"), Some(CanonicalType::Severity));
        assert_eq!(pins.get("_severity"), Some(CanonicalType::Severity));
    }
}
