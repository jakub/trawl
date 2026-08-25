// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `parse_emit` fuzz wire encoding, and the pin map it derives.
//!
//! The pin-aware fuzz target feeds one byte string into two places at once:
//! the DSL text the parser reads, and a selector that invents a catalog pin
//! for every field the parsed query binds. Both halves are decoded here, so
//! the target and the PREPARE fixture in trawl-engine cannot drift. The
//! fixture replays committed cases through this same decoder and asserts
//! they cover every entry of [`CanonicalType::ALL`]; if the encoding
//! lived in the fuzz target, the fixture would be re-implementing it and the
//! two copies would answer differently the first time either changed.
//!
//! Why a production crate carries fuzz plumbing: the fuzz package declares
//! its own `[workspace]` and the root workspace `exclude`s it, so no
//! workspace member can dev-depend on it. Cargo refuses outright with
//! `multiple workspace roots found in the same workspace` (probed, not
//! assumed). [`crate::field_refs`] is the local precedent for a small public
//! module that exists to serve one consumer.
//!
//! # The wire encoding
//!
//! `query \0 selector`, split at the FIRST NUL. With no NUL anywhere, the
//! whole input is BOTH halves: the query text, and the selector bytes.
//!
//! That fallback is the load-bearing choice. All 3842 committed seeds under
//! `crates/trawl-core/fuzz/corpus/parse/` are NUL-free DSL, so they decode
//! to themselves byte for byte and each one immediately invents a
//! non-trivial pin map out of its own text. Reserving a length prefix or a
//! fixed header would have eaten bytes off the front of every seed and
//! turned valid DSL into parse errors, throwing away the corpus the parse
//! target spent its runs building. libFuzzer reaches the independently
//! mutable form on its own by inserting a NUL, which it does constantly.

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

/// Split a fuzz input into its query text and its pin selector.
///
/// The first NUL is the separator: everything before it is the query,
/// everything after it is the selector. Later NULs are ordinary selector
/// bytes with value 0, which is a meaningful value there (it leaves a field
/// unpinned), so there is nothing to escape and no second parse.
///
/// With no NUL at all the input plays both parts, which is what keeps the
/// parse corpus usable as a seed corpus here. See the module docs.
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

