// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The case fold every in-memory reading of the text search's `ILIKE`
//! containment applies: the live filter and `sightings` attribution
//! (ADR-0015).

/// The Unicode lowercase fold of `s`.
///
/// Each character is folded through [`char::to_lowercase`], the Unicode
/// default lowercase mapping of that one code point, so `ÄB` folds to
/// `äb`. `DuckDB`'s `ILIKE` folds with a one-to-one mapping per code
/// point; Rust's `to_lowercase` is the full mapping. The residual
/// difference is the special casing that maps one code point to several
/// or depends on context: Rust folds `İ` (U+0130) to `i̇` (two code
/// points) where a one-to-one fold gives `i`, and neither side applies
/// the final-sigma rule. Every other character folds the same way.
#[must_use]
pub fn fold_ci(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// Whether `haystack` contains `needle` under the [`fold_ci`] fold of
/// both sides, standing for the text search's `ILIKE` containment.
///
/// `_` and `%` are characters here, never wildcards. A literal or text
/// carrying one of the residual characters [`fold_ci`] names can match
/// differently from how `DuckDB` matches it.
#[must_use]
pub fn contains_ci(haystack: &str, needle: &str) -> bool {
    fold_ci(haystack).contains(&fold_ci(needle))
}
