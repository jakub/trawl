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
//! ## Why a fixed capacity, oldest-insertion-first
//!
//! The cache holds at most [`VERIFICATION_CACHE_CAPACITY`] entries. Only a
//! verification that ran argon2id and then passed every database check
//! inserts, and argon2id admits one plaintext per stored hash, so an
//! attacker without a key cannot grow the map. Growth comes from legitimate
//! churn instead: automation that mints many short-lived keys over a long
//! uptime, and rotations that leave entries for the old hash behind. Each
//! would once have grown the map until restart.
//!
//! When the cache is full, the oldest insertion is evicted first. A lookup
//! is not a use and does not reorder anything, and a duplicate insert
//! neither adds a second queue record nor renews the entry, so the queue
//! and the set stay the same size. A short mutex covers the evict-and-insert
//! step, which is what holds the cap exactly under concurrent inserts; the
//! lock is never held across an await or the KDF. An evicted key's next
//! request reruns argon2id under the process-wide permit limit, which costs
//! latency, never authorization.
//!
//! There is no TTL, because nothing a TTL would refresh lives here. The
//! cache only records "argon2id already accepted this exact plaintext for
//! this stored hash". Activity, expiry, revocation and roles are read from
//! Postgres on every request, so revoking or expiring a key refuses its
//! next use whether or not its entry is cached, and a rotated key changes
//! `stored_hash`, which makes the old entry unreachable until it ages out.
//! A rotation that commits while a verification is in flight refuses that
//! verification too, cached or not: its `last_used` update requires the
//! hash it read, so the old plaintext is never authenticated against a
//! hash it was not checked under.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

/// How many successful verifications the cache holds at most.
///
/// A shared Fleet contract with no config surface: a few thousand live
/// tokens is far above the fleet's scale, and an installation that somehow
/// exceeds it pays argon2id more often, not a wrong answer.
pub const VERIFICATION_CACHE_CAPACITY: usize = 4096;

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

/// The state behind the mutex: the presence set and the insertion order.
///
/// Both hold the same `Arc`s, so a key is stored once. `order` has no
/// duplicates, because a duplicate insert returns before pushing, so
/// `order.len() == entries.len()` always holds and the front of `order`
/// is the oldest live entry.
#[derive(Debug)]
struct Inner {
    entries: HashSet<Arc<VerificationCacheKey>>,
    order: VecDeque<Arc<VerificationCacheKey>>,
    capacity: usize,
    /// How many `insert` calls reached the cache, duplicates included. A
    /// test seam for the store: a cache hit must not call `insert` at all.
    #[cfg(test)]
    insert_calls: usize,
}

/// Bounded, concurrent, presence-only cache of successful argon2id
/// verifications. See the module docs for the capacity and the eviction
/// policy.
///
/// `Clone` is cheap (`Arc`-shared state), so `KeyStore` can derive `Clone`
/// for free axum extractor / `FromRef` ergonomics.
#[derive(Clone, Debug)]
pub struct VerificationCache {
    inner: Arc<Mutex<Inner>>,
}

impl Default for VerificationCache {
    fn default() -> Self {
        Self::with_capacity(VERIFICATION_CACHE_CAPACITY)
    }
}

impl VerificationCache {
    /// A cache that holds [`VERIFICATION_CACHE_CAPACITY`] entries.
    pub fn new() -> Self {
        Self::default()
    }

