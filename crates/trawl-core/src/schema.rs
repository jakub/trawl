// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The declared event envelope (ADR-0013): field names and the sealed
//! namespace predicate.

use std::fmt;

pub const TIME: &str = "_time";
pub const INGESTED: &str = "_ingested";
pub const RAW: &str = "_raw";
pub const REPAIRS: &str = "_repairs";
pub const ENV: &str = "env";
pub const SERVICE: &str = "service";
pub const HOST: &str = "host";
pub const SEVERITY: &str = "_severity";
pub const MESSAGE: &str = "message";
pub const PRODUCER: &str = "_producer";
/// The synthetic column compaction reads each WAL row's source file into
/// (`read_json(..., filename='_trawl_wal_file')`). Internal provenance, not
/// an envelope column: it never reaches parquet.
pub const WAL_FILE: &str = "_trawl_wal_file";

pub const STAMP_NAMES: &[&str] = &[INGESTED, REPAIRS, PRODUCER, WAL_FILE];

pub const DERIVATION_ONLY_NAMES: &[&str] = &[SEVERITY];

/// Envelope columns stored as TIMESTAMP on disk. Every seam that casts the
/// hot (ndjson VARCHAR) side to match parquet must cover all of these: a
/// second TIMESTAMP column left VARCHAR on the hot side trips the
/// union-conflict path on every query with a non-empty hot buffer.
pub const TIMESTAMP_COLUMNS: &[&str] = &[TIME, INGESTED];

pub const NON_DIMENSION_FIELDS: &[&str] = &[TIME, INGESTED, RAW];

pub const MAX_FIELD_NAME_BYTES: usize = 255;

/// Bounded in BYTES because the constraint being respected is postgres'
/// byte-sized btree key limit, not a character count.
pub fn is_storable_field_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_FIELD_NAME_BYTES
}

pub const LEADING_LOG_FIELDS: &[&str] = &[TIME, ENV, SERVICE, HOST, SEVERITY, MESSAGE];

pub const TRAILING_LOG_FIELDS: &[&str] = &[RAW, INGESTED, REPAIRS, PRODUCER];

/// Whether a name belongs to trawl's contract namespace (ADR-0013 §1).
#[must_use]
pub fn is_reserved_name(name: &str) -> bool {
    name.starts_with('_')
}

const SENDER_ASSERTED_ENVELOPE: &[&str] = &[ENV, SERVICE, HOST, MESSAGE];

#[must_use]
pub fn is_contract_typed(name: &str) -> bool {
    is_reserved_name(name) || SENDER_ASSERTED_ENVELOPE.contains(&name)
}

#[must_use]
pub fn reserved_name_message(what: &str, name: &str) -> String {
    format!(
        "{what} '{name}' is in trawl's reserved namespace — names \
         starting with '_' are trawl's contract slots and only trawl \
         writes them (ADR-0013); choose a name without the underscore"
    )
}

#[must_use]
pub fn duplicate_target_message<'a>(
    targets: impl Iterator<Item = &'a str>,
    what: &str,
) -> Option<String> {
    let mut seen: Vec<(String, &str)> = Vec::new();
    for name in targets {
        let folded = catalog_key(name);
        if let Some((_, first)) = seen.iter().find(|(key, _)| *key == folded) {
            let both = if *first == name {
                format!("`{name}` twice")
            } else {
                format!("`{first}` and `{name}`, which name one column")
            };
            return Some(format!(
                "{what} writes {both} — give each target a name of its own"
            ));
        }
        seen.push((folded, name));
    }
    None
}

/// The catalog spelling of a DSL field reference: an ASCII fold, and
/// nothing else (ADR-0013 §6 — the DSL has zero aliases).
#[must_use]
pub fn catalog_key(dsl_name: &str) -> String {
    dsl_name.to_ascii_lowercase()
}

#[must_use]
pub fn is_event_time(dsl_name: &str) -> bool {
    catalog_key(dsl_name) == TIME
}

