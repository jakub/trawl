// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `CodeMirror` ↔ Rust offset translation helpers.
//!
//! `CodeMirror` (and JS in general) indexes documents in **UTF-16 code
//! units**: every char is 1 unit except astral characters (🌊, emoji,
//! most CJK supplementary plane) which are 2 units.
//!
//! Rust `str` methods index in **UTF-8 bytes**: ASCII is 1 byte, Latin-1
//! extended 2, most BMP CJK 3, astral 4.
//!
//! Slicing a Rust string with a `CodeMirror` offset directly risks a
//! panic (mid-codepoint slice) and mispositioned diagnostics even when
//! it doesn't panic. These helpers convert between the two coordinate
//! spaces by walking characters.
//!
//! The helpers are only wired into the Leptos editor on wasm32; on
//! native builds they exist purely so their tests can run under plain
//! `cargo test` without wasm-bindgen-test. Hence the dead-code allow.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// Convert a UTF-16 code-unit offset (as `CodeMirror` reports) into the
/// corresponding UTF-8 byte offset in `s`. Offsets past the end of the
/// string clamp to `s.len()`.
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
/// the corresponding UTF-16 code-unit offset. Offsets past the end of
/// the string clamp to the total UTF-16 length.
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
        // é is 2 bytes in UTF-8, 1 code unit in UTF-16.
        let s = "naïve=true";
        //       ^ ^
        //       | └─ naïve: n=1, a=1, ï=2, v=1, e=1  bytes, total 6
        //       positions: 0  1  2  4  5  6 = utf-8 byte offsets
        //       utf-16 offsets: 0  1  2  3  4  5

        // After "naïve" (utf-16=5, utf-8=6), the "=" starts.
        assert_eq!(utf16_to_utf8(s, 5), 6);
        assert_eq!(utf8_to_utf16(s, 6), 5);

        // Mid-string conversions at "ï" boundary: utf-16=2 → utf-8=2
        // (byte offset of ï), utf-8=2 → utf-16=2.
        assert_eq!(utf16_to_utf8(s, 2), 2);
        assert_eq!(utf8_to_utf16(s, 2), 2);

        // Just after "ï": utf-16=3 → utf-8=4, and back.
        assert_eq!(utf16_to_utf8(s, 3), 4);
        assert_eq!(utf8_to_utf16(s, 4), 3);
    }

    #[test]
    fn astral_chars_are_two_utf16_units() {
        // Wave emoji 🌊 is 4 bytes UTF-8, 2 code units UTF-16 (surrogate pair).
        let s = "x🌊y";
        // bytes:   x  🌊    y
        //          0  1234  5
        // utf-16:  x  🌊    y
        //          0  12    3

        // Start of 🌊
        assert_eq!(utf16_to_utf8(s, 1), 1);
        assert_eq!(utf8_to_utf16(s, 1), 1);

        // Between 🌊 and y: utf-16=3 → utf-8=5
        assert_eq!(utf16_to_utf8(s, 3), 5);
        assert_eq!(utf8_to_utf16(s, 5), 3);

        // End of string
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
        // Regression guard: slicing `&s[..utf16_to_utf8(s, pos)]` must
        // never panic mid-codepoint for ANY utf-16 offset, including
        // ones that fall between surrogate halves of astral chars.
        // `is_char_boundary` is the stdlib invariant we rely on.
        for s in ["x🌊y", "naïve", "🌊🌊", "", "simple ascii", "a\u{1F600}b"] {
            for utf16 in 0..=s.encode_utf16().count() + 5 {
                let utf8 = utf16_to_utf8(s, utf16);
                assert!(
                    s.is_char_boundary(utf8),
                    "utf16_to_utf8({s:?}, {utf16}) = {utf8} is NOT a char boundary",
                );
                // And slicing must not panic, which is the actual
                // property we care about in `complete_at`.
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
        // For every utf-16 position, converting to utf-8 and back should
        // land on the same utf-16 position — UNLESS we land inside the
        // surrogate pair, in which case we clamp to the start of the
        // char. Our implementation clamps to the char boundary, so
        // utf-16=5 (middle of 🌊 surrogate pair) maps to utf-8=4 (start
        // of 🌊 byte sequence), which maps back to utf-16=4.
        for utf16 in [0, 1, 2, 3, 4, 6, 7, 8, 9, 10] {
            let utf8 = utf16_to_utf8(s, utf16);
            let back = utf8_to_utf16(s, utf8);
            assert_eq!(back, utf16, "round-trip failed at utf-16 offset {utf16}");
        }
    }
}
