// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! In-process field-catalog machinery (ADR-0009 slice 2): the pin cache
//! that keeps the query path free of postgres I/O, and the context handle
//! compaction uses to pin and conform.
//!
//! The postgres tables live in [`crate::store::catalog`]; this module is
//! the process-local view. Pins are add-only until the repin machinery
//! (#53), so the cache never needs invalidation — boot hydrates it and
//! every `pin_missing` refresh only ever adds entries.

pub mod conform;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::RwLock;
use trawl_core::schema::{CanonicalType, FieldTypes};

use crate::store::CatalogStore;

/// Process-local pin cache. Cheap to share (`Arc`), lock-light reads.
#[derive(Debug, Default)]
pub struct FieldCatalog {
    pins: RwLock<HashMap<String, CanonicalType>>,
}

impl FieldCatalog {
    /// An empty cache (hydrated at boot from [`CatalogStore::load_pins`]).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the whole cache with an authoritative pin set (boot only —
    /// hydration is the one place that has read the whole catalog).
    pub fn replace(&self, pins: impl IntoIterator<Item = (String, CanonicalType)>) {
        *self.pins.write() = pins.into_iter().collect();
    }

    /// Fold newly-durable pins into the cache, leaving every other entry
    /// alone.
    ///
    /// This — not [`Self::replace`] — is the steady-state update. Pins are
    /// add-only until the repin machinery (#53), so a delta merge lands the
    /// same map a full reload would, without re-reading a catalog whose size
    /// is bounded only by how many distinct field names clients have ever
    /// sent. Compaction runs this once per batch that actually pinned
    /// something; a batch proposing nothing touches neither postgres nor
    /// this lock.
    pub fn merge(&self, pins: impl IntoIterator<Item = (String, CanonicalType)>) {
        let mut guard = self.pins.write();
        for (field, ty) in pins {
            guard.insert(field, ty);
        }
    }

    /// Look up one field's pin.
    #[must_use]
    pub fn get(&self, field: &str) -> Option<CanonicalType> {
        self.pins.read().get(field).copied()
    }

    /// A snapshot of the current pins.
    #[must_use]
    pub fn snapshot(&self) -> HashMap<String, CanonicalType> {
        self.pins.read().clone()
    }

    /// The pins intersected with a hot snapshot's key set — the
    /// [`FieldTypes`] the emitter conforms the hot side of the union with.
    /// Zero postgres I/O: this is the whole point of the cache.
    ///
    /// A key whose spelling collides case-insensitively with another key in
    /// the SAME key set is pinned `VARCHAR` whatever the catalog says.
    /// Catalog names are case-SENSITIVE (client JSON keys, never
    /// case-normalised) while `DuckDB` identifiers are case-INSENSITIVE, so
    /// `duration` and `Duration` name ONE column and a `REPLACE` naming
    /// either spelling binds whichever column `DuckDB` bound first (probed by
    /// execution in `trawl-engine/tests/duckdb_probe.rs`) — a TYPED pin there
    /// would `TRY_CAST` the other spelling's values to NULL. `VARCHAR` is the
    /// lossless conform (the emitter's expression stringifies every inference
    /// class) and is the one conform that must not be skipped: an unpinned
    /// hot column keeps `read_json`'s inferred type, and a single
    /// union-incompatible pair against its cold counterpart (cold `VARCHAR` ×
    /// hot JSON, cold `TIMESTAMP` × hot `BIGINT`) throws the whole composite
    /// source — failing EVERY query while those events sit in the buffer, not
    /// just queries naming the field. A `VARCHAR` hot column, by contrast,
    /// unions with every cold scalar type (both probed in
    /// `trawl-engine/tests/duckdb_probe.rs`).
    ///
    /// This is the last line of defence, not the policy: the snapshot writer
    /// merges case-variant keys into one spelling before they reach here
    /// ([`crate::hot_buffer::HotBuffer::snapshot`]), so a collided key set is
    /// only what a caller building its own key set can still hand over.
    #[must_use]
    pub fn intersect<'a>(&self, keys: impl IntoIterator<Item = &'a str>) -> FieldTypes {
        let keys: Vec<&str> = keys.into_iter().collect();
        let collided = ascii_case_collisions(keys.iter().copied());
        let pins = self.pins.read();
        let mut out = FieldTypes::new();
        for key in keys {
            if collided.contains(&key.to_ascii_uppercase()) {
                out.insert(key, CanonicalType::Varchar);
            } else if let Some(ty) = pins.get(key) {
                out.insert(key, *ty);
            }
        }
        out
    }
}

