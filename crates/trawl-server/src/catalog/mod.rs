// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! In-process field-catalog machinery (ADR-0009): the pin cache that keeps
//! the query path free of postgres I/O, and the context handle compaction
//! uses to pin and conform.

pub mod analyzer;
pub mod conform;
pub mod gc;

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use trawl_core::schema::{CanonicalType, FieldTypes};

use crate::store::CatalogStore;

#[derive(Debug, Default)]
pub struct FieldCatalog {
    pins: RwLock<HashMap<String, CanonicalType>>,
    repin_generation: std::sync::atomic::AtomicU64,
    #[cfg(any(test, feature = "test-support"))]
    pin_reads: parking_lot::Mutex<Vec<PinRead>>,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinRead {
    pub repin_generation: u64,
}

impl FieldCatalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replace(&self, pins: impl IntoIterator<Item = (String, CanonicalType)>) {
        *self.pins.write() = pins.into_iter().collect();
    }

    /// Overwrite one field's pin: the repin cutover's flip, and the only
    /// path here that is not add-only. Runs while the cutover holds every
    /// query permit, so no in-flight query can observe half a flip.
    ///
    /// The bump is Release inside the write-lock scope,
    /// [`Self::repin_generation`] reads Acquire, and the postgres commit
    /// precedes the bump, so a reader that observes the new stamp can never
    /// see the old pins or the old postgres row. The converse is possible
    /// and safe: a reader that loaded the old stamp may see post-flip state,
    /// producing a cache entry stamped with the old generation, which the
    /// next read discards (over-invalidation, never staleness).
    pub fn repin(&self, field: &str, ty: CanonicalType) {
        let mut guard = self.pins.write();
        guard.insert(field.to_owned(), ty);
        self.repin_generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    /// Infallible on purpose. The gc engine calls this immediately after
    /// the postgres commit, inside the corpus gate, with nothing fallible
    /// in between, so the cache can never be emptied ahead of the store.
    pub fn evict_many<'a>(&self, fields: impl IntoIterator<Item = &'a str>) -> usize {
        let mut guard = self.pins.write();
        fields
            .into_iter()
            .filter(|field| guard.remove(*field).is_some())
            .count()
    }

