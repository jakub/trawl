// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rows as JSON arrays, and the cells too long to print (ADR-0058).

use std::fmt::Write as _;

use serde::Serialize;
use serde_json::Value as Json;
use trawl_api::value::{ColumnType, QueryResult, Value, wire_cell_text};
use trawl_core::sanitize::is_unsafe_display_char;

use crate::reply::{AbbreviatedCell, Column};

/// A cell whose wire JSON text is longer than this many bytes prints
/// abbreviated.
pub const ABBREVIATE_OVER_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq)]
pub struct EncodedPage {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Json>>,
    pub abbreviated: Vec<AbbreviatedCell>,
}

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

#[must_use]
pub fn row_line(row: &[Json]) -> String {
    json_text(row)
}

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

pub(crate) fn json_string(text: &str) -> String {
    json_text(text)
}

/// A JSON encoder escapes the C0 controls itself, but leaves raw DEL, the
/// C1 controls, the bidi controls, the zero-widths, joiners and BOM, the
/// soft hyphen and the Unicode line and paragraph separators. Outside a
/// string JSON text is ASCII punctuation, so every such character sits
/// inside a string, where its escape decodes to the same value.
#[must_use]
pub fn json_text<T: Serialize + ?Sized>(value: &T) -> String {
    // Values built from JSON values and strings always serialize.
    let text = serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
    if !text.chars().any(is_unsafe_display_char) {
        return text;
    }
    escape_unsafe(&text)
}

/// `value` as a reply's `structuredContent` mirror: its JSON value with
/// each [`is_unsafe_display_char`] in every string and object key replaced
/// by the escape text [`json_text`] prints for it.
///
/// A proposed call's arguments are the next call's input, so the escaping
/// never rewrites them: `next_actions` leaves out each call whose arguments
/// hold such a character.
pub(crate) fn mirror<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<Json> {
    let mut mirror = serde_json::to_value(value)?;
    if let Some(Json::Array(actions)) = mirror.get_mut("next_actions") {
        actions.retain(|action| !action.get("arguments").is_some_and(holds_unsafe));
    }
    escape_strings(&mut mirror);
    Ok(mirror)
}

fn holds_unsafe(value: &Json) -> bool {
    match value {
        Json::String(text) => text.chars().any(is_unsafe_display_char),
        Json::Array(items) => items.iter().any(holds_unsafe),
        Json::Object(map) => map
            .iter()
            .any(|(key, item)| key.chars().any(is_unsafe_display_char) || holds_unsafe(item)),
        Json::Null | Json::Bool(_) | Json::Number(_) => false,
    }
}

fn escape_strings(value: &mut Json) {
    match value {
        Json::String(text) => {
            if text.chars().any(is_unsafe_display_char) {
                *text = escape_unsafe(text);
            }
        }
        Json::Array(items) => items.iter_mut().for_each(escape_strings),
        Json::Object(map) => {
            map.values_mut().for_each(escape_strings);
            if map
                .keys()
                .any(|key| key.chars().any(is_unsafe_display_char))
            {
                *map = std::mem::take(map)
                    .into_iter()
                    .map(|(key, item)| (escape_unsafe(&key), item))
                    .collect();
            }
        }
        Json::Null | Json::Bool(_) | Json::Number(_) => {}
    }
}

/// Every [`is_unsafe_display_char`] is in the Basic Multilingual Plane, so
/// `\uXXXX` always has four hex digits.
fn escape_unsafe(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        match c {
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if is_unsafe_display_char(c) => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

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

pub(crate) fn as_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(value: &Json, out: &mut Vec<String>) {
        match value {
            Json::String(text) => out.push(text.clone()),
            Json::Array(items) => items.iter().for_each(|item| strings(item, out)),
            Json::Object(map) => {
                for (key, item) in map {
                    out.push(key.clone());
                    strings(item, out);
                }
            }
            _ => {}
        }
    }

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
    fn the_mirror_holds_the_escapes_the_text_prints() {
        let hostile = "a\u{7f}b\u{202e}c\u{1b}d\n\te";
        let value = serde_json::json!({
            "text": hostile,
            "list": [hostile, 1, null],
            "keyed": {hostile: true},
        });
        let mirrored = mirror(&value).unwrap();
        let expected = r"a\u007fb\u202ec\u001bd\n\te";
        assert_eq!(mirrored["text"], Json::String(expected.to_owned()));
        assert_eq!(mirrored["list"][0], Json::String(expected.to_owned()));
        assert_eq!(mirrored["keyed"][expected], Json::Bool(true));
        assert_eq!(json_string(hostile), format!("\"{expected}\""));
        assert_eq!(
            mirrored["text"].to_string(),
            r#""a\\u007fb\\u202ec\\u001bd\\n\\te""#
        );
    }

    #[test]
    fn every_unsafe_character_is_in_the_mirror_as_the_text_prints_it() {
        let unsafe_chars = (0..=0x10_ffff)
            .filter_map(char::from_u32)
            .filter(|&c| is_unsafe_display_char(c));
        for c in unsafe_chars {
            let raw = c.to_string();
            let text = json_string(&raw);
            let printed = &text[1..text.len() - 1];
            // The character as a key, a list item and a value nested two
            // levels down, so a path that skips the recursion into keys or
            // into nested values leaves it raw.
            let nested =
                serde_json::json!({ raw.as_str(): [raw.as_str(), {raw.as_str(): raw.as_str()}] });
            let mirrored = mirror(&nested).unwrap();
            let mut found = Vec::new();
            strings(&mirrored, &mut found);
            assert_eq!(found.len(), 4, "U+{:04x}: {mirrored}", u32::from(c));
            for string in found {
                assert_eq!(string, printed, "U+{:04x}", u32::from(c));
                assert!(
                    !string.chars().any(is_unsafe_display_char),
                    "U+{:04x}",
                    u32::from(c)
                );
            }
        }
    }

    #[test]
    fn the_mirror_keeps_a_proposed_call_whole_or_leaves_it_out() {
        let clean = serde_json::json!({
            "tool": "search",
            "arguments": {"mode": "execute", "query": "message=\"a b\""},
            "reason": "run it",
        });
        let lined = serde_json::json!({
            "tool": "search",
            "arguments": {"mode": "execute", "query": "message=\"a\nb\""},
            "reason": "run it",
        });
        let hidden = serde_json::json!({
            "tool": "fields",
            "arguments": {"source": "service", "service": "api\u{200b}"},
            "reason": "list it",
        });
        let keyed = serde_json::json!({
            "tool": "search",
            "arguments": {"mode": "execute", "query\u{202e}": "x"},
            "reason": "run it",
        });
        let value = serde_json::json!({
            "next_actions": [clean, lined, hidden, keyed],
            "entries": {"payload": {"next_actions": [hidden]}},
        });
        let mirrored = mirror(&value).unwrap();
        assert_eq!(mirrored["next_actions"], serde_json::json!([clean]));
        // A payload's key of the same name is event data, escaped as such.
        assert_eq!(
            mirrored["entries"]["payload"]["next_actions"][0]["arguments"]["service"],
            Json::String("api\\u200b".to_owned())
        );
    }

    #[test]
    fn integers_stay_exact_decimal_strings() {
        let (cell, _) =
            encode_cell("n", &ColumnType::Integer, &Value::UInt(u64::MAX), 0, 0).unwrap();
        assert_eq!(cell, Json::String(u64::MAX.to_string()));
    }
}