#[must_use]
pub fn is_non_dimension(name: &str) -> bool {
    let key = catalog_key(name);
    NON_DIMENSION_FIELDS.contains(&key.as_str())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CanonicalType {
    Boolean,
    BigInt,
    Double,
    Timestamp,
    Varchar,
    Severity,
}

impl CanonicalType {
    /// Stable Rust cannot count a plain enum's variants, so nothing
    /// mechanically proves a new variant was added here. `index` below is a
    /// test-only exhaustive match: the pre-commit hook runs clippy with
    /// `--all-targets`, so that build stops with the author at this list.
    pub const ALL: [Self; 6] = [
        Self::Boolean,
        Self::BigInt,
        Self::Double,
        Self::Timestamp,
        Self::Varchar,
        Self::Severity,
    ];

    #[cfg(test)]
    const fn index(self) -> usize {
        match self {
            Self::Boolean => 0,
            Self::BigInt => 1,
            Self::Double => 2,
            Self::Timestamp => 3,
            Self::Varchar => 4,
            Self::Severity => 5,
        }
    }

    #[must_use]
    pub const fn as_duckdb(self) -> &'static str {
        match self {
            Self::Boolean => "BOOLEAN",
            Self::BigInt | Self::Severity => "BIGINT",
            Self::Double => "DOUBLE",
            Self::Timestamp => "TIMESTAMP",
            Self::Varchar => "VARCHAR",
        }
    }

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

    #[must_use]
    pub const fn as_catalog(self) -> &'static str {
        match self {
            Self::Severity => "SEVERITY",
            other => other.as_duckdb(),
        }
    }

    /// Exact match only — the catalog is written by code, so any other
    /// spelling is corruption and must surface, not be guessed at.
    #[must_use]
    pub fn from_catalog(s: &str) -> Option<Self> {
        match s {
            "SEVERITY" => Some(Self::Severity),
            other => Self::from_duckdb(other),
        }
    }
}

impl fmt::Display for CanonicalType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_catalog())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeResolution {
    Pin(CanonicalType),
    Ladder,
    /// `DuckDB` says `JSON`, which it infers both for an all-null column
    /// and for mixed scalar values — the caller must inspect the data:
    /// all-null defers the pin, mixed runs the [`LADDER`].
    Json,
}

pub const LADDER: [CanonicalType; 4] = [
    CanonicalType::BigInt,
    CanonicalType::Double,
    CanonicalType::Timestamp,
    CanonicalType::Boolean,
];

pub const LADDER_SUCCESS_THRESHOLD: f64 = 0.9;

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
    (SEVERITY, CanonicalType::Severity),
    (ENV, CanonicalType::Varchar),
    (SERVICE, CanonicalType::Varchar),
    (HOST, CanonicalType::Varchar),
    (MESSAGE, CanonicalType::Varchar),
    (PRODUCER, CanonicalType::Varchar),
];

/// Shared behind an `Arc` and written copy-on-write: the emitter takes one
/// per pass, one logical query runs up to four passes, and with the catalog
/// bounded at `MAX_PINNED_FIELDS` (10 000 install-wide) a deep copy per
/// clone is a real per-query cost.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldTypes {
    entries: std::sync::Arc<std::collections::BTreeMap<String, CanonicalType>>,
}

