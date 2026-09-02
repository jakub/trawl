// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Argon2id result cache for the auth verification hot path.
//!
//! ## Why a cache at all
//!
//! `verify_key` runs argon2id (production: ~200 ms / 128 MiB / 3 iter) on
//! every authenticated request. Caching the successful KDF result lets the
//! second and subsequent calls return in ~1 ms (just the Postgres liveness
//! query) without sacrificing revocation latency — the `active` flag check
//! always runs against the database, never the cache.
//!
//! ## Why three-component key
//!
//! Naively, you'd key the cache on `(prefix, stored_hash)`. That is broken:
//! after one successful `verify_key("flt_AAAAAAAA<...>")`, any subsequent
//! `verify_key("flt_AAAAAAAA<garbage>")` would hit the cache (same prefix,
//! same row, same stored hash), skip argon2id, and succeed. Auth bypass.
//!
//! Binding the key to a non-secret fingerprint of the presented token
//! (`blake3(plaintext)`) closes the bypass: a different plaintext produces a
//! different fingerprint and misses the cache, forcing argon2id to run.
//!
//! ## Why `DashMap` with no eviction
//!
//! At fleet scale (<30 keys × maybe a few rotation revisions = bounded),
//! the cache can't grow without bound — only successful verifications are
//! inserted, so attack-spray of random tokens never grows the map. A key
//! rotation produces a new `stored_hash` that misses naturally; the orphaned
//! entry sits inert until process restart. Past a few thousand entries, swap
//! to a bounded LRU (e.g. moka).

use std::sync::Arc;

use dashmap::DashMap;

/// Three-component key for the argon2id result cache.
///
/// `prefix` and `stored_hash` invalidate naturally on rotation (the stored
/// hash changes); `token_fingerprint` (blake3 of the plaintext) binds the
/// cache entry to the exact token that produced it, preventing the
/// "same-prefix bypass" described in the module docs.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct VerificationCacheKey {
    /// The 8-char operational prefix from the token body.
    pub prefix: String,
    /// The PHC hash string as currently stored in the database for this
    /// prefix. Changes on rotation; ensures cached entries become unreachable
    /// after a hash change.
    pub stored_hash: String,
    /// blake3 fingerprint of the full plaintext token. Non-secret on its own
    /// (and never exposed) but binds the cache entry to the exact token that
    /// successfully verified.
    pub token_fingerprint: [u8; 32],
}

impl VerificationCacheKey {
    /// Compute the key for a presented plaintext + the stored hash returned
    /// by the indexed Postgres lookup.
    pub fn build(prefix: &str, plaintext: &str, stored_hash: &str) -> Self {
        let fp = blake3::hash(plaintext.as_bytes());
        Self {
            prefix: prefix.to_owned(),
            stored_hash: stored_hash.to_owned(),
            token_fingerprint: *fp.as_bytes(),
        }
    }
}

/// Snapshot of cache state for observability / tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct VerificationCacheStats {
    /// Current number of cached successful KDF results.
    pub entries: usize,
}

/// Concurrent presence-only cache of successful argon2id verifications.
///
/// `Clone` is cheap (Arc-shared `DashMap`), so `KeyStore` can derive `Clone`
/// for free axum extractor / `FromRef` ergonomics.
#[derive(Clone, Debug, Default)]
pub struct VerificationCache {
    inner: Arc<DashMap<VerificationCacheKey, ()>>,
}

impl VerificationCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// True if the key is present (KDF already verified for this exact
    /// `(prefix, stored_hash, plaintext)` tuple).
    pub fn contains(&self, key: &VerificationCacheKey) -> bool {
        self.inner.contains_key(key)
    }

    /// Insert a successful verification. Idempotent.
    pub fn insert(&self, key: VerificationCacheKey) {
        self.inner.insert(key, ());
    }

    /// Snapshot stats for observability or testing.
    pub fn stats(&self) -> VerificationCacheStats {
        VerificationCacheStats {
            entries: self.inner.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(prefix: &str, plaintext: &str, stored_hash: &str) -> VerificationCacheKey {
        VerificationCacheKey::build(prefix, plaintext, stored_hash)
    }

    #[test]
    fn insert_then_contains_hits() {
        let cache = VerificationCache::new();
        let k = key(
            "AAAAAAAA",
            "flt_AAAAAAAAfull_token_body",
            "$argon2id$v=19$m=1024$abc$xyz",
        );
        assert!(!cache.contains(&k));
        cache.insert(k.clone());
        assert!(cache.contains(&k));
        assert_eq!(cache.stats().entries, 1);
    }

    #[test]
    fn different_plaintext_produces_different_key_even_with_same_prefix_and_hash() {
        // The security test for the cache-key shape: without
        // `token_fingerprint` in the key, both lookups would hit and the
        // second (wrong) token would skip argon2id.
        let cache = VerificationCache::new();
        let k_legit = key(
            "AAAAAAAA",
            "flt_AAAAAAAAlegitimate_token_body_xxxxxxxxxxxxxxx",
            "$argon2id$v=19$m=1024$abc$xyz",
        );
        cache.insert(k_legit.clone());

        let k_attacker = key(
            "AAAAAAAA",
            "flt_AAAAAAAAattacker_supplied_body_yyyyyyyyyyyyyy",
            "$argon2id$v=19$m=1024$abc$xyz",
        );
        assert!(
            !cache.contains(&k_attacker),
            "attacker plaintext sharing prefix+stored_hash MUST NOT hit cache"
        );
        assert_ne!(k_legit.token_fingerprint, k_attacker.token_fingerprint);
    }

    #[test]
    fn different_stored_hash_invalidates() {
        // Simulates key rotation: same prefix, same plaintext, but DB now
        // stores a fresh hash → cache miss, argon2id re-runs, new entry inserted.
        let cache = VerificationCache::new();
        let plaintext = "flt_AAAAAAAAtoken_body_xxxxxxxxxxxxxxxxxxxxxxx";
        let pre = key("AAAAAAAA", plaintext, "$argon2id$v=19$m=1024$old$old");
        let post = key("AAAAAAAA", plaintext, "$argon2id$v=19$m=1024$new$new");
        cache.insert(pre);
        assert!(!cache.contains(&post));
    }

    #[test]
    fn build_is_deterministic() {
        let a = VerificationCacheKey::build("p", "flt_pAAAAAAA", "h");
        let b = VerificationCacheKey::build("p", "flt_pAAAAAAA", "h");
        assert_eq!(a, b);
        assert_eq!(a.token_fingerprint, b.token_fingerprint);
    }

    #[test]
    fn concurrent_inserts_do_not_panic() {
        use std::thread;

        let cache = VerificationCache::new();
        let mut handles = vec![];
        for i in 0..32u32 {
            let c = cache.clone();
            handles.push(thread::spawn(move || {
                let k = key(
                    "AAAAAAAA",
                    &format!("flt_AAAAAAAAbody_{i:028}"),
                    "$argon2id$v=19$m=1024$abc$xyz",
                );
                c.insert(k);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(cache.stats().entries, 32);
    }
}
