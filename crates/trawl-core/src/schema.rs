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

/// The catalog spelling of a DSL field reference: resolve the time aliases,
/// then ASCII-lowercase (ADR-0011 slice A).
///
/// Catalog names are ASCII-folded at every producer's door (ingest, boot
/// seeding, compaction proposals), and `DuckDB` folds identifiers over
/// ASCII too — so `Status` in a query names the same column, and the same
/// pin, as `status`. Without this fold a mixed-case reference would
/// silently fall back to unpinned while the lowercase spelling is
/// pin-aware. Non-ASCII stays put, mirroring `DuckDB`'s ASCII-only
/// identifier folding.
#[must_use]
pub fn catalog_key(dsl_name: &str) -> String {
    resolve_field_alias(dsl_name).to_ascii_lowercase()
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

/// The canonical storage types a field can be pinned to.
///
/// Five are physical types; [`Self::Severity`] is a SEMANTIC type over the
/// physical `BIGINT` (ADR-0013): the pin is what gives `_severity` its
/// token vocabulary in the ADR-0011 comparison rule table, and it is
/// reachable only from the declared envelope seed — `DESCRIBE` never says
/// `SEVERITY`, so inference cannot mint it.
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
    /// The `OTel` `SeverityNumber` ladder, `BIGINT` on disk and bounded to
    /// 1-24 by its conform rung (ADR-0013). Only `_severity` carries it.
    Severity,
}

impl CanonicalType {
    /// The PHYSICAL `DuckDB` type spelling — what a cast, a `DESCRIBE`
    /// comparison, a repin rewrite and the hot branch's `REPLACE` all
    /// name. NOT injective: `SEVERITY` is a `BIGINT` on disk.
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

    /// Parse a PHYSICAL spelling back into the enum. EXACT match only.
    ///
    /// `BIGINT` resolves to [`Self::BigInt`] and nothing resolves to
    /// [`Self::Severity`] — the semantic pin has no physical spelling of
    /// its own, which is exactly what keeps `repin --to severity` (slice
    /// 2) out of the operator surface for now.
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

    /// The CATALOG spelling: what postgres stores in `field_types` and
    /// what every schema wire surface carries. Injective, so a pin can be
    /// read back exactly as it was written.
    #[must_use]
    pub const fn as_catalog(self) -> &'static str {
        match self {
            Self::Severity => "SEVERITY",
            other => other.as_duckdb(),
        }
    }

    /// Parse the catalog's stored spelling back into the enum. EXACT match
    /// only — the catalog is written by code, so any other spelling is
    /// corruption and must surface, not be guessed at.
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
///
/// The map is shared behind an `Arc` and written copy-on-write: a pin set
/// is built once and then read many times — the emitter carries one per
/// pass, and a single logical query can re-emit up to three times on the
/// pruned-retry / hot-only-fallback ladder. With the catalog bounded at
/// `MAX_PINNED_FIELDS` (10 000 install-wide) a deep copy per clone is a
/// real per-query cost; `clone` here is a refcount bump instead. Mutation
/// stays available (`insert` through [`std::sync::Arc::make_mut`]) and
/// unshared instances — the build-then-share path — never copy at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldTypes {
    entries: std::sync::Arc<std::collections::BTreeMap<String, CanonicalType>>,
}