/// Invent a catalog pin map for the fields `query` binds.
///
/// Field *i* of [`crate::field_refs::referenced_fields`] reads
/// `selector[i % selector.len()]`, or 0 when the selector is empty.
/// `referenced_fields` returns a sorted set of ASCII-folded catalog keys, so
/// the same query always lands the same selector byte on the same name.
/// That determinism is what makes the input mutable in the useful way: flip
/// one byte, repin one field, and the crash that appears is attributable to
/// that pin rather than to a reshuffle of all of them.
///
/// The byte maps modulo `ALL.len() + 1`: 0 leaves the field UNPINNED
/// (absent from the map, which is its own case — the emitter's pin-blind
/// path), and `n` picks `ALL[n - 1]`. The modulus is derived from
/// [`CanonicalType::ALL`] rather than written out, so adding a canonical
/// type widens the fuzzer's reach without anyone remembering to edit a
/// number here.
///
/// No admission policy is applied, deliberately. This does not consult
/// `schema::is_contract_typed` and does not skip reserved `_`-prefixed
/// names, so it will happily pin `host` to SEVERITY, which production
/// refuses. The target's claim is that emission is TOTAL over every pin
/// map, not just the reachable ones: over-approximating costs some fuzzer
/// time on maps the catalog would never hand out, while under-approximating
/// would let a panic hide behind a rule that could be relaxed later.
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

    /// The corpus-compatibility case, and the reason the no-NUL fallback
    /// exists: a committed parse seed is DSL with no NUL in it, and it has
    /// to survive the decode unchanged while still carrying a selector.
    #[test]
    fn a_nul_free_input_is_both_halves() {
        let decoded = decode_case("service=nginx status>=400");
        assert_eq!(decoded.query, "service=nginx status>=400");
        assert_eq!(decoded.selector, b"service=nginx status>=400");
    }

    #[test]
    fn the_first_nul_separates_the_halves() {
        let decoded = decode_case("service=nginx\0\x01\x02");
        assert_eq!(decoded.query, "service=nginx");
        assert_eq!(decoded.selector, &[1, 2]);
    }

    /// A leading NUL is how libFuzzer asks for an empty query with a live
    /// selector. Nothing downstream may treat it as "no separator".
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

    /// Later NULs are selector bytes, not separators. Byte 0 means
    /// "unpinned", so they carry information and must not be stripped.
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

    /// A query binding no fields is an empty catalog, not a panic and not
    /// a special case. `field_refs` deliberately ignores bare-word search
    /// and time bounds, so this is an everyday shape.
    #[test]
    fn a_query_binding_no_fields_yields_an_empty_catalog() {
        let pins = derive_field_types(&parse("error last=1h"), b"\x01\x02\x03");
        assert_eq!(pins.iter().count(), 0);
    }

    /// An empty selector pins nothing: byte 0 is the unpinned choice, and
    /// that is what a trailing-NUL input decodes to.
    #[test]
    fn an_empty_selector_leaves_every_field_unpinned() {
        let pins = derive_field_types(&parse("a=1 b=2 c=3"), b"");
        assert_eq!(pins.iter().count(), 0);
    }

    /// A short selector cycles. One byte pins every field the same way,
    /// which is a legitimate map and not an error.
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

    /// Sorted, folded field order is what makes one flipped byte mean one
    /// repinned field. `Zebra` folds to `zebra` and sorts last.
    #[test]
    fn fields_take_selector_bytes_in_sorted_folded_order() {
        let pins = derive_field_types(&parse("Zebra=1 alpha=2 middle=3"), &[1, 2, 3]);
        assert_eq!(pins.get("alpha"), Some(CanonicalType::ALL[0]));
        assert_eq!(pins.get("middle"), Some(CanonicalType::ALL[1]));
        assert_eq!(pins.get("zebra"), Some(CanonicalType::ALL[2]));
    }

    /// Every canonical type is reachable, and byte 0 (plus every multiple
    /// of the modulus) means unpinned. If this drifts, the fuzzer silently
    /// stops exercising a type.
    #[test]
    fn every_canonical_type_is_reachable_from_some_byte() {
        let query = parse("a=1");
        for (index, expected) in CanonicalType::ALL.into_iter().enumerate() {
            let byte = u8::try_from(index + 1).expect("ALL is far shorter than 255");
            let pins = derive_field_types(&query, &[byte]);
            assert_eq!(pins.get("a"), Some(expected), "byte {byte}");
        }
        assert_eq!(derive_field_types(&query, &[0]).get("a"), None);

        // The modulus wraps, so a high byte lands on the same choices as a
        // low one instead of piling onto the last type.
        let modulus = u8::try_from(CanonicalType::ALL.len() + 1)
            .expect("the canonical vocabulary is far shorter than 255");
        assert_eq!(derive_field_types(&query, &[modulus]).get("a"), None);
        assert_eq!(
            derive_field_types(&query, &[modulus + 1]).get("a"),
            Some(CanonicalType::ALL[0])
        );

        // 255 is where byte-flip mutations pile up, so spell out where it
        // lands. Computed rather than written down: the answer moves when
        // the vocabulary grows, and a hand-written index would then be a
        // silent off-by-one or an underflow in this very test.
        let choice = usize::from(255 % modulus);
        let expected = if choice == 0 {
            None
        } else {
            Some(CanonicalType::ALL[choice - 1])
        };
        assert_eq!(derive_field_types(&query, &[255]).get("a"), expected);
    }

    /// The two halves compose: decode, parse, derive. This is exactly the
    /// sequence the fuzz target and the PREPARE fixture both run.
    #[test]
    fn decoding_and_deriving_compose_into_one_case() {
        let decoded = decode_case("status>=400 service=nginx\0\x05\x01");
        let query = parse(decoded.query);
        let pins = derive_field_types(&query, decoded.selector);
        // Sorted: service takes byte 5, status takes byte 1.
        assert_eq!(pins.get("service"), Some(CanonicalType::ALL[4]));
        assert_eq!(pins.get("status"), Some(CanonicalType::ALL[0]));
    }

    /// Reserved and contract-typed names are pinned like any other,
    /// because the target's claim is totality over pin maps rather than
    /// fidelity to what the catalog would admit.
    #[test]
    fn no_admission_policy_is_applied() {
        let pins = derive_field_types(&parse("host=a _severity=error"), &[6]);
        assert_eq!(pins.get("host"), Some(CanonicalType::Severity));
        assert_eq!(pins.get("_severity"), Some(CanonicalType::Severity));
    }
}
