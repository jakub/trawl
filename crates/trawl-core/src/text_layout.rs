// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Terminal text cut, padded and caret-marked in display cells.
//!
//! The unit is the display cell: the columns a terminal gives a string.
//! `日` takes two, a combining accent none, `❤` one and `❤\u{FE0F}` two.
//! Every width here is [`UnicodeWidthStr::width`] of a whole string, plus
//! one cell for each halfwidth katakana sound mark, never a sum of
//! per-character widths. A string's width is not the sum of its parts,
//! and it does not always grow as characters are added: `⌚` takes two
//! cells and `⌚\u{FE0E}` one.
//!
//! Text is passed through [`sanitize_display_text`] before it is measured
//! or returned, so a control, bidi or zero-width character prints as
//! U+FFFD and cannot rewrite the line around it.
//!
//! [`fit_cells`] returns the sanitized text whole when it fits its budget.
//! Otherwise it returns a prefix of the sanitized text, cut on a character
//! boundary, followed by one [`MARKER`], all within the budget. The prefix
//! fits, but it is not always the longest one that would: when a character
//! makes the text narrower, a longer prefix can fit after a shorter one did
//! not. For text that only grows as it lengthens, which includes Latin, CJK
//! and combining marks, the cut is the longest prefix that fits.
//!
//! The work [`fit_cells`] does is bounded by the budget, not by the input.
//! It reads at most `8 × (budget + 1)` characters of the input. It then
//! measures about log₂ of that many candidates, none longer than what it
//! read. A value of any length, a million combining marks included, costs
//! O(budget × log budget). Text longer than that read window is cut even
//! when its width would fit: ordinary text needs far fewer than eight
//! characters per cell.
//!
//! [`locate_span`] resolves a span, a pair of UTF-8 byte offsets into the
//! text it indexes, to the line holding its start. It refuses a span that
//! cannot index the text and never clamps one. [`caret_layout`] lays that
//! line out for a caret line underneath it.

use unicode_width::UnicodeWidthStr;

use crate::sanitize::sanitize_display_text;

/// What [`fit_cells`] puts where it cut text. It takes one cell.
pub const MARKER: char = '…';

/// How many input characters [`fit_cells`] reads per cell of budget.
///
/// A base letter with its accents, or a flag with its tag characters,
/// stays under this. A longer run is cut whatever its width.
const CHARS_PER_CELL: usize = 8;

/// How many input characters [`fit_cells`] reads at `budget`.
fn scan_cap(budget: usize) -> usize {
    budget.saturating_add(1).saturating_mul(CHARS_PER_CELL)
}

/// The first [`scan_cap`] characters of `text`, and whether that left any
/// of it unread. Walks at most that many characters.
fn scan_window(text: &str, budget: usize) -> (&str, bool) {
    match text.char_indices().nth(scan_cap(budget)) {
        Some((at, _)) => (&text[..at], true),
        None => (text, false),
    }
}

/// The display cells `s` takes: [`UnicodeWidthStr::width`] of the whole
/// string, plus one for each halfwidth katakana sound mark, U+FF9E `ﾞ` or
/// U+FF9F `ﾟ`.
///
/// unicode-width gives those two marks no width, as it does a combining
/// mark, but a terminal draws each in a cell of its own. ratatui-core
/// counts them the same way (`count_halfwidth_sound_marks` in its
/// `buffer/cell_width.rs`), so a width measured here is where ratatui puts
/// the next cell. The combining sound marks U+3099 and U+309A keep their
/// unicode-width of none.
///
/// Every width in this module goes through here.
fn measure(s: &str) -> usize {
    let sound_marks = s
        .chars()
        .filter(|c| matches!(c, '\u{FF9E}' | '\u{FF9F}'))
        .count();
    s.width() + sound_marks
}

/// The display cells `text` takes once sanitized, measured as one string.
///
/// This reads all of `text`. To size a column from values of any length,
/// measure each value's [`fit_cells`] at the widest the column may be,
/// which reads only a bounded prefix.
#[must_use]
pub fn cells(text: &str) -> usize {
    measure(&sanitize_display_text(text))
}