    /// Ordered Release like the
    /// repin's, and taken after the map write it describes, so a reader can
    /// see the new pins under the old stamp (over-invalidation, discarded
    /// on the next read) but never the old pins under the new one.
    pub fn touch_generation(&self) {
        self.repin_generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    #[must_use]
    pub fn repin_generation(&self) -> u64 {
        self.repin_generation
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn merge(&self, pins: impl IntoIterator<Item = (String, CanonicalType)>) {
        let mut guard = self.pins.write();
        for (field, ty) in pins {
            guard.insert(field, ty);
        }
    }

    #[must_use]
    pub fn get(&self, field: &str) -> Option<CanonicalType> {
        self.pins.read().get(field).copied()
    }

    #[must_use]
    pub fn snapshot(&self) -> HashMap<String, CanonicalType> {
        self.pins.read().clone()
    }

    /// Deliberately unfiltered, unlike [`Self::intersect`]: the
    /// hot-intersected set is empty with no hot buffer and misses
    /// cold-only fields, so typing comparisons with it would make
    /// `status>=400` mean different things depending on ingest timing.
    #[must_use]
    pub fn all(&self) -> FieldTypes {
        let pins = self.pins.read();
        #[cfg(any(test, feature = "test-support"))]
        self.pin_reads.lock().push(PinRead {
            repin_generation: self.repin_generation(),
        });
        let mut out = FieldTypes::new();
        for (field, ty) in pins.iter() {
            out.insert(field, *ty);
        }
        out
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn pins_read_for_test(&self) -> Vec<PinRead> {
        self.pin_reads.lock().clone()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn clear_pins_read_for_test(&self) {
        self.pin_reads.lock().clear();
    }

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
        let cache = catalog(&[("duration", CanonicalType::BigInt)]);
        let pins = cache.intersect(["Duration"]);
        assert_eq!(pins.get("Duration"), None);
        assert!(pins.is_empty());
    }

    #[test]
    fn all_returns_the_full_unfiltered_snapshot() {
        let cache = catalog(&[
            ("duration", CanonicalType::BigInt),
            ("status", CanonicalType::Varchar),
        ]);
        let all = cache.all();
        assert_eq!(all.get("duration"), Some(CanonicalType::BigInt));
        assert_eq!(all.get("status"), Some(CanonicalType::Varchar));
        assert_eq!(all.len(), 2);
        assert!(FieldCatalog::new().all().is_empty());
    }

    #[test]
    fn merge_adds_deltas_without_dropping_existing_pins() {
        let cache = catalog(&[("duration", CanonicalType::BigInt)]);
        cache.merge([("status".to_owned(), CanonicalType::BigInt)]);
        assert_eq!(cache.get("duration"), Some(CanonicalType::BigInt));
        assert_eq!(cache.get("status"), Some(CanonicalType::BigInt));
        assert_eq!(cache.snapshot().len(), 2);
    }

    #[test]
    fn repin_overwrites_exactly_one_key() {
        let cache = catalog(&[
            ("status", CanonicalType::BigInt),
            ("dur", CanonicalType::Double),
        ]);

        cache.repin("status", CanonicalType::Varchar);

        assert_eq!(cache.get("status"), Some(CanonicalType::Varchar));
        assert_eq!(cache.get("dur"), Some(CanonicalType::Double));
        assert_eq!(cache.snapshot().len(), 2, "an overwrite, never an add");
    }

    #[test]
    fn only_a_repin_bumps_the_generation() {
        let cache = catalog(&[("status", CanonicalType::BigInt)]);
        let start = cache.repin_generation();

        cache.merge([("dur".to_owned(), CanonicalType::Double)]);
        assert_eq!(cache.repin_generation(), start, "merge is add-only");

        cache.replace([("status".to_owned(), CanonicalType::BigInt)]);
        assert_eq!(
            cache.repin_generation(),
            start,
            "replace is boot hydration, not a retype"
        );

        cache.repin("status", CanonicalType::Varchar);
        assert_eq!(cache.repin_generation(), start + 1, "a repin bumps");

        cache.repin("status", CanonicalType::BigInt);
        assert_eq!(cache.repin_generation(), start + 2, "every repin bumps");
    }

    #[test]
    fn evict_many_removes_the_set_without_stamping_it() {
        let cache = catalog(&[
            ("typo", CanonicalType::Varchar),
            ("dead", CanonicalType::BigInt),
            ("live", CanonicalType::Double),
        ]);
        let start = cache.repin_generation();

        let removed = cache.evict_many(["typo", "dead", "never_pinned"]);

        assert_eq!(removed, 2, "only the pins that were there count");
        assert_eq!(cache.get("typo"), None);
        assert_eq!(cache.get("dead"), None);
        assert_eq!(cache.get("live"), Some(CanonicalType::Double));
        assert_eq!(
            cache.repin_generation(),
            start,
            "the eviction does not stamp itself"
        );

        cache.touch_generation();
        assert_eq!(cache.repin_generation(), start + 1, "one purge, one bump");
    }

    #[test]
    fn a_purge_the_cache_never_held_still_bumps_the_generation() {
        let cache = catalog(&[("live", CanonicalType::Double)]);
        let start = cache.repin_generation();

        assert_eq!(cache.evict_many(["dead"]), 0, "nothing was in the map");
        assert_eq!(cache.evict_many(std::iter::empty()), 0);
        assert_eq!(cache.repin_generation(), start, "the map did not change");

        cache.touch_generation();

        assert_eq!(cache.repin_generation(), start + 1, "the store did");
        assert_eq!(cache.get("live"), Some(CanonicalType::Double));
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

#[derive(Debug, Clone)]
pub struct CatalogContext {
    pub store: CatalogStore,
    pub cache: Arc<FieldCatalog>,
}
