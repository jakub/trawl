// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The declared event envelope (ADR-0009): field names, reserved keys, and
//! wire aliases.
//!
//! Namespace rule: `_` marks metadata about the record's handling; no prefix
//! means data about the event. `severity` is server-derived but carries no
//! prefix because it is content.

/// Event time (TIMESTAMP, required).
pub const TIME: &str = "_time";
/// Server-stamped arrival time (TIMESTAMP, required; client cannot set).
pub const INGESTED: &str = "_ingested";
/// Most original form available (VARCHAR, required; server-filled).
pub const RAW: &str = "_raw";
/// Comma-separated repair codes (VARCHAR, nullable).
pub const REPAIRS: &str = "_repairs";
/// Environment — path segment 1, mirrored as a column (VARCHAR, required).
pub const ENV: &str = "env";
/// Service — path segment 2, mirrored as a column (VARCHAR, required).
pub const SERVICE: &str = "service";
/// Origin host (VARCHAR, required; column, not a path segment).
pub const HOST: &str = "host";
/// `OTel` `SeverityNumber` 1-24 (INTEGER, derived).
pub const SEVERITY: &str = "severity";
/// Original severity text, verbatim (VARCHAR, optional).
pub const SEVERITY_TEXT: &str = "severity_text";
/// The important part of the line (VARCHAR, by convention).
pub const MESSAGE: &str = "message";

/// Envelope columns stored as TIMESTAMP on disk. Every seam that casts the
/// hot (ndjson VARCHAR) side to match parquet must cover ALL of these —
/// a second TIMESTAMP column left VARCHAR on the hot side trips the
/// union-conflict path on every query with a non-empty hot buffer.
pub const TIMESTAMP_COLUMNS: &[&str] = &[TIME, INGESTED];

/// Server-owned metadata a client may never set. A client-sent value is
/// dropped and replaced, recorded with the `meta.stripped` repair code —
/// silently honouring it would let a sender forge its own handling history.
/// (`_raw` is also server-owned when the client value is not a string.)
pub const RESERVED_CLIENT_FIELDS: &[&str] = &[INGESTED, REPAIRS];

/// Leading well-known columns for result reordering, in display order.
pub const LEADING_LOG_FIELDS: &[&str] =
    &[TIME, ENV, SERVICE, HOST, SEVERITY, SEVERITY_TEXT, MESSAGE];

/// Trailing columns demoted to the end of result reordering.
pub const TRAILING_LOG_FIELDS: &[&str] = &[RAW, INGESTED, REPAIRS];

/// Resolve a wire-format alias for the event-time input at ingest.
///
/// Clients may send `timestamp` or `@timestamp` (the DSL aliases them to
/// `_time` identically); none of the aliases are stored as columns — the
/// canonical value lands in `_time`.
pub fn is_time_alias(key: &str) -> bool {
    matches!(key, "timestamp" | "@timestamp" | "_time")
}

/// Wire keys consumed as the `_time` input, in precedence order.
pub const TIME_ALIASES: &[&str] = &[TIME, "timestamp", "@timestamp"];

/// The DSL-side alias resolution: `timestamp` and `@timestamp` resolve to
/// the physical `_time` column.
pub fn resolve_field_alias(name: &str) -> &str {
    match name {
        "timestamp" | "@timestamp" => TIME,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_resolve_to_time() {
        assert_eq!(resolve_field_alias("timestamp"), "_time");
        assert_eq!(resolve_field_alias("@timestamp"), "_time");
        assert_eq!(resolve_field_alias("_time"), "_time");
        assert_eq!(resolve_field_alias("host"), "host");
    }

    #[test]
    fn time_alias_detection() {
        assert!(is_time_alias("timestamp"));
        assert!(is_time_alias("@timestamp"));
        assert!(is_time_alias("_time"));
        assert!(!is_time_alias("time"));
        assert!(!is_time_alias("_ingested"));
    }

    #[test]
    fn timestamp_columns_cover_time_and_ingested() {
        assert_eq!(TIMESTAMP_COLUMNS, &[TIME, INGESTED]);
    }

    #[test]
    fn reserved_fields_are_server_owned() {
        assert!(RESERVED_CLIENT_FIELDS.contains(&INGESTED));
        assert!(RESERVED_CLIENT_FIELDS.contains(&REPAIRS));
        // _raw is conditionally honoured (string values kept), so not listed.
        assert!(!RESERVED_CLIENT_FIELDS.contains(&RAW));
    }

    #[test]
    fn leading_and_trailing_disjoint() {
        for f in LEADING_LOG_FIELDS {
            assert!(!TRAILING_LOG_FIELDS.contains(f));
        }
    }
}