impl FieldTypes {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, field: &str, ty: CanonicalType) {
        std::sync::Arc::make_mut(&mut self.entries).insert(field.to_owned(), ty);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, CanonicalType)> {
        self.entries.iter().map(|(f, t)| (f.as_str(), *t))
    }

    /// Look up a field's pinned type by exact catalog key; a DSL field
    /// reference goes through [`Self::pin_for`] to be folded first.
    #[must_use]
    pub fn get(&self, field: &str) -> Option<CanonicalType> {
        self.entries.get(field).copied()
    }

    #[must_use]
    pub fn pin_for(&self, dsl_name: &str) -> Option<CanonicalType> {
        self.get(&catalog_key(dsl_name))
    }

    pub fn remove(&mut self, field: &str) {
        if self.entries.contains_key(field) {
            std::sync::Arc::make_mut(&mut self.entries).remove(field);
        }
    }

    pub fn restrict_to(&mut self, keep: &[String]) {
        if self.entries.keys().all(|k| keep.contains(k)) {
            return;
        }
        std::sync::Arc::make_mut(&mut self.entries).retain(|k, _| keep.contains(k));
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_underscore_prefix_is_sealed_as_a_predicate() {
        for name in [
            TIME,
            INGESTED,
            RAW,
            REPAIRS,
            "_severity",
            "_anything",
            "__name__",
            "_",
        ] {
            assert!(is_reserved_name(name), "{name}");
        }
        for name in [
            ENV,
            SERVICE,
            HOST,
            MESSAGE,
            "level",
            "timestamp",
            "severity",
            "a_b",
        ] {
            assert!(!is_reserved_name(name), "{name}");
        }
    }

    #[test]
    fn every_envelope_field_is_contract_typed() {
        for (field, _) in ENVELOPE_TYPES {
            assert!(is_contract_typed(field), "{field} escaped the predicate");
        }
        for name in SENDER_ASSERTED_ENVELOPE {
            assert!(!is_reserved_name(name), "{name} is bare by design");
            assert!(
                ENVELOPE_TYPES.iter().any(|(f, _)| f == name),
                "{name} must be a declared envelope field"
            );
        }
        for name in [
            "level",
            "severity",
            "timestamp",
            "status",
            "duration",
            "env2",
        ] {
            assert!(!is_contract_typed(name), "{name}");
        }
    }

    #[test]
    fn storable_field_names_are_bounded_in_bytes() {
        assert!(is_storable_field_name("duration"));
        assert!(is_storable_field_name(&"k".repeat(MAX_FIELD_NAME_BYTES)));
        assert!(!is_storable_field_name(
            &"k".repeat(MAX_FIELD_NAME_BYTES + 1)
        ));
        assert!(!is_storable_field_name(&"é".repeat(MAX_FIELD_NAME_BYTES)));
        assert!(!is_storable_field_name(""));
        for (field, _) in ENVELOPE_TYPES {
            assert!(is_storable_field_name(field));
        }
    }

    #[test]
    fn timestamp_columns_cover_time_and_ingested() {
        assert_eq!(TIMESTAMP_COLUMNS, &[TIME, INGESTED]);
    }

    #[test]
    fn leading_and_trailing_disjoint() {
        for f in LEADING_LOG_FIELDS {
            assert!(!TRAILING_LOG_FIELDS.contains(f));
        }
    }

    #[test]
    fn canonical_type_catalog_spellings_round_trip() {
        for ty in CanonicalType::ALL {
            assert_eq!(CanonicalType::from_catalog(ty.as_catalog()), Some(ty));
        }
        assert_eq!(CanonicalType::BigInt.as_catalog(), "BIGINT");
        assert_eq!(CanonicalType::Severity.as_catalog(), "SEVERITY");
        assert_eq!(CanonicalType::from_catalog("JSON"), None);
        assert_eq!(CanonicalType::from_catalog("bigint"), None);
    }

    #[test]
    fn canonical_type_all_is_ordered_and_duplicate_free() {
        assert_eq!(
            CanonicalType::ALL.len(),
            6,
            "an entry left CanonicalType::ALL; every entry also needs a \
             PREPARE fixture case, which is what the engine's coverage \
             assertion checks"
        );
        for (slot, ty) in CanonicalType::ALL.into_iter().enumerate() {
            assert_eq!(ty.index(), slot, "{ty:?} sits in the wrong ALL slot");
        }
        let unique: std::collections::BTreeSet<CanonicalType> =
            CanonicalType::ALL.into_iter().collect();
        assert_eq!(
            unique.len(),
            CanonicalType::ALL.len(),
            "duplicate entry in CanonicalType::ALL would bias the fuzz pin \
             selector and skew the trawl-engine fixture's coverage"
        );
    }

    #[test]
    fn severity_is_physically_bigint_and_unreachable_by_physical_parse() {
        assert_eq!(CanonicalType::Severity.as_duckdb(), "BIGINT");
        assert_eq!(
            CanonicalType::from_duckdb("BIGINT"),
            Some(CanonicalType::BigInt)
        );
        assert_eq!(CanonicalType::from_duckdb("SEVERITY"), None);
    }

    #[test]
    fn inference_can_never_pin_severity() {
        for t in [
            "BIGINT",
            "SEVERITY",
            "severity",
            "INTEGER",
            "VARCHAR",
            "DOUBLE",
            "BOOLEAN",
            "TIMESTAMP",
            "JSON",
            "HUGEINT",
        ] {
            assert_ne!(
                normalize_duckdb_type(t),
                TypeResolution::Pin(CanonicalType::Severity),
                "{t}"
            );
        }
    }

    #[test]
    fn lattice_normalizes_every_duckdb_inference() {
        use TypeResolution::{Json, Ladder, Pin};
        for t in ["TINYINT", "SMALLINT", "INTEGER", "BIGINT"] {
            assert_eq!(normalize_duckdb_type(t), Pin(CanonicalType::BigInt), "{t}");
        }
        for t in ["UBIGINT", "HUGEINT", "UHUGEINT"] {
            assert_eq!(normalize_duckdb_type(t), Ladder, "{t}");
        }
        for t in ["FLOAT", "DOUBLE", "DECIMAL(18,3)", "REAL"] {
            assert_eq!(normalize_duckdb_type(t), Pin(CanonicalType::Double), "{t}");
        }
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
        for t in ["VARCHAR", "TIME"] {
            assert_eq!(normalize_duckdb_type(t), Pin(CanonicalType::Varchar), "{t}");
        }
        assert_eq!(normalize_duckdb_type("JSON"), Json);
        for t in [
            "STRUCT(a BIGINT)",
            "MAP(VARCHAR, JSON)",
            "VARCHAR[]",
            "UNION(a BIGINT)",
        ] {
            assert_eq!(normalize_duckdb_type(t), Pin(CanonicalType::Varchar), "{t}");
        }
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
                TIME, INGESTED, RAW, REPAIRS, SEVERITY, ENV, SERVICE, HOST, MESSAGE, PRODUCER
            ]
        );
        assert_eq!(SEVERITY, "_severity");
        assert_eq!(PRODUCER, "_producer");
        assert!(!fields.contains(&"severity"));
        assert!(!fields.contains(&"severity_text"));
        for f in [TIME, INGESTED, RAW, REPAIRS, SEVERITY, PRODUCER] {
            assert!(is_reserved_name(f), "{f} must be reserved");
        }
        for f in [ENV, SERVICE, HOST, MESSAGE] {
            assert!(!is_reserved_name(f), "{f} is sender-asserted");
        }

        let ty = |name: &str| {
            ENVELOPE_TYPES
                .iter()
                .find(|(f, _)| *f == name)
                .map(|(_, t)| *t)
                .unwrap()
        };
        assert_eq!(ty(TIME), CanonicalType::Timestamp);
        assert_eq!(ty(INGESTED), CanonicalType::Timestamp);
        assert_eq!(ty(SEVERITY), CanonicalType::Severity);
        for f in [RAW, REPAIRS, ENV, SERVICE, HOST, MESSAGE, PRODUCER] {
            assert_eq!(ty(f), CanonicalType::Varchar, "{f}");
        }
    }

    #[test]
    fn non_dimension_fields_are_the_instants_and_the_raw_event() {
        assert_eq!(NON_DIMENSION_FIELDS, &[TIME, INGESTED, RAW]);
        for name in NON_DIMENSION_FIELDS {
            assert!(is_reserved_name(name), "{name}");
            assert!(is_non_dimension(name), "{name}");
            assert!(is_non_dimension(&name.to_ascii_uppercase()), "{name}");
        }
        assert!(is_non_dimension("_Time"));
        for name in [
            SEVERITY,
            REPAIRS,
            PRODUCER,
            "timestamp",
            "time",
            "raw",
            "ingested",
        ] {
            assert!(!is_non_dimension(name), "{name}");
        }
    }

    #[test]
    fn catalog_key_only_folds_ascii() {
        assert_eq!(catalog_key("timestamp"), "timestamp");
        assert_eq!(catalog_key("@timestamp"), "@timestamp");
        assert_eq!(catalog_key("Status"), "status");
        assert_eq!(catalog_key("DUR"), "dur");
        assert_eq!(catalog_key("CAFÉ"), "cafÉ");
        assert_eq!(catalog_key("host"), "host");
    }

    #[test]
    fn pin_for_looks_up_through_the_catalog_key() {
        let mut ft = FieldTypes::new();
        ft.insert("status", CanonicalType::Varchar);
        ft.insert("_time", CanonicalType::Timestamp);
        assert_eq!(ft.pin_for("status"), Some(CanonicalType::Varchar));
        assert_eq!(ft.pin_for("Status"), Some(CanonicalType::Varchar));
        assert_eq!(ft.pin_for("_time"), Some(CanonicalType::Timestamp));
        assert_eq!(ft.pin_for("_TIME"), Some(CanonicalType::Timestamp));
        assert_eq!(ft.pin_for("timestamp"), None);
        assert_eq!(ft.pin_for("unpinned"), None);
    }

    #[test]
    fn field_types_is_ordered_and_deduplicated() {
        let mut ft = FieldTypes::new();
        ft.insert("zeta", CanonicalType::BigInt);
        ft.insert("alpha", CanonicalType::Varchar);
        ft.insert("zeta", CanonicalType::Double);
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

    #[test]
    fn remove_and_restrict_edit_copy_on_write_with_noop_fast_paths() {
        let mut ft = FieldTypes::new();
        ft.insert("status", CanonicalType::Varchar);
        ft.insert("dur", CanonicalType::BigInt);
        let shared = ft.clone();

        ft.remove("absent");
        ft.restrict_to(&["status".into(), "dur".into(), "extra".into()]);
        assert!(std::sync::Arc::ptr_eq(&ft.entries, &shared.entries));

        ft.remove("status");
        assert_eq!(ft.get("status"), None);
        assert_eq!(shared.get("status"), Some(CanonicalType::Varchar));

        let mut ft2 = shared.clone();
        ft2.restrict_to(&["dur".into()]);
        assert_eq!(ft2.get("status"), None);
        assert_eq!(ft2.get("dur"), Some(CanonicalType::BigInt));
        assert_eq!(shared.get("status"), Some(CanonicalType::Varchar));
    }

    #[test]
    fn cloning_shares_the_map_and_insert_copies_on_write() {
        let mut ft = FieldTypes::new();
        ft.insert("status", CanonicalType::Varchar);
        let shared = ft.clone();
        assert!(std::sync::Arc::ptr_eq(&ft.entries, &shared.entries));

        ft.insert("duration", CanonicalType::BigInt);
        assert_eq!(ft.get("duration"), Some(CanonicalType::BigInt));
        assert_eq!(shared.get("duration"), None);
        assert_eq!(shared.get("status"), Some(CanonicalType::Varchar));
    }
}