/// `text` sanitized and cut to at most `budget` display cells.
///
/// The sanitized text whole when it fits; otherwise a prefix of it that
/// ends on a character boundary, then [`MARKER`]. A budget of 0 returns an
/// empty string. See the module documentation for which prefix is kept,
/// and for the bound on the work done.
#[must_use]
pub fn fit_cells(text: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    // `sanitize_display_text` is a per-character `map`, one character out
    // for each character in, so the sanitized window is a prefix of the
    // sanitized text and a cut of it is a cut of the whole.
    let (window, cut) = scan_window(text, budget);
    let shown = sanitize_display_text(window);
    if !cut && measure(&shown) <= budget {
        return shown;
    }

    // The prefix ends a candidate may keep. When the window held the whole
    // text, the whole did not fit, so only a shorter prefix may be kept.
    let ends: Vec<usize> = shown
        .char_indices()
        .map(|(at, _)| at)
        .chain(cut.then_some(shown.len()))
        .collect();
    let candidate = |n: usize| {
        let mut out = String::with_capacity(ends[n] + MARKER.len_utf8());
        out.push_str(&shown[..ends[n]]);
        out.push(MARKER);
        out
    };

    // Binary search for the last candidate that fits, measuring each one
    // whole. `best` is always a candidate that was measured and fit, so the
    // result fits even where width shrinks as the text lengthens and the
    // search settles on a shorter prefix than the longest that fits. The
    // first candidate is the marker alone, one cell; it is measured like
    // the rest so that nothing is returned unmeasured.
    let mut best = candidate(0);
    if measure(&best) > budget {
        return String::new();
    }
    let (mut fits, mut too_wide) = (0, ends.len());
    while too_wide - fits > 1 {
        let mid = fits + (too_wide - fits) / 2;
        let tried = candidate(mid);
        if measure(&tried) <= budget {
            fits = mid;
            best = tried;
        } else {
            too_wide = mid;
        }
    }
    best
}

/// [`fit_cells`] of `text`, then spaces to exactly `width` display cells.
#[must_use]
pub fn pad_cells(text: &str, width: usize) -> String {
    let mut out = fit_cells(text, width);
    let used = measure(&out);
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    out
}

/// A span resolved to the line holding its start.
///
/// Every offset is a byte offset into the text [`locate_span`] read, and
/// sits on a character boundary of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanLine {
    /// 1-based number of the line.
    pub line: usize,
    /// Whether the text holds a `\n`. A client that names the line it
    /// quotes does so only then.
    pub multi_line: bool,
    /// Where the line starts.
    pub line_start: usize,
    /// Where the line's text ends: before its `\n` or `\r\n`, and before
    /// a lone `\r` that ends the text.
    pub line_end: usize,
    /// The span's start, moved back to `line_end` when it sat on the line
    /// ending.
    pub start: usize,
    /// The span's end, cut to `line_end` and never before `start`.
    pub end: usize,
}

impl SpanLine {
    /// The line, without its line ending.
    ///
    /// `text` must be the text [`locate_span`] read; any other text can
    /// panic here and in the other accessors.
    #[must_use]
    pub fn line_text<'t>(&self, text: &'t str) -> &'t str {
        &text[self.line_start..self.line_end]
    }

    /// The line before the span.
    #[must_use]
    pub fn before<'t>(&self, text: &'t str) -> &'t str {
        &text[self.line_start..self.start]
    }

    /// The part of the line the span covers.
    #[must_use]
    pub fn covered<'t>(&self, text: &'t str) -> &'t str {
        &text[self.start..self.end]
    }

    /// The line after the span.
    #[must_use]
    pub fn after<'t>(&self, text: &'t str) -> &'t str {
        &text[self.end..self.line_end]
    }
}

/// Resolve the byte span `start..end` against `text`.
///
/// `None` when the span cannot index `text`: reversed, past the end, or
/// off a character boundary at either end. The caller then shows the
/// message without an excerpt; a caret under the wrong character is worse
/// than none.
///
/// For a valid span this is the line holding `start`. `\r\n` is one line
/// ending, and a lone `\r` at the end of the text is dropped like one. A
/// span that runs past its line is cut there, and a span that starts on
/// the line ending sits just past the line's text.
#[must_use]
pub fn locate_span(text: &str, start: usize, end: usize) -> Option<SpanLine> {
    // Every check comes before any slice or arithmetic, so no offset, not
    // even `usize::MAX`, can panic below.
    if start > end
        || end > text.len()
        || !text.is_char_boundary(start)
        || !text.is_char_boundary(end)
    {
        return None;
    }
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let next_newline = text[start..].find('\n').map_or(text.len(), |i| start + i);
    let with_ending = &text[line_start..next_newline];
    let line_end = line_start + with_ending.strip_suffix('\r').unwrap_or(with_ending).len();
    let start = start.min(line_end);
    Some(SpanLine {
        line: text[..line_start].matches('\n').count() + 1,
        multi_line: text.contains('\n'),
        line_start,
        line_end,
        start,
        end: end.min(line_end).max(start),
    })
}

