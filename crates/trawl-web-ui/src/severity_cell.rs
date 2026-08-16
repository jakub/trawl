// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Severity PRESENTATION, keyed off `_severity` alone (ADR-0013 §9).
//!
//! Data-in / data-out, and ungated, so the two decisions that are easy to
//! get subtly wrong are table-testable natively rather than only in a
//! browser: which text a `_severity` cell shows, and which rows the
//! histogram paints as errors.
//!
//! Two rules, both load-bearing:
//!
//! - **`_severity` only.** A bare `severity` column is ordinary sender
//!   data now, and the `severity_text` fallback died with the column, so
//!   coloring one would be trawl assigning meaning to a bare name.
//! - **display, never the wire.** Results DISPLAY the token (`17` →
//!   `error`), because that is the vocabulary the query language uses —
//!   but json/csv/SSE keep the NUMBER, so arithmetic consumers are
//!   untouched. Rendering is presentation, and only presentation.
//!
//! The token text comes from `trawl_core::severity::otel_name`, the same
//! injective table the `SEVERITY` pin's patterns match, so a cell reads
//! as exactly the value that would filter it.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::value::Value;

/// Which result column presentation reads severity from: `_severity`,
/// and nothing else (ADR-0013 §9).
///
/// Both SPA call sites — the results table's cell rendering and the
/// histogram's error bars — go through this one lookup, so "reads
/// `_severity` only" is decided in a natively-tested function rather
/// than twice in wasm-gated component code.
#[must_use]
pub fn severity_column<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<usize> {
    names
        .into_iter()
        .position(|name| name == trawl_core::schema::SEVERITY)
}

/// Which result columns render as severity TOKENS: `_severity`, plus the
/// columns the response DECLARED (`sev()` output — ADR-0013 slice 2,
/// ruling 9).
///
/// The membership rule is `trawl_core::severity::renders_as_severity`,
/// the one every renderer asks; only the index collection is local.
/// Distinct from [`severity_column`], which stays `_severity`-ONLY
/// because the histogram's error bucketing is a statement about the
/// EVENT's severity, not about any column a query happened to compute.
#[must_use]
pub fn severity_columns<'a>(
    names: impl IntoIterator<Item = &'a str>,
    declared: &[String],
) -> Vec<usize> {
    names
        .into_iter()
        .enumerate()
        .filter(|(_, name)| trawl_core::severity::renders_as_severity(name, declared))
        .map(|(i, _)| i)
        .collect()
}

/// The `SeverityNumber` a severity cell holds, if it holds one.
///
/// The column is BIGINT on the wire; a string is tolerated only because
/// a hot-buffer row can carry the JSON shape before conformance renders
/// it. Both shapes read through the ONE kernel
/// (`trawl_core::severity::reading_*`, ADR-0013 slice 2 ruling 9), so a
/// cell displays exactly the number ingest would have derived and
/// `sev()` would compute — no surface has its own severity vocabulary.
/// Every other shape (a float, a bool, an array) names no rung and has
/// no reading, exactly as the kernel says.
#[must_use]
pub fn severity_number(v: &Value) -> Option<u8> {
    use trawl_core::severity::Dialect;
    match v {
        Value::Integer(n) => trawl_core::severity::reading_number(*n, Dialect::Otel),
        Value::String(s) => trawl_core::severity::reading_text(s, Dialect::Otel),
        _ => None,
    }
}

/// A `_severity` cell's display text: the `OTel` short name (`17` →
/// `error`, `18` → `error2`), or the raw value where there is no reading.
#[must_use]
pub fn severity_display(v: &Value) -> String {
    severity_number(v)
        .and_then(trawl_core::severity::otel_name)
        .map_or_else(|| trawl_api::display::value_to_string(v), str::to_owned)
}

/// CSS class for a `_severity` value, by `OTel` band.
#[must_use]
pub fn severity_class(v: &Value) -> &'static str {
    match severity_number(v).and_then(trawl_core::severity::band_name) {
        Some("error" | "fatal") => "lvl lvl-error",
        Some("warn") => "lvl lvl-warn",
        Some("info") => "lvl lvl-info",
        Some("debug" | "trace") => "lvl lvl-debug",
        _ => "lvl",
    }
}

