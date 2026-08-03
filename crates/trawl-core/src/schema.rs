// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The declared event envelope (ADR-0009): field names, reserved keys, and
//! wire aliases.
//!
//! Namespace rule: `_` marks metadata about the record's handling; no prefix
//! means data about the event. `severity` is server-derived but carries no
//! prefix because it is content.

use std::fmt;

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

/// Maximum length (bytes) of a field name trawl will store.
///
/// The field catalog keys on the name (`field_types.field` is a `TEXT`
/// PRIMARY KEY, `field_services` a `(field, service)` one), so a name that
/// cannot fit a postgres btree key cannot be pinned — and an unpinnable
/// column is a wedge, not a nuisance: pinning is a hard gate in front of
/// every parquet write. Postgres' limit is ~2704 bytes for the whole index
/// tuple; 255 keeps a full order of magnitude of headroom while being far
/// above any field name a log producer legitimately emits.
pub const MAX_FIELD_NAME_BYTES: usize = 255;

/// Whether a field name can be carried through the catalog (and therefore
/// stored at all). Bounded in BYTES because the constraint being respected
/// is postgres' byte-sized btree key limit, not a character count.
pub fn is_storable_field_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_FIELD_NAME_BYTES
}

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

// ---------------------------------------------------------------------------
// Field-catalog type vocabulary (ADR-0009 slice 2)
// ---------------------------------------------------------------------------
//
// The catalog pins every dynamic field to exactly one of five canonical
// storage types. This vocabulary is pure data — no I/O, no store coupling —
// and lives here because the SQL emitter needs the pins to conform the hot
// (JSON snapshot) side of the hot+cold union to the parquet side at emit
// time, with no read-time reconciliation left to do.

/// The five canonical storage types a field can be pinned to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CanonicalType {
    /// `BOOLEAN` on disk.
    Boolean,
    /// `BIGINT` on disk — every signed integer width collapses here.
    BigInt,
    /// `DOUBLE` on disk — floats and decimals collapse here.
    Double,
    /// `TIMESTAMP` on disk — timestamp variants and `DATE` collapse here.
    Timestamp,
    /// `VARCHAR` on disk — the honest fallback for everything else.
    Varchar,
}

impl CanonicalType {
    /// The exact `DuckDB` type spelling this canonical type is stored as
    /// (also the spelling persisted in the catalog's `duckdb_type` column).
    #[must_use]
    pub const fn as_duckdb(self) -> &'static str {
        match self {
            Self::Boolean => "BOOLEAN",
            Self::BigInt => "BIGINT",
            Self::Double => "DOUBLE",
            Self::Timestamp => "TIMESTAMP",
            Self::Varchar => "VARCHAR",
        }
    }

    /// Parse the catalog's stored spelling back into the enum. EXACT match
    /// only — the catalog is written by code, so any other spelling is
    /// corruption and must surface, not be guessed at.
    #[must_use]
    pub fn from_duckdb(s: &str) -> Option<Self> {
        match s {
            "BOOLEAN" => Some(Self::Boolean),
            "BIGINT" => Some(Self::BigInt),
            "DOUBLE" => Some(Self::Double),
            "TIMESTAMP" => Some(Self::Timestamp),
            "VARCHAR" => Some(Self::Varchar),
            _ => None,
        }
    }
}

impl fmt::Display for CanonicalType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_duckdb())
    }
}

/// How a `DuckDB`-inferred type normalizes into the canonical vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeResolution {
    /// Maps directly onto one canonical type.
    Pin(CanonicalType),
    /// Out-of-range or mixed inference — run the candidate [`LADDER`] over
    /// the batch's actual values.
    Ladder,
    /// `DuckDB` says `JSON`, which it infers both for an all-null column
    /// and for mixed scalar values — the caller must inspect the data:
    /// all-null defers the pin, mixed runs the [`LADDER`].
    Json,
}

/// The candidate ladder for pinning a field from a mixed or out-of-range
/// batch: `TRY_CAST` the batch's non-null values to each candidate in this
/// order; the first with a ≥90% success rate pins. None qualifies →
/// `VARCHAR`. Deterministic, first-wins, no ties.
pub const LADDER: [CanonicalType; 4] = [
    CanonicalType::BigInt,
    CanonicalType::Double,
    CanonicalType::Timestamp,
    CanonicalType::Boolean,
];

/// Minimum `TRY_CAST` success rate for a ladder candidate to pin.
pub const LADDER_SUCCESS_THRESHOLD: f64 = 0.9;

