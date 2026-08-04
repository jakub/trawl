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
//!
//! Every name in the catalog is ASCII-lowercase by construction: the
//! ingest canonicalizer folds field names before anything reads them, the
//! boot pass folds when seeding from standing parquet, and compaction
//! folds when proposing from a stale WAL. One `DuckDB` identifier therefore
//! has exactly one catalog spelling, and lookups are plain exact-name.

pub mod conform;

use std::collections::HashMap;
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
    /// same map a full reload would, without re-reading a catalog sized by
    /// how many distinct field names clients have ever sent (bounded, but
    /// only by [`crate::store::MAX_PINNED_FIELDS`]). Compaction runs this
    /// once per batch that actually pinned something; a batch proposing
    /// nothing touches neither postgres nor this lock.
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
    /// A plain exact-name lookup: catalog names AND hot-snapshot keys are
    /// both ASCII-folded at their sources (ingest canonicalization; boot
    /// seeding), so two spellings of one `DuckDB` identifier cannot meet
    /// here. The former case-variant defence — degrading any colliding
    /// spelling to `VARCHAR` — is deliberately gone: with folded names it
    /// could never fire on real pins again, and while it existed it broke
    /// every numeric comparison on a field the (unfolded) catalog held two
    /// spellings of, permanently.
    #[must_use]
    pub fn intersect<'a>(&self, keys: impl IntoIterator<Item = &'a str>) -> FieldTypes {
        let pins = self.pins.read();
        let mut out = FieldTypes::new();
        for key in keys {
            if let Some(ty) = pins.get(key).copied() {
                out.insert(key, ty);
            }
        }
        out
    }
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
    fn intersect_is_exact_name_lookup() {
        // Folding happens at ingest/boot, not here: a key spelled
        // differently from every pin simply matches nothing. (Such a key
        // cannot reach this code from the wired system — hot-snapshot keys
        // are ingest-folded — so no defensive conform is warranted.)
        let cache = catalog(&[("duration", CanonicalType::BigInt)]);
        let pins = cache.intersect(["Duration"]);
        assert_eq!(pins.get("Duration"), None);
        assert!(pins.is_empty());
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
    fn merge_last_write_wins_for_one_spelling() {
        let cache = catalog(&[]);
        cache.merge([("duration".to_owned(), CanonicalType::BigInt)]);
        cache.merge([("duration".to_owned(), CanonicalType::Double)]);
        assert_eq!(
            cache.intersect(["duration"]).get("duration"),
            Some(CanonicalType::Double)
        );
        assert_eq!(cache.snapshot().len(), 1);
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
