// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rows as JSON arrays, and the cells too long to print (ADR-0058).
//!
//! Every printed cell is one JSON value: the cell's #214 wire JSON, from
//! [`trawl_api::value::wire_cell_text`], parsed back into a value. Event
//! text is therefore always a JSON string inside a JSON array, whatever it
//! holds: quotes, newlines, control characters or instructions stay data.
//!
//! A cell whose wire text is longer than [`ABBREVIATE_OVER_BYTES`] prints
//! as a JSON string holding the first bytes of that wire text, cut on a
//! character boundary, followed by a marker naming the omitted bytes and
//! the `read` call that returns the cell whole. The bytes are of the wire
//! text because the cell route serves byte ranges of exactly that text.

use std::fmt::Write as _;

use serde::Serialize;
use serde_json::Value as Json;
use trawl_api::value::{ColumnType, QueryResult, Value, wire_cell_text};

use crate::reply::{AbbreviatedCell, Column};

/// A cell whose wire JSON text is longer than this many bytes prints
/// abbreviated.
pub const ABBREVIATE_OVER_BYTES: usize = 512;

/// A page's printed columns, rows and abbreviated cells, ready for a
/// [`crate::reply::ResultView`].
#[derive(Debug, Clone, PartialEq)]
pub struct EncodedPage {
    /// The printed columns with their captured positions.
    pub columns: Vec<Column>,
    /// The rows; every cell is one JSON value.
    pub rows: Vec<Vec<Json>>,
    /// The cells printed shorter than they are.
    pub abbreviated: Vec<AbbreviatedCell>,
}

/// Encode a page of a captured result.
///
/// `positions` holds each page column's 0-based position in the captured
/// result (a [`trawl_api::results::ResultPage`]'s `positions`); a column
/// past its end is placed at its page index. Row `i` of `page` is captured
/// row `offset + i`.
///
/// # Errors
///
/// A cell that does not fit its column's type, refused the way the wire
/// encoder refuses it, with the column named.
pub fn encode_page(
    page: &QueryResult,
    positions: &[usize],
    offset: u64,
) -> serde_json::Result<EncodedPage> {
    let columns: Vec<Column> = page
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| Column {
            name: column.name.clone(),
            position: as_u64(positions.get(index).copied().unwrap_or(index)),
            ty: column.ty.to_string(),
        })
        .collect();
    let mut rows = Vec::with_capacity(page.rows.len());
    let mut abbreviated = Vec::new();
    for (index, cells) in page.rows.iter().enumerate() {
        let row = offset + as_u64(index);
        let mut printed = Vec::with_capacity(cells.len());
        for ((column, value), position) in page.columns.iter().zip(cells).zip(&columns) {
            let (cell, cut) = encode_cell(&column.name, &column.ty, value, row, position.position)?;
            printed.push(cell);
            abbreviated.extend(cut);
        }
        rows.push(printed);
    }
    Ok(EncodedPage {
        columns,
        rows,
        abbreviated,
    })
}

/// Encode one cell of captured row `row`, column position `column`.
///
/// The cell's JSON value, and the [`AbbreviatedCell`] entry when its wire
/// text was longer than [`ABBREVIATE_OVER_BYTES`].
///
/// # Errors
///
/// A cell that does not fit `ty`, as [`wire_cell_text`] refuses it.
pub fn encode_cell(
    name: &str,
    ty: &ColumnType,
    value: &Value,
    row: u64,
    column: u64,
) -> serde_json::Result<(Json, Option<AbbreviatedCell>)> {
    let wire = wire_cell_text(name, ty, value)?;
    if wire.len() <= ABBREVIATE_OVER_BYTES {
        return Ok((serde_json::from_str(&wire)?, None));
    }
    let kept = floor_char_boundary(&wire, ABBREVIATE_OVER_BYTES);
    let omitted = wire.len() - kept;
    let text = format!(
        "{} …[+{omitted} bytes: read action=cell row={row} column={column}]",
        &wire[..kept]
    );
    Ok((
        Json::String(text),
        Some(AbbreviatedCell {
            row,
            column,
            total_bytes: as_u64(wire.len()),
        }),
    ))
}