/// Normalize a `DuckDB` type name (as reported by `DESCRIBE`) into the
/// canonical lattice (ADR-0009 slice 2).
///
/// Complex kinds (`STRUCT`/`MAP`/`LIST`/`UNION`) cannot occur on the write
/// path once ingest stringifies nested values, so they normalize to
/// `VARCHAR` defensively rather than erroring. Unknown spellings likewise
/// fail safe to `VARCHAR` — an honest string beats a wrong number.
#[must_use]
pub fn normalize_duckdb_type(dtype: &str) -> TypeResolution {
    let t = dtype.trim().to_ascii_uppercase();
    match t.as_str() {
        "TINYINT" | "SMALLINT" | "INTEGER" | "BIGINT" | "INT" => {
            TypeResolution::Pin(CanonicalType::BigInt)
        }
        "UBIGINT" | "HUGEINT" | "UHUGEINT" | "UTINYINT" | "USMALLINT" | "UINTEGER" => {
            TypeResolution::Ladder
        }
        "FLOAT" | "DOUBLE" | "REAL" => TypeResolution::Pin(CanonicalType::Double),
        "BOOLEAN" => TypeResolution::Pin(CanonicalType::Boolean),
        "DATE" => TypeResolution::Pin(CanonicalType::Timestamp),
        "JSON" => TypeResolution::Json,
        _ if t.starts_with("DECIMAL") => TypeResolution::Pin(CanonicalType::Double),
        _ if t.starts_with("TIMESTAMP") => TypeResolution::Pin(CanonicalType::Timestamp),
        _ => TypeResolution::Pin(CanonicalType::Varchar),
    }
}

/// The declared envelope (ADR-0009) with its storage types — the catalog's
/// migration seed mirrors this list exactly (parity-tested server-side).
pub const ENVELOPE_TYPES: &[(&str, CanonicalType)] = &[
    (TIME, CanonicalType::Timestamp),
    (INGESTED, CanonicalType::Timestamp),
    (RAW, CanonicalType::Varchar),
    (REPAIRS, CanonicalType::Varchar),
    (ENV, CanonicalType::Varchar),
    (SERVICE, CanonicalType::Varchar),
    (HOST, CanonicalType::Varchar),
    (SEVERITY, CanonicalType::BigInt),
    (SEVERITY_TEXT, CanonicalType::Varchar),
    (MESSAGE, CanonicalType::Varchar),
];

/// An ordered field → canonical-type map, as threaded from the server's
/// pin cache into the emitter (pins ∩ hot-snapshot keys). Ordered so the
/// emitted SQL is deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldTypes {
    entries: std::collections::BTreeMap<String, CanonicalType>,
}

impl FieldTypes {
    /// An empty map (no pins — plain sources, embedded mode).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or overwrite a field's type.
    pub fn insert(&mut self, field: &str, ty: CanonicalType) {
        self.entries.insert(field.to_owned(), ty);
    }

