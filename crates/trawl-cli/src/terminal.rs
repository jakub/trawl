// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Sender and server text on its way to the operator's terminal
//! (ADR-0039, amendment of 2026-11-09).
//!
//! Field names and event values are chosen by whoever can ingest, so a sink
//! that can reach a terminal must show them through
//! [`trawl_core::sanitize`]. A prose line sanitizes each value it
//! interpolates at its own site. A table draws each header and cell through
//! [`table_cell`]. `json` and `csv` change only on a [`Destination::Terminal`]:
//! in a pipe or a file they carry every value exactly.

use std::io::{self, IsTerminal, Write};

use serde::Serialize;
use serde_json::ser::{CharEscape, CompactFormatter, Formatter, PrettyFormatter};
use trawl_core::sanitize::{is_unsafe_display_char, sanitize_display_text};

/// Where a renderer's output goes, as far as sender text is concerned.
///
/// A command decides it once, from its stdout, and hands it to the
/// renderers; a renderer never asks for itself. An `-o` file is always
/// [`Data`](Self::Data), whatever stdout is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Destination {
    /// An operator reads it.
    Terminal,
    /// A pipe or a file: `json` and `csv` carry every value exactly.
    Data,
}

impl Destination {
    /// What this process's stdout is.
    pub fn stdout() -> Self {
        if io::stdout().is_terminal() {
            Self::Terminal
        } else {
            Self::Data
        }
    }
}

/// `value` as one line of JSON, its record-ending newline included.
///
/// For [`Destination::Data`] the line is `serde_json::to_writer`'s bytes.
/// For a terminal, every unsafe character in a key or a string, at any
/// depth, is a `\uXXXX` escape instead, so the line decodes to the same
/// value and none of those characters reaches the terminal raw.
pub(crate) fn write_json_line<T: Serialize + ?Sized>(
    out: &mut impl Write,
    value: &T,
    destination: Destination,
) -> io::Result<()> {
    match destination {
        Destination::Data => serde_json::to_writer(&mut *out, value),
        Destination::Terminal => value.serialize(&mut serde_json::Serializer::with_formatter(
            &mut *out,
            TerminalFormatter(CompactFormatter),
        )),
    }
    .map_err(io::Error::other)?;
    writeln!(out)
}

/// `value` as pretty JSON: `serde_json::to_string_pretty`'s text for
/// [`Destination::Data`], and for a terminal the same layout with every
/// unsafe character in a key or a string escaped as in [`write_json_line`].
pub(crate) fn json_pretty<T: Serialize + ?Sized>(
    value: &T,
    destination: Destination,
) -> serde_json::Result<String> {
    match destination {
        Destination::Data => serde_json::to_string_pretty(value),
        Destination::Terminal => {
            let mut text = Vec::new();
            value.serialize(&mut serde_json::Serializer::with_formatter(
                &mut text,
                TerminalFormatter(PrettyFormatter::new()),
            ))?;
            Ok(String::from_utf8(text).expect("serde_json writes UTF-8 and every escape is ASCII"))
        }
    }
}

/// A `serde_json` formatter that writes each unsafe character of string
/// content, keys included, as a `\uXXXX` escape, one per UTF-16 unit in
/// lowercase hex. That covers the controls `serde_json` would spell `\n`,
/// `\t`, `\r`, `\b` and `\f`.
///
/// Everything else is the wrapped formatter's: the quote, reverse solidus
/// and solidus escapes, numbers and literals, and the whole layout, so a
/// pretty formatter keeps its structural newlines and indentation.
struct TerminalFormatter<F>(F);

/// Write `c` as JSON `\uXXXX` escapes.
fn write_unicode_escape<W: ?Sized + Write>(writer: &mut W, c: char) -> io::Result<()> {
    let mut units = [0; 2];
    for unit in c.encode_utf16(&mut units) {
        write!(writer, "\\u{unit:04x}")?;
    }
    Ok(())
}

/// Forward each named `Formatter` method, with its extra arguments, to the
/// wrapped formatter.
macro_rules! forward {
    ($($method:ident($($arg:ident: $ty:ty),*);)*) => {
        $(
            fn $method<W: ?Sized + Write>(
                &mut self,
                writer: &mut W
                $(, $arg: $ty)*
            ) -> io::Result<()> {
                self.0.$method(writer $(, $arg)*)
            }
        )*
    };
}

impl<F: Formatter> Formatter for TerminalFormatter<F> {
    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        let mut rest = fragment;
        while let Some((at, c)) = rest
            .char_indices()
            .find(|&(_, c)| is_unsafe_display_char(c))
        {
            if at > 0 {
                self.0.write_string_fragment(writer, &rest[..at])?;
            }
            write_unicode_escape(writer, c)?;
            rest = &rest[at + c.len_utf8()..];
        }
        if rest.is_empty() {
            Ok(())
        } else {
            self.0.write_string_fragment(writer, rest)
        }
    }

    fn write_char_escape<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        char_escape: CharEscape,
    ) -> io::Result<()> {
        let c = match &char_escape {
            CharEscape::Quote => '"',
            CharEscape::ReverseSolidus => '\\',
            CharEscape::Solidus => '/',
            CharEscape::Backspace => '\u{8}',
            CharEscape::FormFeed => '\u{c}',
            CharEscape::LineFeed => '\n',
            CharEscape::CarriageReturn => '\r',
            CharEscape::Tab => '\t',
            CharEscape::AsciiControl(byte) => char::from(*byte),
        };
        if is_unsafe_display_char(c) {
            write_unicode_escape(writer, c)
        } else {
            self.0.write_char_escape(writer, char_escape)
        }
    }

    forward! {
        write_null();
        write_bool(value: bool);
        write_i8(value: i8);
        write_i16(value: i16);
        write_i32(value: i32);
        write_i64(value: i64);
        write_i128(value: i128);
        write_u8(value: u8);
        write_u16(value: u16);
        write_u32(value: u32);
        write_u64(value: u64);
        write_u128(value: u128);
        write_f32(value: f32);
        write_f64(value: f64);
        write_number_str(value: &str);
        begin_string();
        end_string();
        write_byte_array(value: &[u8]);
        begin_array();
        end_array();
        begin_array_value(first: bool);
        end_array_value();
        begin_object();
        end_object();
        begin_object_key(first: bool);
        end_object_key();
        begin_object_value();
        end_object_value();
        write_raw_fragment(fragment: &str);
    }
}

/// A table header or cell as the table draws it, after value presentation.
///
/// A line break, `\n` or `\r\n`, stays a line inside the cell: comfy-table
/// splits a cell on `\n` and draws its borders on every line, so a break
/// cannot leave the cell. Every other unsafe character on a line becomes
/// U+FFFD, a tab and a lone `\r` included. Tables are a human format, so
/// this holds wherever the table goes, an `-o` file included.
pub(crate) fn table_cell(text: &str) -> String {
    text.replace("\r\n", "\n")
        .split('\n')
        .map(sanitize_display_text)
        .collect::<Vec<_>>()
        .join("\n")
}
