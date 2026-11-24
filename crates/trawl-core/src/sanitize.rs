// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Client-chosen text made safe to render (ADR-0011).
//!
//! [`char::is_control`] is not enough: it covers category Cc alone, while
//! the characters that rewrite a rendering are bidi controls, zero-widths
//! and joiners, the soft hyphen, and the line and paragraph separators.
//! Rust's standard library has no `is_format`, so the set is written out
//! and pinned by tests. Substitution, never deletion: U+FFFD leaves visible
//! evidence that something was there.

pub const REPLACEMENT: char = '\u{fffd}';

#[must_use]
pub fn is_unsafe_display_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            // bidi controls
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            // zero-widths, joiners, BOM-as-ZWNBSP
            | '\u{200b}'..='\u{200d}' | '\u{2060}' | '\u{feff}'
            // soft hyphen
            | '\u{00ad}'
            // line and paragraph separators
            | '\u{2028}' | '\u{2029}'
        )
}

#[must_use]
pub fn sanitize_display_text(text: &str) -> String {
    text.chars()
        .map(|c| {
            if is_unsafe_display_char(c) {
                REPLACEMENT
            } else {
                c
            }
        })
        .collect()
}

pub const ERROR_EXCERPT_BYTES: usize = 64;

#[must_use]
pub fn excerpt(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return format!("{text:?}");
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{:?}… ({} bytes)", &text[..end], text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_excerpt_quotes_a_bounded_prefix_and_counts_the_rest() {
        assert_eq!(excerpt("host", 64), "\"host\"");
        let exact = "a".repeat(64);
        assert_eq!(excerpt(&exact, 64), format!("{exact:?}"));
        let long = "a".repeat(100 * 1024);
        assert_eq!(
            excerpt(&long, 64),
            format!("{:?}… (102400 bytes)", "a".repeat(64))
        );
        // `é` is two bytes: a cut at byte 3 of `aéé` keeps `aé`.
        assert_eq!(excerpt("aéé", 3), "\"aé\"… (5 bytes)");
        assert_eq!(excerpt("aéé", 2), "\"a\"… (5 bytes)");
        assert_eq!(excerpt("a\"\u{1}", 64), "\"a\\\"\\u{1}\"");
    }

    #[test]
    fn every_named_group_is_replaced() {
        let cases: [(&str, &str, &str); 12] = [
            ("a\u{1}b", "a\u{fffd}b", "C0 control"),
            ("a\u{7f}b", "a\u{fffd}b", "DEL"),
            ("a\u{9c}b", "a\u{fffd}b", "C1 control"),
            ("a\u{202e}b", "a\u{fffd}b", "RLO — the Trojan-Source lever"),
            ("a\u{2066}b", "a\u{fffd}b", "first strong isolate"),
            ("a\u{2069}b", "a\u{fffd}b", "pop directional isolate"),
            ("a\u{061c}b", "a\u{fffd}b", "arabic letter mark"),
            ("a\u{200b}b", "a\u{fffd}b", "zero-width space"),
            ("a\u{feff}b", "a\u{fffd}b", "zero-width no-break space"),
            ("a\u{00ad}b", "a\u{fffd}b", "soft hyphen"),
            ("a\u{2028}b", "a\u{fffd}b", "line separator"),
            ("a\u{2029}b", "a\u{fffd}b", "paragraph separator"),
        ];
        for (input, expect, why) in cases {
            assert_eq!(sanitize_display_text(input), expect, "{why}");
        }
    }

    #[test]
    fn ordinary_text_is_untouched() {
        for text in [
            "n/a",
            "pending",
            "  spaced  ",
            "日本語",
            "café",
            "🙂",
            "a\u{200a}b",
        ] {
            assert_eq!(sanitize_display_text(text), text, "{text:?}");
        }
    }
}
