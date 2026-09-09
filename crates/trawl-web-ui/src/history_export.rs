// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure serialization of the loaded history rows, in their supplied order.

use std::fmt::Write as _;

use trawl_api::HistoryEntryResponse;
use trawl_api::csv::sanitize_csv_formula;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryExportFormat {
    Csv,
    Json,
}

impl HistoryExportFormat {
    #[must_use]
    pub const fn filename(self) -> &'static str {
        match self {
            Self::Csv => "trawl-history.csv",
            Self::Json => "trawl-history.json",
        }
    }

    #[must_use]
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Csv => "text/csv",
            Self::Json => "application/json",
        }
    }
}

/// Serialize exactly these rows. Filtering and pagination belong to the caller.
pub fn serialize_history(
    entries: &[HistoryEntryResponse],
    format: HistoryExportFormat,
) -> Result<Vec<u8>, serde_json::Error> {
    match format {
        HistoryExportFormat::Json => serde_json::to_vec(entries),
        HistoryExportFormat::Csv => {
            let mut csv = String::from("id,executed_at,query,status,row_count,duration_ms\r\n");
            for entry in entries {
                write!(csv, "{},", entry.id).expect("writing to a String cannot fail");
                write_text_cell(&mut csv, &entry.executed_at);
                csv.push(',');
                write_text_cell(&mut csv, &entry.query);
                csv.push(',');
                write_text_cell(&mut csv, &entry.status.to_string());
                write!(csv, ",{},{}\r\n", entry.row_count, entry.duration_ms)
                    .expect("writing to a String cannot fail");
            }
            Ok(csv.into_bytes())
        }
    }
}

fn write_text_cell(csv: &mut String, text: &str) {
    let text = sanitize_csv_formula(text);
    if text.contains([',', '"', '\r', '\n']) {
        csv.push('"');
        csv.push_str(&text.replace('"', "\"\""));
        csv.push('"');
    } else {
        csv.push_str(&text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trawl_api::QueryStatus;

    fn entry(id: i64, query: &str) -> HistoryEntryResponse {
        HistoryEntryResponse {
            id,
            query: query.into(),
            executed_at: "2026-09-08T12:00:00Z".into(),
            duration_ms: 12,
            row_count: 3,
            status: QueryStatus::Success,
        }
    }

    fn csv(entries: &[HistoryEntryResponse]) -> String {
        String::from_utf8(serialize_history(entries, HistoryExportFormat::Csv).unwrap()).unwrap()
    }

    #[test]
    fn csv_formula_neutralizes_all_six_prefixes_in_text_cells() {
        for prefix in ['=', '+', '-', '@', '\t', '|'] {
            let text = format!("{prefix}cmd()");
            let mut row = entry(1, &text);
            row.executed_at.clone_from(&text);
            assert_eq!(
                csv(&[row]),
                format!(
                    "id,executed_at,query,status,row_count,duration_ms\r\n1,'{text},'{text},success,3,12\r\n"
                )
            );
        }
    }

    #[test]
    fn csv_formula_quotes_after_sanitizing_and_preserves_unicode() {
        let row = entry(-42, "=cmd(\"a,b\")\r\n雪");
        assert_eq!(
            csv(&[row]),
            concat!(
                "id,executed_at,query,status,row_count,duration_ms\r\n",
                "-42,2026-09-08T12:00:00Z,\"'=cmd(\"\"a,b\"\")\r\n雪\",success,3,12\r\n"
            )
        );
    }

    #[test]
    fn csv_formula_preserves_safe_and_empty_text_and_numeric_cells() {
        for text in ["", "safe", "雪", "200", " =cmd()", "'=cmd()"] {
            assert_eq!(
                csv(&[entry(-42, text)]),
                format!(
                    "id,executed_at,query,status,row_count,duration_ms\r\n-42,2026-09-08T12:00:00Z,{text},success,3,12\r\n"
                )
            );
        }
    }

    #[test]
    fn csv_formula_keeps_header_and_input_row_order() {
        assert_eq!(
            csv(&[]),
            "id,executed_at,query,status,row_count,duration_ms\r\n"
        );
        assert_eq!(
            csv(&[entry(9, "newest"), entry(2, "older")]),
            concat!(
                "id,executed_at,query,status,row_count,duration_ms\r\n",
                "9,2026-09-08T12:00:00Z,newest,success,3,12\r\n",
                "2,2026-09-08T12:00:00Z,older,success,3,12\r\n"
            )
        );
    }

    #[test]
    fn json_keeps_original_entry_objects_and_order() {
        let rows = [entry(9, "=cmd(\"a,b\")\r\n雪"), entry(-42, "+text")];
        let bytes = serialize_history(&rows, HistoryExportFormat::Json).unwrap();
        assert_eq!(bytes, serde_json::to_vec(&rows).unwrap());
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value[0]["query"], rows[0].query);
        assert_eq!(value[1]["id"], -42);
        assert_eq!(
            serialize_history(&[], HistoryExportFormat::Json).unwrap(),
            b"[]"
        );
    }

    #[test]
    fn formats_name_downloads_and_media_types() {
        assert_eq!(HistoryExportFormat::Csv.filename(), "trawl-history.csv");
        assert_eq!(HistoryExportFormat::Csv.mime(), "text/csv");
        assert_eq!(HistoryExportFormat::Json.filename(), "trawl-history.json");
        assert_eq!(HistoryExportFormat::Json.mime(), "application/json");
    }
}
