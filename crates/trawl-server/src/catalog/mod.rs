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
    #[must_use]
    pub fn intersect<'a>(&self, keys: impl IntoIterator<Item = &'a str>) -> FieldTypes {
        let pins = self.pins.read();
        let mut out = FieldTypes::new();
        for key in keys {
            if let Some(ty) = pins.get(key) {
                out.insert(key, *ty);
            }
        }
        out
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
