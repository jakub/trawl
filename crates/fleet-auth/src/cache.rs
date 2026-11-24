// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Argon2id result cache for the auth verification hot path.
//!
//! Keying on `(prefix, stored_hash)` alone is broken: after one successful
//! `verify_key("flt_AAAAAAAA<...>")`, `verify_key("flt_AAAAAAAA<garbage>")`
//! would hit the cache, skip argon2id, and succeed. Binding the key to
//! `blake3(plaintext)` closes the bypass.
//!
//! There is no TTL: activity, expiry, revocation and roles are read from
//! Postgres on every request, so revoking or expiring a key refuses its
//! next use whether or not its entry is cached.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

pub const VERIFICATION_CACHE_CAPACITY: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct VerificationCacheKey {
    pub prefix: String,
    pub stored_hash: String,
    pub token_fingerprint: [u8; 32],
}

impl VerificationCacheKey {
    pub fn build(prefix: &str, plaintext: &str, stored_hash: &str) -> Self {
        let fp = blake3::hash(plaintext.as_bytes());
        Self {
            prefix: prefix.to_owned(),
            stored_hash: stored_hash.to_owned(),
            token_fingerprint: *fp.as_bytes(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct VerificationCacheStats {
    pub entries: usize,
}

/// `order` has no duplicates, because a duplicate insert returns before
/// pushing, so `order.len() == entries.len()` always holds and the front of
/// `order` is the oldest live entry.
#[derive(Debug)]
struct Inner {
    entries: HashSet<Arc<VerificationCacheKey>>,
    order: VecDeque<Arc<VerificationCacheKey>>,
    capacity: usize,
    #[cfg(test)]
    insert_calls: usize,
}

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
    pub fn new() -> Self {
        Self::default()
    }

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

    pub fn contains(&self, key: &VerificationCacheKey) -> bool {
        self.lock().entries.contains(key)
    }

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

    pub fn stats(&self) -> VerificationCacheStats {
        VerificationCacheStats {
            entries: self.lock().entries.len(),
        }
    }

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

        cache.insert(a.clone());
        cache.insert(c.clone());
        assert_eq!(cache.stats().entries, 2);
        assert!(!cache.contains(&a), "the duplicate did not renew `a`");
        assert!(cache.contains(&b));
        assert!(cache.contains(&c));

        cache.insert(body_key(3));
        assert_eq!(cache.stats().entries, 2);
        assert!(!cache.contains(&b));
        assert!(cache.contains(&c));
    }
}