impl FieldTypes {
    /// An empty map (no pins — plain sources, embedded mode).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or overwrite a field's type.
    ///
    /// Copy-on-write: copies the map only when another clone is still
    /// holding it, which the build-then-share callers never do.
    pub fn insert(&mut self, field: &str, ty: CanonicalType) {
        std::sync::Arc::make_mut(&mut self.entries).insert(field.to_owned(), ty);
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

    /// Look up the pin for a DSL field reference, through [`catalog_key`]
    /// (alias resolution + ASCII fold). The emitter's and the in-memory
    /// filter's shared pin lookup — both must agree on which pin a query
    /// token names (ADR-0011 slice A).
    #[must_use]
    pub fn pin_for(&self, dsl_name: &str) -> Option<CanonicalType> {
        self.get(&catalog_key(dsl_name))
    }

    /// Remove a field's pin, if present.
    ///
    /// No-op fast path when the name isn't pinned, so an unshared map is
    /// never copied for nothing — the pin-scope walk (`crate::pin_scope`)
    /// removes client-chosen names that mostly aren't in the catalog.
    pub fn remove(&mut self, field: &str) {
        if self.entries.contains_key(field) {
            std::sync::Arc::make_mut(&mut self.entries).remove(field);
        }
    }

    /// Restrict the map to the given field names, dropping every other
    /// pin. Same fast path as [`Self::remove`]: when nothing would be
    /// dropped, the shared map is left untouched.
    pub fn restrict_to(&mut self, keep: &[String]) {
        if self.entries.keys().all(|k| keep.contains(k)) {
            return;
        }
        std::sync::Arc::make_mut(&mut self.entries).retain(|k, _| keep.contains(k));
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

    /// The CATALOG spelling is the injective one — it is what postgres
    /// stores and what the wire carries, so it must round-trip for every
    /// canonical type, `SEVERITY` included.
    #[test]
    fn canonical_type_catalog_spellings_round_trip() {
        for ty in [
            CanonicalType::Boolean,
            CanonicalType::BigInt,
            CanonicalType::Double,
            CanonicalType::Timestamp,
            CanonicalType::Varchar,
            CanonicalType::Severity,
        ] {
            assert_eq!(CanonicalType::from_catalog(ty.as_catalog()), Some(ty));
        }
        assert_eq!(CanonicalType::BigInt.as_catalog(), "BIGINT");
        assert_eq!(CanonicalType::Severity.as_catalog(), "SEVERITY");
        assert_eq!(CanonicalType::from_catalog("JSON"), None);
        assert_eq!(CanonicalType::from_catalog("bigint"), None);
    }

    /// The PHYSICAL spelling is what casts and DDL use, and it is
    /// deliberately NOT injective: `SEVERITY` is a BIGINT on disk, so
    /// `from_duckdb` — the inverse of the physical spelling — cannot
    /// name it. That is the structural reason `repin --to severity` is a
    /// slice-2 feature rather than a live foot-gun.
    #[test]
    fn severity_is_physically_bigint_and_unreachable_by_physical_parse() {
        assert_eq!(CanonicalType::Severity.as_duckdb(), "BIGINT");
        assert_eq!(
            CanonicalType::from_duckdb("BIGINT"),
            Some(CanonicalType::BigInt)
        );
        assert_eq!(CanonicalType::from_duckdb("SEVERITY"), None);
    }

    /// `DESCRIBE` never reports SEVERITY, so inference can never pin it —
    /// only the declared seed can.
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
    fn catalog_key_resolves_aliases_then_folds_ascii() {
        // Alias resolution first: the pinned column is `_time`, whatever
        // spelling the DSL used.
        assert_eq!(catalog_key("timestamp"), "_time");
        assert_eq!(catalog_key("@timestamp"), "_time");
        // ASCII fold second: catalog names are ingest-folded lowercase, so
        // `Status` must find the `status` pin instead of silently falling
        // back to unpinned.
        assert_eq!(catalog_key("Status"), "status");
        assert_eq!(catalog_key("DUR"), "dur");
        // Non-ASCII stays put — DuckDB folds identifiers over ASCII only.
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
        assert_eq!(ft.pin_for("timestamp"), Some(CanonicalType::Timestamp));
        assert_eq!(ft.pin_for("@timestamp"), Some(CanonicalType::Timestamp));
        assert_eq!(ft.pin_for("unpinned"), None);
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

    #[test]
    fn remove_and_restrict_edit_copy_on_write_with_noop_fast_paths() {
        let mut ft = FieldTypes::new();
        ft.insert("status", CanonicalType::Varchar);
        ft.insert("dur", CanonicalType::BigInt);
        let shared = ft.clone();

        // No-op paths leave the shared Arc untouched.
        ft.remove("absent");
        ft.restrict_to(&["status".into(), "dur".into(), "extra".into()]);
        assert!(std::sync::Arc::ptr_eq(&ft.entries, &shared.entries));

        // Real removals copy and don't reach the other handle.
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
        // The emitter clones a pin set per pass (up to three passes per
        // logical query); that must not deep-copy the catalog.
        assert!(std::sync::Arc::ptr_eq(&ft.entries, &shared.entries));

        // ... and a write through one handle must not reach the other.
        ft.insert("duration", CanonicalType::BigInt);
        assert_eq!(ft.get("duration"), Some(CanonicalType::BigInt));
        assert_eq!(shared.get("duration"), None);
        assert_eq!(shared.get("status"), Some(CanonicalType::Varchar));
    }
}
