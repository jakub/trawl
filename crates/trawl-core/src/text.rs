// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The case fold every in-memory reading of the text search's `ILIKE`
//! containment applies: the live filter and `sightings` attribution
//! (ADR-0015).

mod duckdb_lower;

/// The bundled `DuckDB`'s lowercase fold of `s`.
///
/// Each character folds through `DuckDB`'s own `lower()` table, one code
/// point to one and with no context rule, so `ÄB` folds to `äb`, `İ`
/// (U+0130) to `i` and `Σ` to `σ` wherever it stands. `ILIKE` folds both
/// sides with the same table, so the live filter and `sightings`
/// attribution match exactly what the emitted SQL matches. The table is
/// generated from the bundled `DuckDB`; a `DuckDB` bump that changes its
/// mapping fails `tests/fold_table.rs` until the table is regenerated.
#[must_use]
pub fn fold_ci(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    s.chars().map(fold_char).collect()
}

/// `c` under the table, or `c` itself when `DuckDB`'s `lower()` keeps it.
fn fold_char(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    duckdb_lower::LOWER
        .binary_search_by_key(&c, |&(from, _)| from)
        .map_or(c, |i| duckdb_lower::LOWER[i].1)
}

/// Whether `haystack` contains `needle` under the [`fold_ci`] fold of
/// both sides, the text search's `ILIKE` containment.
///
/// `_` and `%` are characters here, never wildcards.
#[must_use]
pub fn contains_ci(haystack: &str, needle: &str) -> bool {
    fold_ci(haystack).contains(&fold_ci(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_sorted_and_never_maps_ascii_elsewhere() {
        assert!(
            duckdb_lower::LOWER
                .windows(2)
                .all(|pair| pair[0].0 < pair[1].0)
        );
        for &(from, to) in duckdb_lower::LOWER {
            if from.is_ascii() {
                assert_eq!(to, from.to_ascii_lowercase());
            }
        }
    }

    #[test]
    fn folds_one_code_point_to_one() {
        assert_eq!(fold_ci("ÄB"), "äb");
        assert_eq!(fold_ci("İ"), "i");
        assert_eq!(fold_ci("ΣΑΣ"), "σασ");
        assert_eq!(fold_ci("ẞ"), "ß");
        assert_eq!(fold_ci("\u{A7CB}"), "\u{A7CB}");
        assert!(contains_ci("xxİMxx", "im"));
        assert!(!contains_ci("İ", "i\u{307}"));
    }
}
