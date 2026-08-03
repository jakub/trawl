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

    /// Replace the whole cache with an authoritative pin set.
    pub fn replace(&self, pins: impl IntoIterator<Item = (String, CanonicalType)>) {
        *self.pins.write() = pins.into_iter().collect();
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
    /// Fields whose spelling collides case-insensitively with another key in
    /// the SAME snapshot are dropped: they are unconformable, and conforming
    /// them would rewrite the wrong column. Catalog names are
    /// case-SENSITIVE (client JSON keys, never case-normalised) while
    /// `DuckDB` identifiers are case-INSENSITIVE, so a snapshot carrying both
    /// `duration` and `Duration` is read as columns `duration` and
    /// `Duration_1` — `read_json` renames the collided key, and a `REPLACE`
    /// naming EITHER spelling binds the FIRST column (probed by execution in
    /// `trawl-engine/tests/duckdb_probe.rs`). Emitting the pin there would
    /// silently retype and rewrite the other service's values while leaving
    /// the pinned field's own data (now in `Duration_1`) untouched — the
    /// server guessing where it has no honest answer (ADR-0009). Dropped
    /// instead, both columns reach the union exactly as ingested: a genuine
    /// disagreement with the pin then errors loudly rather than being
    /// papered over on the wrong column.
    #[must_use]
    pub fn intersect<'a>(&self, keys: impl IntoIterator<Item = &'a str>) -> FieldTypes {
        let keys: Vec<&str> = keys.into_iter().collect();
        let collided = ascii_case_collisions(keys.iter().copied());
        let pins = self.pins.read();
        let mut out = FieldTypes::new();
        for key in keys {
            if collided.contains(&key.to_ascii_uppercase()) {
                continue;
            }
            if let Some(ty) = pins.get(key) {
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
    fn intersect_drops_pins_for_case_collided_keys() {
        // Both spellings in one snapshot: read_json exposes `duration` and
        // `Duration_1`, so NEITHER pin can name its own column. Conforming
        // either would rewrite the other service's values.
        let cache = catalog(&[
            ("duration", CanonicalType::BigInt),
            ("Duration", CanonicalType::Varchar),
            ("host", CanonicalType::Varchar),
        ]);
        let pins = cache.intersect(["duration", "Duration", "host"]);
        assert_eq!(pins.get("duration"), None);
        assert_eq!(pins.get("Duration"), None);
        assert_eq!(pins.get("host"), Some(CanonicalType::Varchar));
    }

    #[test]
    fn intersect_drops_a_pin_collided_by_an_unpinned_spelling() {
        // The collision does not need two pins: one pinned spelling plus a
        // brand-new event carrying the other spelling is enough to make the
        // pin bind the wrong column.
        let cache = catalog(&[("Duration", CanonicalType::Varchar)]);
        assert!(cache.intersect(["Duration", "duration"]).is_empty());
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
