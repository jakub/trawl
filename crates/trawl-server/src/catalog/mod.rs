// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! In-process field-catalog machinery (ADR-0009 slice 2): the pin cache
//! that keeps the query path free of postgres I/O, and the context handle
//! compaction uses to pin and conform.
//!
//! The postgres tables live in [`crate::store::catalog`]; this module is
//! the process-local view. Boot hydrates it, every `pin_missing` refresh
//! adds entries — and since ADR-0011 slice B the repin cutover overwrites
//! exactly one entry through [`FieldCatalog::repin`], the cache's first
//! non-add-only path. "Add-only" is therefore no longer a cache invariant.
//! Nothing needs to detect a stale snapshot: a query roots ONE snapshot
//! per execution and the cutover's exclusion primitives guarantee no query
//! straddles a flip.
//!
//! Every name in the catalog is ASCII-lowercase by construction: each
//! producer folds field names at its own door — HTTP ingest in
//! `envelope::canonicalize`, the syslog listener at SD-key construction
//! (`crate::syslog::convert`), telemetry in its `JsonVisitor` — and the
//! catalog folds again at its own entry points (the boot pass when seeding
//! from standing parquet, compaction when proposing from a stale WAL). One
//! `DuckDB` identifier therefore has exactly one catalog spelling, and
//! lookups are plain exact-name.

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

    /// Overwrite ONE field's pin — the repin cutover's flip (ADR-0011
    /// slice B), and the cache's first non-add-only path. Runs while the
    /// cutover holds every query permit, so no in-flight query can observe
    /// half a flip.
    pub fn repin(&self, field: &str, ty: CanonicalType) {
        self.pins.write().insert(field.to_owned(), ty);
    }

    /// Fold newly-durable pins into the cache, leaving every other entry
    /// alone.
    ///
    /// This — not [`Self::replace`] — is the steady-state update: on the
    /// COMPACTION path pins only ever appear (`pin_missing` never rewrites
    /// one — the one path that does is the repin cutover, which goes
    /// through [`Self::repin`]), so a delta merge lands the same map a full
    /// reload would, without re-reading a catalog sized by how many
    /// distinct field names clients have ever sent (bounded, but only by
    /// [`crate::store::MAX_PINNED_FIELDS`]). Compaction runs this once per
    /// batch that actually pinned something; a batch proposing nothing
    /// touches neither postgres nor this lock.
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

    /// The FULL pin set as a [`FieldTypes`] — the comparison-typing
    /// snapshot the query path passes to `emit_with_pins` and
    /// `CompiledFilter::compile` (ADR-0011 slice A).
    ///
    /// Deliberately unfiltered, unlike [`Self::intersect`]: the
    /// hot-intersected set is empty with no hot buffer and misses
    /// cold-only fields, so typing comparisons with it would make
    /// `status>=400` mean different things depending on ingest timing.
    /// The clone is bounded by `MAX_PINNED_FIELDS` (10k) and typically
    /// tiny; taken once per query.
    #[must_use]
    pub fn all(&self) -> FieldTypes {
        let pins = self.pins.read();
        let mut out = FieldTypes::new();
        for (field, ty) in pins.iter() {
            out.insert(field, *ty);
        }
        out
    }

    /// The pins intersected with a hot snapshot's key set — the
    /// [`FieldTypes`] the emitter conforms the hot side of the union with.
    /// Zero postgres I/O: this is the whole point of the cache.
    ///
    /// A plain exact-name lookup: catalog names AND hot-snapshot keys are
    /// both ASCII-folded at their sources (every producer folds at its own
    /// door — see the module doc; boot seeding and compaction proposals
    /// fold on the catalog side), so two spellings of one `DuckDB`
    /// identifier cannot meet here. The former case-variant defence —
    /// degrading any colliding spelling to `VARCHAR` — is deliberately
    /// gone: with folded names it could never fire on real pins again, and
    /// while it existed it broke every numeric comparison on a field the
    /// (unfolded) catalog held two spellings of, permanently.
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
    fn all_returns_the_full_unfiltered_snapshot() {
        // The comparison-typing set (ADR-0011 slice A) must be the FULL
        // catalog — the hot-intersected set is empty with no hot buffer
        // and misses cold-only fields, so `status>=400` would change
        // meaning with ingest timing if `intersect` were reused.
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
        // The steady-state compaction update: only the batch's own new pins
        // are known, and folding them in must not evict the rest of the
        // catalog the way `replace` would.
        let cache = catalog(&[("duration", CanonicalType::BigInt)]);
        cache.merge([("status".to_owned(), CanonicalType::BigInt)]);
        assert_eq!(cache.get("duration"), Some(CanonicalType::BigInt));
        assert_eq!(cache.get("status"), Some(CanonicalType::BigInt));
        assert_eq!(cache.snapshot().len(), 2);
    }

    /// The first non-add-only path (ADR-0011 slice B): a repin overwrites
    /// exactly one key and leaves every other pin alone — unlike `merge`,
    /// which only ever adds.
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