/// The ASCII-uppercased spellings that appear more than once in `keys` —
/// i.e. the names `DuckDB` cannot tell apart.
///
/// ASCII-only, matching `DuckDB`'s own identifier comparison: `café` and
/// `CAFÉ` stay distinct columns, so treating them as a collision would drop
/// real pins (probed in `trawl-engine/tests/duckdb_probe.rs`).
#[must_use]
pub fn ascii_case_collisions<'a>(keys: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut collided = HashSet::new();
    for key in keys {
        let folded = key.to_ascii_uppercase();
        if !seen.insert(folded.clone()) {
            collided.insert(folded);
        }
    }
    collided
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(pins: &[(&str, CanonicalType)]) -> FieldCatalog {
        let cache = FieldCatalog::new();
        cache.replace(pins.iter().map(|(f, t)| ((*f).to_owned(), *t)));
        cache
    }

    #[test]
    fn intersect_keeps_pins_for_observed_keys_only() {
        let cache = catalog(&[
            ("duration", CanonicalType::BigInt),
            ("absent", CanonicalType::Varchar),
        ]);
        let pins = cache.intersect(["duration", "unpinned"]);
        assert_eq!(pins.get("duration"), Some(CanonicalType::BigInt));
        assert_eq!(pins.get("absent"), None);
        assert_eq!(pins.len(), 1);
    }

    #[test]
    fn merge_adds_deltas_without_dropping_existing_pins() {
        // The steady-state compaction update: only the batch's own new pins
        // are known, and folding them in must not evict the rest of the
        // catalog the way `replace` would.
        let cache = catalog(&[("duration", CanonicalType::BigInt)]);
        cache.merge([("status".to_owned(), CanonicalType::BigInt)]);
        assert_eq!(cache.get("duration"), Some(CanonicalType::BigInt));
        assert_eq!(cache.get("status"), Some(CanonicalType::BigInt));
        assert_eq!(cache.snapshot().len(), 2);
    }

    #[test]
    fn intersect_degrades_case_collided_keys_to_varchar() {
        // Both spellings in one key set: neither typed pin can name its own
        // column, so both degrade to the lossless VARCHAR conform. Dropping
        // them instead would leave the column at read_json's inferred type,
        // where one union-incompatible pair fails EVERY query.
        let cache = catalog(&[
            ("duration", CanonicalType::BigInt),
            ("Duration", CanonicalType::Varchar),
            ("host", CanonicalType::Varchar),
        ]);
        let pins = cache.intersect(["duration", "Duration", "host"]);
        assert_eq!(pins.get("duration"), Some(CanonicalType::Varchar));
        assert_eq!(pins.get("Duration"), Some(CanonicalType::Varchar));
        assert_eq!(pins.get("host"), Some(CanonicalType::Varchar));
    }

    #[test]
    fn intersect_degrades_an_unpinned_collided_spelling_too() {
        // The collision does not need two pins: one pinned spelling plus a
        // brand-new event carrying the other spelling is enough to make the
        // pin bind the wrong column — and the unpinned spelling is exactly
        // the one whose inferred type can throw the union, so it must be
        // conformed even though the catalog says nothing about it.
        let cache = catalog(&[("Duration", CanonicalType::Varchar)]);
        let pins = cache.intersect(["Duration", "duration"]);
        assert_eq!(pins.get("Duration"), Some(CanonicalType::Varchar));
        assert_eq!(pins.get("duration"), Some(CanonicalType::Varchar));
    }

    #[test]
    fn intersect_does_not_fold_non_ascii_case() {
        // DuckDB folds ASCII only — `café` and `CAFÉ` are two columns, each
        // nameable, so both pins survive.
        let cache = catalog(&[
            ("café", CanonicalType::Varchar),
            ("CAFÉ", CanonicalType::BigInt),
        ]);
        let pins = cache.intersect(["café", "CAFÉ"]);
        assert_eq!(pins.get("café"), Some(CanonicalType::Varchar));
        assert_eq!(pins.get("CAFÉ"), Some(CanonicalType::BigInt));
    }
}

/// Everything compaction needs to enforce the write-time invariant: the
/// durable store (pins are written here BEFORE any parquet carrying them)
/// and the in-process cache (refreshed after every pin write so the query
/// path sees new pins without touching postgres).
#[derive(Debug, Clone)]
pub struct CatalogContext {
    /// Durable catalog store (postgres).
    pub store: CatalogStore,
    /// Shared in-process pin cache.
    pub cache: Arc<FieldCatalog>,
}
