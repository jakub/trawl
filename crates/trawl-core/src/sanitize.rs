// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Client-chosen text made safe to render (ADR-0011 slice C1).
//!
//! Two renderings carry text a sender picked. The conflict SAMPLES
//! compaction captures are the obvious one — arbitrary event values,
//! sanitised once at capture so every consumer downstream is safe by
//! construction. The other is the CATALOG-SOURCED field name the CLI's case
//! file prints (`schema field`'s `resp.name`, which is a client-chosen JSON
//! key that ingest polices for length and case only). Both reach a terminal,
//! a JSON body and (slice C2) a browser.
//!
//! Field names taken from a QUERY need none of this, and that stays true
//! now that backticks let a query name ANY column (ADR-0013 ruling 7):
//! `parser::primitives::backtick_name` refuses every
//! [`is_unsafe_display_char`] outright, so the query notice's footer still
//! echoes what the user typed through a grammar that admits no control or
//! format character. The guarantee moved from "the shape is narrow" to
//! "this predicate is the door", and
//! `the_grammar_admits_no_unrenderable_name` pins it.
//!
//! [`char::is_control`] is not enough for that. It covers category Cc
//! alone, while the characters that actually rewrite a rendering are format
//! characters (Cf) and one Zs-adjacent oddity:
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
//!
//! Rust's standard library has no `is_format`, so the set is written out
//! and pinned by tests. Substitution, never deletion: U+FFFD leaves visible
//! evidence that something was there, where dropping the character would
//! silently produce a DIFFERENT string that looks legitimate.

/// The replacement every unsafe character collapses to.
pub const REPLACEMENT: char = '\u{fffd}';

/// Whether `c` may not be rendered as-is: a control character (Cc), a bidi
/// or zero-width format character (Cf), or the soft hyphen.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole named set, one case each — a character silently escaping
    /// this list is a rendering an operator cannot trust.
    #[test]
    fn every_named_group_is_replaced() {
        let cases: [(&str, &str, &str); 10] = [
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
