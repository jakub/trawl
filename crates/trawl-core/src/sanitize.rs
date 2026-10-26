// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Client-chosen text made safe to render (ADR-0011).
//!
//! Several renderings carry text a sender picked. The conflict samples
//! compaction captures are the obvious one — arbitrary event values,
//! sanitised once at capture so every consumer downstream is safe by
//! construction. Another is the catalog-sourced field name the CLI's case
//! file prints (`schema field`'s `resp.name`, which is a client-chosen JSON
//! key that ingest polices for length and case only). They reach a terminal,
//! a JSON body and a browser alike.
//!
//! Field names taken from a query need none of this: the bare production is
//! `[A-Za-z_][A-Za-z0-9_]*` with `.` and `@`, and the backtick-quoted one
//! (ADR-0013 ruling 7) admits any character except the ones below — its
//! parser refuses a name carrying an [`is_unsafe_display_char`], so the
//! query notice's footer and the names in a projection-collision message
//! echo what the user typed through a grammar that admits no control or
//! format character. The policing stops at names: a query string literal
//! (`none_of('"')`) admits everything below, so a message that renders an
//! expression rather than a name — `projection::render_agg`, which prints
//! an aggregate's arguments — sanitises its rendering here like any other
//! client-chosen text.
//!
//! [`char::is_control`] is not enough for that. It covers category Cc
//! alone, while the characters that actually rewrite a rendering are format
//! characters (Cf), one Zs-adjacent oddity, and the two line breaks that
//! sit outside Cc:
//!
//! - **bidi controls** — U+061C, U+200E/200F, U+202A–202E, U+2066–2069.
//!   These reorder the text AROUND them, which is the Trojan-Source shape:
//!   a sample value can make the line that follows it read as something
//!   else entirely, remedy commands included.
//! - **zero-widths and joiners** — U+200B–200D, U+2060, U+FEFF. Invisible,
//!   so two different values render identically, and a name can carry
//!   payload no operator can see.
//! - **the soft hyphen** — U+00AD, invisible until a renderer decides to
//!   break the line there.
//! - **the line and paragraph separators** — U+2028, U+2029 (Zl, Zp). A
//!   reader that splits lines the Unicode way, as a log viewer or Python's
//!   `str.splitlines` does, starts a new line at each, so one value can
//!   forge the line after it.
//!
//! Rust's standard library has no `is_format`, so the set is written out
//! and pinned by tests. Substitution, never deletion: U+FFFD leaves visible
//! evidence that something was there, where dropping the character would
//! silently produce a DIFFERENT string that looks legitimate.
//!
//! A refusal that names what a caller sent quotes it through [`excerpt`]:
//! escaped as `{:?}` escapes it, and at most [`ERROR_EXCERPT_BYTES`] of
//! it, so the answer stays bounded whatever was sent.

/// The replacement every unsafe character collapses to.
pub const REPLACEMENT: char = '\u{fffd}';

/// Whether `c` may not be rendered as-is: a control character (Cc), a bidi
/// or zero-width format character (Cf), the soft hyphen, or the line or
/// paragraph separator.
///
/// See the module documentation for what each group does to a rendering.
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

/// `text` with every [`is_unsafe_display_char`] replaced by [`REPLACEMENT`].
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

/// The bytes of caller text an error message quotes ([`excerpt`]).
pub const ERROR_EXCERPT_BYTES: usize = 64;

/// `text` as an error message quotes it: `{:?}`-escaped, and when longer
/// than `max_bytes`, only its longest prefix of at most `max_bytes` that
/// ends on a character boundary, then `…` and the whole length, as in
/// `"aaa"… (102400 bytes)`. A refusal that echoes what a caller sent stays
/// bounded whatever was sent.
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

    /// Text at most the limit is quoted whole; longer text is cut at the
    /// limit, never inside a character, and states its whole length.
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
        // Escaped like any `{:?}` echo: a quote and a control character.
        assert_eq!(excerpt("a\"\u{1}", 64), "\"a\\\"\\u{1}\"");
    }

    /// The whole named set, one case each — a character silently escaping
    /// this list is a rendering an operator cannot trust.
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

    /// Ordinary text — including non-ASCII a client legitimately sends —
    /// passes through untouched. A sanitizer that mangles real values makes
    /// the evidence useless.
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