/// A located line laid out for a terminal: its three parts, sanitized,
/// and where a caret line marks the span under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaretLayout {
    /// The line before the span.
    pub before: String,
    /// The part of the line the span covers.
    pub covered: String,
    /// The line after the span.
    pub after: String,
    /// The display cells before the first caret: the width of `before`.
    pub pad: usize,
    /// How many carets mark the span: the width of `covered`, at least
    /// one, so an empty span still gets a caret where it sits.
    pub carets: usize,
}

/// Lay out the line `at` names in `text` for a caret line underneath.
///
/// Each part is sanitized, so a tab or any other character
/// [`is_unsafe_display_char`](crate::sanitize::is_unsafe_display_char)
/// names prints as one U+FFFD cell, in the line and in the count alike.
#[must_use]
pub fn caret_layout(text: &str, at: &SpanLine) -> CaretLayout {
    let before = sanitize_display_text(at.before(text));
    let covered = sanitize_display_text(at.covered(text));
    let after = sanitize_display_text(at.after(text));
    CaretLayout {
        pad: measure(&before),
        carets: measure(&covered).max(1),
        before,
        covered,
        after,
    }
}

/// One row of the span table every client's caret tests read.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone, Copy)]
pub struct SpanCase {
    /// What the row checks.
    pub name: &'static str,
    /// The text the span indexes.
    pub text: &'static str,
    /// The span's start, a byte offset.
    pub start: usize,
    /// The span's end, a byte offset.
    pub end: usize,
    /// `None` when the span cannot index the text: the client shows the
    /// message and hint without an excerpt.
    pub expect: Option<SpanExpect>,
}

/// What a terminal client shows for a [`SpanCase`] that locates.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanExpect {
    /// 1-based number of the line shown.
    pub line: usize,
    /// The line as printed: sanitized, without its line ending.
    pub excerpt: &'static str,
    /// Display cells before the first caret.
    pub pad: usize,
    /// How many carets mark the span.
    pub carets: usize,
}

