// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `CodeMirror` ↔ Rust offset translation helpers.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// Convert a UTF-16 code-unit offset (as `CodeMirror` reports) into the
/// corresponding UTF-8 byte offset in `s`.
#[must_use]
pub fn utf16_to_utf8(s: &str, utf16_offset: usize) -> usize {
    let mut utf16_seen = 0usize;
    for (byte_idx, ch) in s.char_indices() {
        if utf16_seen >= utf16_offset {
            return byte_idx;
        }
        utf16_seen += ch.len_utf16();
    }
    s.len()
}

/// Convert a UTF-8 byte offset (as parser `ParseError.span` uses) into
/// the corresponding UTF-16 code-unit offset.
#[must_use]
pub fn utf8_to_utf16(s: &str, utf8_offset: usize) -> usize {
    let mut utf16_seen = 0usize;
    for (byte_idx, ch) in s.char_indices() {
        if byte_idx >= utf8_offset {
            return utf16_seen;
        }
        utf16_seen += ch.len_utf16();
    }
    utf16_seen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_identity() {
        let s = "service=nginx | stats count() by host";
        for i in 0..=s.len() {
            assert_eq!(utf16_to_utf8(s, i), i, "utf16→utf8 at {i}");
            assert_eq!(utf8_to_utf16(s, i), i, "utf8→utf16 at {i}");
        }
    }

    #[test]
    fn bmp_multibyte_utf8_single_utf16_unit() {
        let s = "naïve=true";

        assert_eq!(utf16_to_utf8(s, 5), 6);
        assert_eq!(utf8_to_utf16(s, 6), 5);

        assert_eq!(utf16_to_utf8(s, 2), 2);
        assert_eq!(utf8_to_utf16(s, 2), 2);

        assert_eq!(utf16_to_utf8(s, 3), 4);
        assert_eq!(utf8_to_utf16(s, 4), 3);
    }

    #[test]
    fn astral_chars_are_two_utf16_units() {
        let s = "x🌊y";

        assert_eq!(utf16_to_utf8(s, 1), 1);
        assert_eq!(utf8_to_utf16(s, 1), 1);

        assert_eq!(utf16_to_utf8(s, 3), 5);
        assert_eq!(utf8_to_utf16(s, 5), 3);

        assert_eq!(utf16_to_utf8(s, 4), 6);
        assert_eq!(utf8_to_utf16(s, 6), 4);
    }

    #[test]
    fn out_of_bounds_clamps_to_end() {
        let s = "abc";
        assert_eq!(utf16_to_utf8(s, 100), 3);
        assert_eq!(utf8_to_utf16(s, 100), 3);
    }

    #[test]
    fn every_utf16_offset_yields_char_boundary() {
        for s in ["x🌊y", "naïve", "🌊🌊", "", "simple ascii", "a\u{1F600}b"] {
            for utf16 in 0..=s.encode_utf16().count() + 5 {
                let utf8 = utf16_to_utf8(s, utf16);
                assert!(
                    s.is_char_boundary(utf8),
                    "utf16_to_utf8({s:?}, {utf16}) = {utf8} is NOT a char boundary",
                );
                let _ = &s[..utf8];
            }
        }
    }

    #[test]
    fn empty_string_returns_zero() {
        assert_eq!(utf16_to_utf8("", 0), 0);
        assert_eq!(utf16_to_utf8("", 5), 0);
        assert_eq!(utf8_to_utf16("", 0), 0);
        assert_eq!(utf8_to_utf16("", 5), 0);
    }

    #[test]
    fn round_trip_astral_in_middle() {
        let s = "pre 🌊 post";
        for utf16 in [0, 1, 2, 3, 4, 6, 7, 8, 9, 10] {
            let utf8 = utf16_to_utf8(s, utf16);
            let back = utf8_to_utf16(s, utf8);
            assert_eq!(back, utf16, "round-trip failed at utf-16 offset {utf16}");
        }
    }
}