/// One printed row: a compact JSON array on one line. [`json_text`]
/// escapes every line break and control character inside a string, so a
/// row never spans two lines.
#[must_use]
pub fn row_line(row: &[Json]) -> String {
    json_text(row)
}

/// The columns line: each printed column's position, its name as a JSON
/// string, and its wire type.
#[must_use]
pub fn columns_line(columns: &[Column]) -> String {
    let listed: Vec<String> = columns
        .iter()
        .map(|column| {
            format!(
                "{} {} {}",
                column.position,
                json_string(&column.name),
                column.ty
            )
        })
        .collect();
    format!("columns (position name type): {}", listed.join(", "))
}

/// `text` as a JSON string literal, escaped as [`json_text`] escapes.
pub(crate) fn json_string(text: &str) -> String {
    json_text(text)
}

/// `value` as compact JSON text with no raw control character in it.
///
/// The JSON encoder escapes the C0 controls but leaves DEL, the C1
/// controls (a terminal reads some as escape sequences) and the Unicode
/// line and paragraph separators raw. Outside a string JSON text is ASCII
/// punctuation, so every such character sits inside a string, where its
/// `\uXXXX` escape decodes to the same value.
#[must_use]
pub fn json_text<T: Serialize + ?Sized>(value: &T) -> String {
    // Values built from JSON values and strings always serialize.
    let text = serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
    if !text.chars().any(needs_escape) {
        return text;
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if needs_escape(c) {
            let _ = write!(out, "\\u{:04x}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    out
}

/// Whether `c` stays raw in the JSON encoder's output but must not reach
/// the text block raw.
fn needs_escape(c: char) -> bool {
    c.is_control() || c == '\u{2028}' || c == '\u{2029}'
}

/// The largest character boundary of `text` at or below `index`.
pub(crate) fn floor_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    let mut at = index;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// A count or index as the `u64` the reply carries.
pub(crate) fn as_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cell_at_the_limit_prints_whole() {
        // A JSON string of 510 characters plus its two quotes is 512 bytes.
        let value = Value::String("a".repeat(510));
        let (cell, cut) = encode_cell("msg", &ColumnType::String, &value, 0, 0).unwrap();
        assert_eq!(cell, Json::String("a".repeat(510)));
        assert!(cut.is_none());
    }

    #[test]
    fn a_cell_over_the_limit_is_cut_on_a_char_boundary_with_a_marker() {
        // 511 bytes of `a`, then a three-byte character straddling byte 512.
        let value = Value::String(format!("{}€tail", "a".repeat(510)));
        let wire = wire_cell_text("msg", &ColumnType::String, &value).unwrap();
        let (cell, cut) = encode_cell("msg", &ColumnType::String, &value, 7, 3).unwrap();
        let Json::String(text) = cell else {
            panic!("an abbreviated cell is a string");
        };
        let kept = 511;
        assert_eq!(&text[..kept], &wire[..kept]);
        assert_eq!(
            &text[kept..],
            format!(
                " …[+{} bytes: read action=cell row=7 column=3]",
                wire.len() - kept
            )
        );
        assert_eq!(
            cut,
            Some(AbbreviatedCell {
                row: 7,
                column: 3,
                total_bytes: as_u64(wire.len()),
            })
        );
    }

    #[test]
    fn raw_controls_the_encoder_keeps_are_escaped_and_decode_back() {
        let hostile = "a\u{7f}b\u{9b}31mc\u{2028}d\n";
        let text = json_string(hostile);
        assert_eq!(text, r#""a\u007fb\u009b31mc\u2028d\n""#);
        assert_eq!(serde_json::from_str::<String>(&text).unwrap(), hostile);
    }

    #[test]
    fn integers_stay_exact_decimal_strings() {
        let (cell, _) =
            encode_cell("n", &ColumnType::Integer, &Value::UInt(u64::MAX), 0, 0).unwrap();
        assert_eq!(cell, Json::String(u64::MAX.to_string()));
    }
}