/// Whether a row sits at or above the `OTel` ERROR band (17), read off
/// `_severity` alone.
#[must_use]
pub fn row_is_error(row: &[Value], severity_idx: Option<usize>) -> bool {
    matches!(
        severity_idx.and_then(|i| row.get(i)),
        Some(Value::Integer(n)) if *n >= 17
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Results DISPLAY the token, never the number (ADR-0013 §6) — and
    /// the rendering is INJECTIVE, so `error2` is not shown as `error`.
    #[test]
    fn a_severity_cell_renders_its_otel_short_name() {
        assert_eq!(severity_display(&Value::Integer(17)), "error");
        assert_eq!(severity_display(&Value::Integer(18)), "error2");
        assert_eq!(severity_display(&Value::Integer(13)), "warn");
        assert_eq!(severity_display(&Value::Integer(1)), "trace");
        assert_eq!(severity_display(&Value::Integer(24)), "fatal4");
        // Out of the ladder, or absent: the raw value, never a guess.
        assert_eq!(severity_display(&Value::Integer(99)), "99");
        assert_eq!(severity_display(&Value::Null), "NULL");
    }

    /// Coloring is by BAND, over the same reading.
    #[test]
    fn a_severity_cell_colors_by_band() {
        assert_eq!(severity_class(&Value::Integer(17)), "lvl lvl-error");
        assert_eq!(severity_class(&Value::Integer(20)), "lvl lvl-error");
        assert_eq!(severity_class(&Value::Integer(21)), "lvl lvl-error");
        assert_eq!(severity_class(&Value::Integer(13)), "lvl lvl-warn");
        assert_eq!(severity_class(&Value::Integer(9)), "lvl lvl-info");
        assert_eq!(severity_class(&Value::Integer(5)), "lvl lvl-debug");
        assert_eq!(severity_class(&Value::Integer(99)), "lvl");
        assert_eq!(severity_class(&Value::Null), "lvl");
    }

    /// A hot-buffer row can still carry the JSON shape before conformance
    /// renders it, so a string reads through the same ladder — the ONE
    /// kernel, so a numeric string and a padded token read here exactly
    /// as they read at ingest and under `sev()`.
    #[test]
    fn a_string_severity_reads_through_the_same_ladder() {
        assert_eq!(severity_number(&Value::String("error".into())), Some(17));
        assert_eq!(severity_number(&Value::String("error2".into())), Some(18));
        assert_eq!(severity_number(&Value::String("err".into())), Some(17));
        assert_eq!(severity_number(&Value::String("17".into())), Some(17));
        assert_eq!(severity_number(&Value::String(" error ".into())), Some(17));
        // No reading is no reading: the cell renders its raw value.
        assert_eq!(severity_number(&Value::String("gold".into())), None);
        assert_eq!(severity_number(&Value::String("1.5".into())), None);
        assert_eq!(severity_number(&Value::String("25".into())), None);
        assert_eq!(severity_display(&Value::String("gold".into())), "gold");
        assert_eq!(severity_display(&Value::String("17".into())), "error");
    }

    /// The INTEGER shape — what the wire actually carries — is unchanged
    /// by the delegation: the ladder guard is the kernel's own.
    #[test]
    fn an_integer_severity_reads_the_ladder_and_nothing_else() {
        for n in 1..=24i64 {
            assert_eq!(
                severity_number(&Value::Integer(n)),
                u8::try_from(n).ok(),
                "ladder {n}"
            );
        }
        assert_eq!(severity_number(&Value::Integer(0)), None);
        assert_eq!(severity_number(&Value::Integer(25)), None);
        assert_eq!(severity_number(&Value::Integer(-1)), None);
        assert_eq!(severity_number(&Value::Integer(i64::MAX)), None);
        // A shape that names no rung.
        assert_eq!(severity_number(&Value::Float(17.0)), None);
        assert_eq!(severity_number(&Value::Boolean(true)), None);
        assert_eq!(severity_number(&Value::Null), None);
    }

    /// Cell rendering reads `_severity` AND whatever the response
    /// declared — a `sev()` output renders its token like any other
    /// severity column, and an undeclared bare name renders as data.
    #[test]
    fn declared_severity_columns_join_the_name_keyed_one() {
        let cols = ["_time", "severity", "_severity", "s"];
        assert_eq!(severity_columns(cols, &[]), vec![2]);
        assert_eq!(
            severity_columns(cols, &["s".to_owned()]),
            vec![2, 3],
            "a declared column joins the name-keyed one"
        );
        assert_eq!(
            severity_columns(["_time", "message"], &["s".to_owned()]),
            Vec::<usize>::new(),
            "a declared name the result does not carry matches nothing"
        );
    }

    /// The COLUMN both SPA surfaces read is `_severity` and nothing
    /// else: a bare `severity` is ordinary sender data now, and
    /// `severity_text` is gone — neither may be mistaken for the slot.
    #[test]
    fn presentation_binds_the_derived_column_alone() {
        let cols = ["_time", "severity_text", "severity", "_severity", "message"];
        assert_eq!(severity_column(cols), Some(3));

        // Same row set WITHOUT the derived slot: no column, so nothing
        // is colored and no bar is painted red.
        assert_eq!(
            severity_column(["_time", "severity_text", "severity", "message"]),
            None
        );
        assert_eq!(severity_column(["_severity"]), Some(0));
        assert_eq!(severity_column(std::iter::empty()), None);
    }

    /// Bucketing reads `_severity` ONLY: a bare `severity` column is
    /// ordinary sender data and colors nothing.
    #[test]
    fn error_bucketing_keys_off_the_derived_slot_alone() {
        let row = vec![Value::String("t".into()), Value::Integer(17)];
        assert!(row_is_error(&row, Some(1)));
        assert!(
            !row_is_error(&row, Some(0)),
            "a non-integer cell is not an error"
        );
        assert!(
            !row_is_error(&row, None),
            "no `_severity` column, no error bar"
        );

        for (n, expected) in [(16, false), (17, true), (24, true), (9, false)] {
            let row = vec![Value::Integer(n)];
            assert_eq!(row_is_error(&row, Some(0)), expected, "severity {n}");
        }
    }
}