    /// Iterate `(field, type)` in field-name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, CanonicalType)> {
        self.entries.iter().map(|(f, t)| (f.as_str(), *t))
    }

    /// Look up a field's pinned type.
    #[must_use]
    pub fn get(&self, field: &str) -> Option<CanonicalType> {
        self.entries.get(field).copied()
    }

    /// Whether the map holds no pins.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of pinned fields.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
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
    fn storable_field_names_are_bounded_in_bytes() {
        assert!(is_storable_field_name("duration"));
        assert!(is_storable_field_name(&"k".repeat(MAX_FIELD_NAME_BYTES)));
        assert!(!is_storable_field_name(
            &"k".repeat(MAX_FIELD_NAME_BYTES + 1)
        ));
        // Bytes, not chars: the limit being respected is postgres' btree
        // key size, and every declared envelope field clears it easily.
        assert!(!is_storable_field_name(&"é".repeat(MAX_FIELD_NAME_BYTES)));
        assert!(!is_storable_field_name(""));
        for (field, _) in ENVELOPE_TYPES {
            assert!(is_storable_field_name(field));
        }
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

    // --- the field-catalog type vocabulary (ADR-0009 slice 2) ---

    #[test]
    fn canonical_type_duckdb_spellings_round_trip() {
        for ty in [
            CanonicalType::Boolean,
            CanonicalType::BigInt,
            CanonicalType::Double,
            CanonicalType::Timestamp,
            CanonicalType::Varchar,
        ] {
            assert_eq!(CanonicalType::from_duckdb(ty.as_duckdb()), Some(ty));
        }
        assert_eq!(CanonicalType::BigInt.as_duckdb(), "BIGINT");
        assert_eq!(CanonicalType::from_duckdb("JSON"), None);
        assert_eq!(CanonicalType::from_duckdb("bigint"), None);
    }

    #[test]
    fn lattice_normalizes_every_duckdb_inference() {
        use TypeResolution::{Json, Ladder, Pin};
        // Signed integer widths collapse onto BIGINT.
        for t in ["TINYINT", "SMALLINT", "INTEGER", "BIGINT"] {
            assert_eq!(normalize_duckdb_type(t), Pin(CanonicalType::BigInt), "{t}");
        }
        // Out-of-range integer inferences go to the candidate ladder.
        for t in ["UBIGINT", "HUGEINT", "UHUGEINT"] {
            assert_eq!(normalize_duckdb_type(t), Ladder, "{t}");
        }
        // Floating / decimal collapse onto DOUBLE.
        for t in ["FLOAT", "DOUBLE", "DECIMAL(18,3)", "REAL"] {
            assert_eq!(normalize_duckdb_type(t), Pin(CanonicalType::Double), "{t}");
        }
        // Timestamp variants and DATE collapse onto TIMESTAMP.
        for t in [
            "TIMESTAMP",
            "TIMESTAMP WITH TIME ZONE",
            "TIMESTAMP_NS",
            "TIMESTAMP_MS",
            "TIMESTAMP_S",
            "DATE",
        ] {
            assert_eq!(
                normalize_duckdb_type(t),
                Pin(CanonicalType::Timestamp),
                "{t}"
            );
        }
        assert_eq!(
            normalize_duckdb_type("BOOLEAN"),
            Pin(CanonicalType::Boolean)
        );
        // A sender's deliberate string stays a string; TIME has no canonical
        // slot and degrades honestly.
        for t in ["VARCHAR", "TIME"] {
            assert_eq!(normalize_duckdb_type(t), Pin(CanonicalType::Varchar), "{t}");
        }
        // JSON needs data inspection (all-null → defer, mixed → ladder).
        assert_eq!(normalize_duckdb_type("JSON"), Json);
        // Complex kinds cannot occur post-stringification; defensive VARCHAR.
        for t in [
            "STRUCT(a BIGINT)",
            "MAP(VARCHAR, JSON)",
            "VARCHAR[]",
            "UNION(a BIGINT)",
        ] {
            assert_eq!(normalize_duckdb_type(t), Pin(CanonicalType::Varchar), "{t}");
        }
        // Unknown spellings fail safe to VARCHAR.
        assert_eq!(
            normalize_duckdb_type("INTERVAL"),
            Pin(CanonicalType::Varchar)
        );
    }

    #[test]
    fn ladder_order_is_fixed() {
        assert_eq!(
            LADDER,
            [
                CanonicalType::BigInt,
                CanonicalType::Double,
                CanonicalType::Timestamp,
                CanonicalType::Boolean,
            ]
        );
    }

    #[test]
    fn envelope_types_cover_the_declared_ten() {
        let fields: Vec<&str> = ENVELOPE_TYPES.iter().map(|(f, _)| *f).collect();
        assert_eq!(
            fields,
            vec![
                TIME,
                INGESTED,
                RAW,
                REPAIRS,
                ENV,
                SERVICE,
                HOST,
                SEVERITY,
                SEVERITY_TEXT,
                MESSAGE
            ]
        );
        let ty = |name: &str| {
            ENVELOPE_TYPES
                .iter()
                .find(|(f, _)| *f == name)
                .map(|(_, t)| *t)
                .unwrap()
        };
        assert_eq!(ty(TIME), CanonicalType::Timestamp);
        assert_eq!(ty(INGESTED), CanonicalType::Timestamp);
        assert_eq!(ty(SEVERITY), CanonicalType::BigInt);
        for f in [RAW, REPAIRS, ENV, SERVICE, HOST, SEVERITY_TEXT, MESSAGE] {
            assert_eq!(ty(f), CanonicalType::Varchar, "{f}");
        }
    }

    #[test]
    fn field_types_is_ordered_and_deduplicated() {
        let mut ft = FieldTypes::new();
        ft.insert("zeta", CanonicalType::BigInt);
        ft.insert("alpha", CanonicalType::Varchar);
        ft.insert("zeta", CanonicalType::Double); // last write wins
        let entries: Vec<(&str, CanonicalType)> = ft.iter().collect();
        assert_eq!(
            entries,
            vec![
                ("alpha", CanonicalType::Varchar),
                ("zeta", CanonicalType::Double)
            ]
        );
        assert!(!ft.is_empty());
        assert!(FieldTypes::new().is_empty());
    }
}