    /// A cache that holds `capacity` entries. Crate-private: the store's
    /// tests use a small capacity to reach eviction without thousands of
    /// inserts, and the product always uses [`VERIFICATION_CACHE_CAPACITY`].
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                entries: HashSet::with_capacity(capacity),
                order: VecDeque::with_capacity(capacity),
                capacity,
                #[cfg(test)]
                insert_calls: 0,
            })),
        }
    }

    /// Lock the state. Nothing under the lock can panic short of an
    /// allocation failure, and a poisoned set and queue are still
    /// consistent with each other, so a poisoned lock is simply reused.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// True if the key is present (KDF already verified for this exact
    /// `(prefix, stored_hash, plaintext)` tuple). A lookup does not count
    /// as a use: it never reorders the eviction queue.
    pub fn contains(&self, key: &VerificationCacheKey) -> bool {
        self.lock().entries.contains(key)
    }

    /// Insert a successful verification.
    ///
    /// A key that is already present is left exactly as it was: it is not
    /// queued again and its position in the eviction order does not change.
    /// A new key evicts the oldest insertion first when the cache is full,
    /// under the one lock, so the entry count never passes the capacity.
    pub fn insert(&self, key: VerificationCacheKey) {
        let mut inner = self.lock();
        #[cfg(test)]
        {
            inner.insert_calls += 1;
        }
        if inner.entries.contains(&key) {
            return;
        }
        while inner.entries.len() >= inner.capacity {
            let Some(oldest) = inner.order.pop_front() else {
                // Unreachable while the set and the queue stay the same
                // size, but a capacity of zero must not spin forever.
                break;
            };
            inner.entries.remove(&oldest);
        }
        let key = Arc::new(key);
        inner.order.push_back(Arc::clone(&key));
        inner.entries.insert(key);
    }

    /// Snapshot stats for observability or testing.
    pub fn stats(&self) -> VerificationCacheStats {
        VerificationCacheStats {
            entries: self.lock().entries.len(),
        }
    }

    /// How many times `insert` was called on this cache, duplicates
    /// included. The store's tests read it to prove a cache hit inserts
    /// nothing.
    #[cfg(test)]
    pub(crate) fn insert_calls(&self) -> usize {
        self.lock().insert_calls
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

    // ------------------------------------------------------------------
    // Capacity bound (#304): the cache never holds more than its capacity,
    // evicts the oldest insertion first, and a duplicate insert neither
    // grows the queue nor renews the entry.
    // ------------------------------------------------------------------

    fn body_key(i: usize) -> VerificationCacheKey {
        key(
            "AAAAAAAA",
            &format!("flt_AAAAAAAAbody_{i:028}"),
            "$argon2id$v=19$m=1024$abc$xyz",
        )
    }

    #[test]
    fn verification_cache_never_exceeds_capacity() {
        let cache = VerificationCache::with_capacity(8);
        for i in 0..(8 + 100) {
            cache.insert(body_key(i));
            assert!(
                cache.stats().entries <= 8,
                "insert {i} left {} entries in a cache of 8",
                cache.stats().entries
            );
        }
        assert_eq!(cache.stats().entries, 8);

        // The shipped constant, not only the seam.
        let cache = VerificationCache::new();
        for i in 0..=VERIFICATION_CACHE_CAPACITY {
            cache.insert(body_key(i));
        }
        assert_eq!(cache.stats().entries, VERIFICATION_CACHE_CAPACITY);
    }

    #[test]
    fn verification_cache_never_exceeds_capacity_under_concurrent_insert() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::thread;

        const CAPACITY: usize = 16;
        const WRITERS: usize = 32;
        const INSERTS_PER_WRITER: usize = 256;

        let cache = VerificationCache::with_capacity(CAPACITY);
        let writing = Arc::new(AtomicBool::new(true));
        let worst = Arc::new(AtomicUsize::new(0));

        // Readers sample the entry count the whole time the writers run, so
        // a transient overshoot between evict and insert would be seen.
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let c = cache.clone();
                let writing = Arc::clone(&writing);
                let worst = Arc::clone(&worst);
                thread::spawn(move || {
                    while writing.load(Ordering::Acquire) {
                        worst.fetch_max(c.stats().entries, Ordering::AcqRel);
                    }
                })
            })
            .collect();

        let writers: Vec<_> = (0..WRITERS)
            .map(|w| {
                let c = cache.clone();
                thread::spawn(move || {
                    for i in 0..INSERTS_PER_WRITER {
                        c.insert(body_key(w * INSERTS_PER_WRITER + i));
                    }
                })
            })
            .collect();
        for h in writers {
            h.join().unwrap();
        }
        writing.store(false, Ordering::Release);
        for h in readers {
            h.join().unwrap();
        }

        assert!(
            worst.load(Ordering::Acquire) <= CAPACITY,
            "a reader observed {} entries in a cache of {CAPACITY}",
            worst.load(Ordering::Acquire)
        );
        assert_eq!(cache.stats().entries, CAPACITY);
    }

    #[test]
    fn verification_cache_evicts_oldest_inserted_entry() {
        let cache = VerificationCache::with_capacity(3);
        let first = body_key(0);
        let second = body_key(1);
        let third = body_key(2);
        let fourth = body_key(3);
        let fifth = body_key(4);
        cache.insert(first.clone());
        cache.insert(second.clone());
        cache.insert(third.clone());
        assert_eq!(cache.stats().entries, 3);

        cache.insert(fourth.clone());
        assert!(
            !cache.contains(&first),
            "the oldest insertion is evicted first"
        );
        assert!(cache.contains(&second));
        assert!(cache.contains(&third));
        assert!(cache.contains(&fourth));
        assert_eq!(cache.stats().entries, 3);

        // A lookup is not a use: `second` was just read, and it still goes
        // next.
        cache.insert(fifth.clone());
        assert!(!cache.contains(&second));
        assert!(cache.contains(&third));
        assert!(cache.contains(&fourth));
        assert!(cache.contains(&fifth));
        assert_eq!(cache.stats().entries, 3);
    }

    #[test]
    fn verification_cache_duplicate_insert_does_not_grow() {
        let cache = VerificationCache::with_capacity(2);
        let (a, b, c) = (body_key(0), body_key(1), body_key(2));
        cache.insert(a.clone());
        cache.insert(a.clone());
        assert_eq!(cache.stats().entries, 1, "a duplicate insert adds no entry");

        cache.insert(b.clone());
        assert_eq!(cache.stats().entries, 2);

        // Re-inserting `a` must not renew it: it stays the oldest insertion,
        // so the next eviction removes `a`, not `b`. Were the duplicate
        // queued again, `b` would go first and `a` would outlive it.
        cache.insert(a.clone());
        cache.insert(c.clone());
        assert_eq!(cache.stats().entries, 2);
        assert!(!cache.contains(&a), "the duplicate did not renew `a`");
        assert!(cache.contains(&b));
        assert!(cache.contains(&c));

        // And the queue did not grow behind the set: one more insert evicts
        // exactly one live entry, leaving the other two.
        cache.insert(body_key(3));
        assert_eq!(cache.stats().entries, 2);
        assert!(!cache.contains(&b));
        assert!(cache.contains(&c));
    }
}