/// The span table. Every expectation is a literal worked out by hand.
#[cfg(any(test, feature = "test-support"))]
pub const SPAN_CASES: &[SpanCase] = &[
    SpanCase {
        name: "ascii",
        text: "service=web | stats count( by host",
        start: 30,
        end: 34,
        expect: Some(SpanExpect {
            line: 1,
            excerpt: "service=web | stats count( by host",
            pad: 30,
            carets: 4,
        }),
    },
    SpanCase {
        // 30 bytes, 26 characters and 28 cells come before the `(`.
        name: "cjk before the span",
        text: "service=\"日本\" | where x ~ /(/",
        start: 30,
        end: 31,
        expect: Some(SpanExpect {
            line: 1,
            excerpt: "service=\"日本\" | where x ~ /(/",
            pad: 28,
            carets: 1,
        }),
    },
    SpanCase {
        name: "cjk covered",
        text: "service=\"日本\" | where x ~ /(/",
        start: 9,
        end: 15,
        expect: Some(SpanExpect {
            line: 1,
            excerpt: "service=\"日本\" | where x ~ /(/",
            pad: 9,
            carets: 4,
        }),
    },
    SpanCase {
        // `ｶ` and its sound mark `ﾞ` are a cell each, as a terminal and
        // ratatui draw them, though unicode-width gives the mark none. 30
        // bytes, 26 characters and 26 cells come before the `(`.
        name: "halfwidth sound mark before the span",
        text: "service=\"ｶﾞ\" | where x ~ /(/",
        start: 30,
        end: 31,
        expect: Some(SpanExpect {
            line: 1,
            excerpt: "service=\"ｶﾞ\" | where x ~ /(/",
            pad: 26,
            carets: 1,
        }),
    },
    SpanCase {
        name: "accented",
        text: "user=résumé_ñ",
        start: 5,
        end: 16,
        expect: Some(SpanExpect {
            line: 1,
            excerpt: "user=résumé_ñ",
            pad: 5,
            carets: 8,
        }),
    },
    SpanCase {
        // 7 bytes, 6 characters and 5 cells come before the `x`.
        name: "combining mark",
        text: "cafe\u{301} x",
        start: 7,
        end: 8,
        expect: Some(SpanExpect {
            line: 1,
            excerpt: "cafe\u{301} x",
            pad: 5,
            carets: 1,
        }),
    },
    SpanCase {
        // The heart and its selector are two cells together, though the
        // per-character widths add up to one.
        name: "emoji presentation selector",
        text: "❤\u{FE0F} x",
        start: 7,
        end: 8,
        expect: Some(SpanExpect {
            line: 1,
            excerpt: "❤\u{FE0F} x",
            pad: 3,
            carets: 1,
        }),
    },
    SpanCase {
        name: "bidi control prints as the replacement",
        text: "a\u{202E}b x",
        start: 6,
        end: 7,
        expect: Some(SpanExpect {
            line: 1,
            excerpt: "a\u{FFFD}b x",
            pad: 4,
            carets: 1,
        }),
    },
    SpanCase {
        name: "covered bidi control",
        text: "a\u{202E}b",
        start: 1,
        end: 4,
        expect: Some(SpanExpect {
            line: 1,
            excerpt: "a\u{FFFD}b",
            pad: 1,
            carets: 1,
        }),
    },
    SpanCase {
        name: "tab prints as the replacement",
        text: "a\tb",
        start: 2,
        end: 3,
        expect: Some(SpanExpect {
            line: 1,
            excerpt: "a\u{FFFD}b",
            pad: 2,
            carets: 1,
        }),
    },
    SpanCase {
        name: "empty text",
        text: "",
        start: 0,
        end: 0,
        expect: Some(SpanExpect {
            line: 1,
            excerpt: "",
            pad: 0,
            carets: 1,
        }),
    },
    SpanCase {
        name: "zero-width span",
        text: "abc def",
        start: 4,
        end: 4,
        expect: Some(SpanExpect {
            line: 1,
            excerpt: "abc def",
            pad: 4,
            carets: 1,
        }),
    },
    SpanCase {
        name: "end of input",
        text: "stats count(",
        start: 12,
        end: 12,
        expect: Some(SpanExpect {
            line: 1,
            excerpt: "stats count(",
            pad: 12,
            carets: 1,
        }),
    },
    SpanCase {
        name: "reversed",
        text: "abc",
        start: 2,
        end: 1,
        expect: None,
    },
    SpanCase {
        name: "past the end",
        text: "abc",
        start: 0,
        end: 4,
        expect: None,
    },
    SpanCase {
        name: "starts past the end",
        text: "abc",
        start: 4,
        end: 4,
        expect: None,
    },
    SpanCase {
        name: "largest offsets",
        text: "abc",
        start: usize::MAX,
        end: usize::MAX,
        expect: None,
    },
    SpanCase {
        name: "largest end",
        text: "abc",
        start: 0,
        end: usize::MAX,
        expect: None,
    },
    SpanCase {
        name: "reversed from the largest start",
        text: "abc",
        start: usize::MAX,
        end: 0,
        expect: None,
    },
    SpanCase {
        // `é` is bytes 2..4.
        name: "start inside a character",
        text: "a=é b",
        start: 3,
        end: 5,
        expect: None,
    },
    SpanCase {
        // `日` is bytes 9..12.
        name: "end inside a character",
        text: "service=\"日本\" | where x ~ /(/",
        start: 9,
        end: 10,
        expect: None,
    },
    SpanCase {
        name: "crlf query",
        text: "service=web\r\n| stats count( by host\r\n| head 5",
        start: 31,
        end: 35,
        expect: Some(SpanExpect {
            line: 2,
            excerpt: "| stats count( by host",
            pad: 18,
            carets: 4,
        }),
    },
    SpanCase {
        name: "crlf span into the line ending",
        text: "service=web\r\nx",
        start: 8,
        end: 13,
        expect: Some(SpanExpect {
            line: 1,
            excerpt: "service=web",
            pad: 8,
            carets: 3,
        }),
    },
    SpanCase {
        name: "span on the newline of a crlf",
        text: "x\r\nnext",
        start: 2,
        end: 3,
        expect: Some(SpanExpect {
            line: 1,
            excerpt: "x",
            pad: 1,
            carets: 1,
        }),
    },
    SpanCase {
        name: "multi-line span clips to its start line",
        text: "service=web\n| stats count(\nby host",
        start: 20,
        end: 29,
        expect: Some(SpanExpect {
            line: 2,
            excerpt: "| stats count(",
            pad: 8,
            carets: 6,
        }),
    },
    SpanCase {
        name: "span on a lone trailing carriage return",
        text: "x\r",
        start: 1,
        end: 2,
        expect: Some(SpanExpect {
            line: 1,
            excerpt: "x",
            pad: 1,
            carets: 1,
        }),
    },
    SpanCase {
        name: "end of input after a lone trailing carriage return",
        text: "x\r",
        start: 2,
        end: 2,
        expect: Some(SpanExpect {
            line: 1,
            excerpt: "x",
            pad: 1,
            carets: 1,
        }),
    },
];

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use proptest::test_runner::TestCaseError;

    use super::*;

    /// One piece of a generated string, biased toward the text that has
    /// broken terminal layouts: accents, CJK, halfwidth katakana and its
    /// sound marks, combining marks, emoji with their selectors and
    /// modifiers, ligatures, controls, bidi and zero-width characters, long
    /// runs of zero-width marks, and anything at all.
    fn piece() -> impl Strategy<Value = String> {
        let pick = |options: &'static [&'static str]| {
            prop::sample::select(options).prop_map(str::to_owned)
        };
        prop_oneof![
            3 => proptest::char::range(' ', '~').prop_map(String::from),
            2 => pick(&["é", "ñ", "ü", "Å", "ß"]),
            2 => pick(&["日", "本", "雪", "語", "Ｈ", "한", "ｶ", "\u{FF9E}", "\u{FF9F}"]),
            2 => pick(&["\u{301}", "\u{308}", "\u{323}", "\u{302}", "\u{17D2}"]),
            2 => (
                pick(&["❤", "⌚", "🙂", "🌊", "👍", "‘", "#"]),
                pick(&["", "\u{FE0E}", "\u{FE0F}", "\u{1F3FD}", "\u{FE01}", "\u{20E3}"]),
            )
                .prop_map(|(base, selector)| base + &selector),
            1 => pick(&["\t", "\n", "\r", "\r\n", "\u{1}", "\u{7f}", "\u{9c}"]),
            1 => pick(&["\u{202E}", "\u{2066}", "\u{200B}", "\u{200D}", "\u{FEFF}", "\u{AD}", "\u{2028}"]),
            1 => pick(&["ل", "ا", "ក", "ⵏ", "\u{2D7F}", "…"]),
            1 => (1usize..40).prop_map(|n| "\u{301}".repeat(n)),
            1 => any::<char>().prop_map(String::from),
        ]
    }

    fn generated_text() -> impl Strategy<Value = String> {
        prop::collection::vec(piece(), 0..48).prop_map(|pieces| pieces.concat())
    }

    /// The `fit_cells` and `pad_cells` contract for one text at one budget.
    fn check_fit(text: &str, budget: usize) -> Result<(), TestCaseError> {
        let out = fit_cells(text, budget);
        let shown = sanitize_display_text(text);
        prop_assert!(cells(&out) <= budget, "{out:?} at {budget}");
        prop_assert!(
            UnicodeWidthStr::width(out.as_str()) <= budget,
            "{out:?} at {budget}"
        );
        if budget == 0 {
            prop_assert_eq!(&out, "");
        } else if out != shown {
            // Exactly one marker appended to a shorter prefix.
            let kept = out.strip_suffix(MARKER);
            prop_assert!(
                kept.is_some_and(|kept| kept.len() < shown.len() && shown.starts_with(kept)),
                "{out:?} is not a cut of {shown:?} at {budget}"
            );
        }
        // Read whole and fitting: returned whole.
        if budget > 0 && text.chars().count() <= scan_cap(budget) && measure(&shown) <= budget {
            prop_assert_eq!(&out, &shown);
        }
        let padded = pad_cells(text, budget);
        prop_assert_eq!(measure(&padded), budget);
        prop_assert!(
            padded
                .strip_prefix(out.as_str())
                .is_some_and(|rest| rest.chars().all(|c| c == ' ')),
            "{padded:?} is not {out:?} padded"
        );
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// No panic, never past the budget, and only the sanitized text
        /// whole or a prefix of it with one marker, at every budget 0..=80.
        #[test]
        fn text_layout_fits_cell_budget(text in generated_text()) {
            for budget in 0..=80 {
                check_fit(&text, budget)?;
            }
        }

        /// The same over strings drawn from all of Unicode, unbiased.
        #[test]
        fn text_layout_fits_cell_budget_any_string(text in any::<String>()) {
            for budget in 0..=80 {
                check_fit(&text, budget)?;
            }
        }
    }

    /// Each named class at budgets 0 and 1, and the longest prefix that
    /// fits wherever width only grows as the text lengthens.
    #[test]
    fn text_layout_fits_cell_budget_fixed_cases() {
        let cases: &[(&str, &str, usize, &str)] = &[
            ("empty", "", 0, ""),
            ("empty", "", 1, ""),
            ("ascii", "abc", 0, ""),
            ("ascii", "abc", 1, "…"),
            ("ascii", "abc", 2, "a…"),
            ("ascii", "abc", 3, "abc"),
            ("accented", "résumé_ñ", 0, ""),
            ("accented", "résumé_ñ", 1, "…"),
            ("accented", "résumé_ñ", 5, "résu…"),
            ("accented", "résumé_ñ", 8, "résumé_ñ"),
            ("cjk", "日本語", 0, ""),
            ("cjk", "日本語", 1, "…"),
            ("cjk", "日本語", 2, "…"),
            ("cjk", "日本語", 3, "日…"),
            ("cjk", "日本語", 4, "日…"),
            ("cjk", "日本語", 5, "日本…"),
            ("cjk", "日本語", 6, "日本語"),
            ("combining", "e\u{301}e\u{301}e\u{301}", 0, ""),
            ("combining", "e\u{301}e\u{301}e\u{301}", 1, "…"),
            ("combining", "e\u{301}e\u{301}e\u{301}", 2, "e\u{301}…"),
            (
                "combining",
                "e\u{301}e\u{301}e\u{301}",
                3,
                "e\u{301}e\u{301}e\u{301}",
            ),
            ("combining alone", "\u{301}", 0, ""),
            ("combining alone", "\u{301}", 1, "\u{301}"),
            ("emoji presentation", "❤\u{FE0F}", 0, ""),
            ("emoji presentation", "❤\u{FE0F}", 1, "…"),
            ("emoji presentation", "❤\u{FE0F}", 2, "❤\u{FE0F}"),
            ("emoji presentation", "❤\u{FE0F}x", 2, "❤…"),
            ("emoji presentation", "❤\u{FE0F}xy", 3, "❤\u{FE0F}…"),
            // Two cells whole, one as a sum of its characters.
            ("quote with selector", "‘\u{FE01}x", 2, "‘…"),
            ("quote with selector", "‘\u{FE01}x", 3, "‘\u{FE01}x"),
            // One cell whole, two as a sum of its characters.
            ("lam-alef ligature", "لاxy", 2, "لا…"),
            ("lam-alef ligature", "لاx", 2, "لاx"),
            ("text presentation", "⌚\u{FE0E}", 0, ""),
            ("text presentation", "⌚\u{FE0E}", 1, "⌚\u{FE0E}"),
            ("control", "a\u{1}b", 0, ""),
            ("control", "a\u{1}b", 1, "…"),
            ("control", "a\u{1}b", 2, "a…"),
            ("control", "a\u{1}b", 3, "a\u{FFFD}b"),
            ("bidi", "\u{202E}abc", 0, ""),
            ("bidi", "\u{202E}abc", 1, "…"),
            ("bidi", "\u{202E}abc", 2, "\u{FFFD}…"),
            ("bidi", "\u{202E}abc", 4, "\u{FFFD}abc"),
            ("zero-width space", "\u{200B}", 1, "\u{FFFD}"),
            // A halfwidth sound mark takes a cell of its own.
            ("halfwidth sound mark", "ｶﾞ", 1, "…"),
            ("halfwidth sound mark", "ｶﾞ", 2, "ｶﾞ"),
            ("halfwidth sound mark", "ｶﾞx", 2, "ｶ…"),
            ("halfwidth sound mark", "ｶﾞx", 3, "ｶﾞx"),
            ("halfwidth sound mark", "ﾊﾟﾊﾟ", 3, "ﾊﾟ…"),
        ];
        for &(class, text, budget, expect) in cases {
            assert_eq!(
                fit_cells(text, budget),
                expect,
                "{class} {text:?} at {budget}"
            );
        }
    }

    /// Where a selector makes the text narrower, the kept prefix still
    /// fits and is still a prefix, whichever one the search lands on.
    #[test]
    fn text_layout_fits_cell_budget_where_width_shrinks() {
        // `⌚` is two cells, `⌚\u{FE0E}` one.
        assert_eq!(cells("⌚"), 2);
        assert_eq!(cells("⌚\u{FE0E}"), 1);
        for budget in 0..=6 {
            check_fit("⌚\u{FE0E}abc", budget).expect("fits");
            check_fit("a⌚\u{FE0E}⌚\u{FE0E}⌚\u{FE0E}b", budget).expect("fits");
        }
    }

    /// unicode-width gives the halfwidth sound marks U+FF9E and U+FF9F no
    /// width, like the combining marks they are not; a terminal, and
    /// ratatui, give each a cell. The combining sound marks keep none.
    #[test]
    fn text_layout_counts_halfwidth_sound_marks() {
        assert_eq!(cells("ｶﾞ"), 2);
        assert_eq!(cells("ﾊﾟ"), 2);
        assert_eq!(cells("\u{FF9E}"), 1);
        assert_eq!(cells("ｶﾞｶﾞx"), 5);
        assert_eq!(cells("か\u{3099}"), 2);
        assert_eq!(pad_cells("ｶﾞ", 3), "ｶﾞ ");
        let text = "ｶﾞx";
        let at = locate_span(text, 6, 7).expect("locates");
        let layout = caret_layout(text, &at);
        assert_eq!((layout.pad, layout.carets), (2, 1));
        let at = locate_span(text, 0, 6).expect("locates");
        assert_eq!(caret_layout(text, &at).carets, 2);
    }

    /// A million combining marks fit any budget, and `fit_cells` still
    /// reads only its window: the result is a cut of that window, not the
    /// whole text. No timing involved.
    #[test]
    fn text_layout_fits_cell_budget_reads_a_bounded_prefix() {
        let text = format!("a{}", "\u{301}".repeat(1_000_000));
        assert_eq!(cells(&text), 1);
        for budget in [1, 2, 10, 80] {
            let (read, cut) = scan_window(&text, budget);
            assert!(cut, "a million characters pass every window");
            assert_eq!(read.chars().count(), scan_cap(budget));
            let out = fit_cells(&text, budget);
            let kept = out.strip_suffix(MARKER).expect("cut with a marker");
            assert!(
                sanitize_display_text(read).starts_with(kept),
                "{budget}: kept text outside the window"
            );
            assert!(out.chars().count() <= scan_cap(budget) + 1);
        }
        // Budget 10 reads `a` and 87 marks, which fit with the marker.
        let out = fit_cells(&text, 10);
        assert_eq!(out, format!("a{}…", "\u{301}".repeat(87)));
    }

    #[test]
    fn text_layout_pads_to_the_width() {
        assert_eq!(pad_cells("ab", 4), "ab  ");
        assert_eq!(pad_cells("日本語", 4), "日… ");
        assert_eq!(pad_cells("日本語", 5), "日本…");
        assert_eq!(pad_cells("", 3), "   ");
        assert_eq!(pad_cells("abc", 0), "");
        assert_eq!(pad_cells("\u{202E}", 2), "\u{FFFD} ");
    }

    /// Every row of [`SPAN_CASES`] through `locate_span` and
    /// `caret_layout`, against its literal expectation.
    #[test]
    fn shared_span_contract() {
        for case in SPAN_CASES {
            let located = locate_span(case.text, case.start, case.end);
            match (case.expect, located) {
                (None, None) => {}
                (Some(expect), Some(at)) => {
                    let layout = caret_layout(case.text, &at);
                    let excerpt = format!("{}{}{}", layout.before, layout.covered, layout.after);
                    assert_eq!(at.line, expect.line, "{}", case.name);
                    assert_eq!(excerpt, expect.excerpt, "{}", case.name);
                    assert_eq!(
                        sanitize_display_text(at.line_text(case.text)),
                        expect.excerpt,
                        "{}",
                        case.name
                    );
                    assert_eq!(layout.pad, expect.pad, "{}", case.name);
                    assert_eq!(layout.carets, expect.carets, "{}", case.name);
                    assert!(!excerpt.contains(['\r', '\n']), "{}", case.name);
                }
                (expect, located) => {
                    panic!("{}: expected {expect:?}, located {located:?}", case.name)
                }
            }
        }
    }

    /// The hand-worked offsets in [`SPAN_CASES`] point where the rows say.
    #[test]
    fn shared_span_contract_offsets_are_what_the_rows_say() {
        let issue = "service=\"日本\" | where x ~ /(/";
        assert_eq!(issue.find('('), Some(30));
        assert_eq!(issue.find('日'), Some(9));
        assert_eq!(&issue[9..15], "日本");
        assert_eq!(issue.len(), 32);
        assert_eq!(issue[..30].chars().count(), 26);

        let sound = "service=\"ｶﾞ\" | where x ~ /(/";
        assert_eq!(sound.find('('), Some(30));
        assert_eq!(sound[..30].chars().count(), 26);

        let ascii = "service=web | stats count( by host";
        assert_eq!(ascii.find("host"), Some(30));
        assert_eq!(&"user=résumé_ñ"[5..16], "résumé_ñ");
        assert_eq!("cafe\u{301} x".find('x'), Some(7));
        assert_eq!("❤\u{FE0F} x".find('x'), Some(7));
        assert_eq!("a\u{202E}b x".find('x'), Some(6));
        assert_eq!("a=é b".find('é'), Some(2));

        let crlf = "service=web\r\n| stats count( by host\r\n| head 5";
        assert_eq!(crlf.find("host"), Some(31));
        assert_eq!(&crlf[13..31], "| stats count( by ");
        let multi = "service=web\n| stats count(\nby host";
        assert_eq!(&multi[20..29], "count(\nby");
        assert_eq!(&multi[12..20], "| stats ");
    }

    /// `locate_span` is the rule trawl-mcp's `excerpt` and the web UI's
    /// `locate` each carried: the start's line, a `\r` before its `\n`
    /// or at the end of the text dropped, the span clipped to the line,
    /// and `multi_line` whenever the text holds a `\n`.
    #[test]
    fn shared_span_contract_locates_like_mcp_and_web() {
        let line = |line, multi_line, line_start, line_end, start, end| {
            Some(SpanLine {
                line,
                multi_line,
                line_start,
                line_end,
                start,
                end,
            })
        };
        assert_eq!(locate_span("", 0, 0), line(1, false, 0, 0, 0, 0));
        assert_eq!(locate_span("ab\ncd", 3, 5), line(2, true, 3, 5, 3, 5));
        assert_eq!(locate_span("ab\ncd", 1, 5), line(1, true, 0, 2, 1, 2));
        assert_eq!(locate_span("ab\ncd", 2, 2), line(1, true, 0, 2, 2, 2));
        assert_eq!(locate_span("x\r\nnext", 1, 2), line(1, true, 0, 1, 1, 1));
        assert_eq!(locate_span("x\r\nnext", 2, 3), line(1, true, 0, 1, 1, 1));
        assert_eq!(locate_span("x\r", 2, 2), line(1, false, 0, 1, 1, 1));
        assert_eq!(locate_span("a\rb", 1, 2), line(1, false, 0, 3, 1, 2));
        assert_eq!(locate_span("a\n\nb", 2, 2), line(2, true, 2, 2, 2, 2));
        assert_eq!(locate_span("abc", usize::MAX, usize::MAX), None);
        assert_eq!(locate_span("abc", 0, usize::MAX), None);
        assert_eq!(locate_span("abc", usize::MAX, 0), None);
    }
}
