// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Query lifecycle tracking — active queries, history ring buffer, and timeout recording.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::time::Instant;

use dashmap::DashMap;
use fleet_auth::VerifiedKey;
use trawl_api::{ActiveQuerySnapshot, CompletedQuerySnapshot};

/// Tracks active and recently completed queries.
///
/// Query ids are allocated by `ExecutorPool::allocate_query_id` — a single
/// id space shared with the pool's interrupt map, so an id listed by
/// `/queries` is the same id `cancel_by_id` interrupts.
#[derive(Debug)]
pub struct QueryTracker {
    active: DashMap<u64, ActiveQuery>,
    history: Mutex<VecDeque<CompletedQuerySnapshot>>,
    max_history: usize,
}

/// A currently executing query.
#[derive(Debug, Clone)]
pub struct ActiveQuery {
    /// Monotonic query ID.
    pub id: u64,
    /// Fleet keystore id of the submitting key — the authorization anchor
    /// for non-admin cancellation (names are mutable and non-unique).
    pub key_id: i64,
    /// Authenticated user name (display only).
    pub user: String,
    /// User's role.
    pub role: String,
    /// The DSL query string.
    pub query: String,
    /// When execution started.
    pub started_at: Instant,
}

/// Default ring buffer capacity for query history.
const DEFAULT_MAX_HISTORY: usize = crate::config::DEFAULT_MAX_QUERY_HISTORY;

/// Convert an `Instant` elapsed time to milliseconds as u64.
/// Saturates at `u64::MAX` (which is ~584 million years, so... fine).
fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

impl Default for QueryTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryTracker {
    /// Create a new tracker with the default history capacity.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_HISTORY)
    }

    /// Create a new tracker with a custom history capacity.
    pub fn with_capacity(max_history: usize) -> Self {
        Self {
            active: DashMap::new(),
            history: Mutex::new(VecDeque::with_capacity(max_history)),
            max_history,
        }
    }

    /// Record the start of a query under a pool-allocated id
    /// (`ExecutorPool::allocate_query_id`).
    pub fn start(&self, id: u64, verified: &VerifiedKey, query: &str) {
        self.active.insert(
            id,
            ActiveQuery {
                id,
                key_id: verified.id,
                user: verified.name.clone(),
                role: verified.roles_display(),
                query: query.to_owned(),
                started_at: Instant::now(),
            },
        );
    }

    /// Record successful completion of a query.
    pub fn complete(&self, id: u64, rows: usize) {
        if let Some((_, active)) = self.active.remove(&id) {
            self.push_history(CompletedQuerySnapshot {
                id: active.id,
                user: active.user,
                query: active.query,
                duration_ms: elapsed_ms(active.started_at),
                rows: Some(rows),
                error: None,
                timed_out: false,
            });
        }
    }

    /// Record a failed query.
    pub fn fail(&self, id: u64, error: &str) {
        if let Some((_, active)) = self.active.remove(&id) {
            self.push_history(CompletedQuerySnapshot {
                id: active.id,
                user: active.user,
                query: active.query,
                duration_ms: elapsed_ms(active.started_at),
                rows: None,
                error: Some(error.to_owned()),
                timed_out: false,
            });
        }
    }

    /// Record a timed-out query.
    pub fn timeout(&self, id: u64) {
        if let Some((_, active)) = self.active.remove(&id) {
            self.push_history(CompletedQuerySnapshot {
                id: active.id,
                user: active.user,
                query: active.query,
                duration_ms: elapsed_ms(active.started_at),
                rows: None,
                error: Some("query timed out".to_owned()),
                timed_out: true,
            });
        }
    }

    /// Fleet keystore id of the key that started the given active query.
    ///
    /// `None` when the query is unknown or already finished — callers treat
    /// that as "not yours" (no information disclosure about live query ids).
    pub fn owner_key_id(&self, id: u64) -> Option<i64> {
        self.active.get(&id).map(|q| q.key_id)
    }

    /// Snapshot of all currently active queries.
    pub fn active(&self) -> Vec<ActiveQuerySnapshot> {
        self.active
            .iter()
            .map(|entry| {
                let q = entry.value();
                ActiveQuerySnapshot {
                    id: q.id,
                    user: q.user.clone(),
                    role: q.role.clone(),
                    query: q.query.clone(),
                    running_ms: elapsed_ms(q.started_at),
                }
            })
            .collect()
    }

    /// Recent completed queries (most recent first).
    pub fn recent(&self) -> Vec<CompletedQuerySnapshot> {
        let history = self.history.lock();
        history.iter().rev().cloned().collect()
    }

    /// Push a completed query into the ring buffer, evicting the oldest if full.
    fn push_history(&self, entry: CompletedQuerySnapshot) {
        let mut history = self.history.lock();
        if history.len() >= self.max_history {
            history.pop_front();
        }
        history.push_back(entry);
    }
}

#[cfg(test)]
mod tests {
    use fleet_auth::{PrincipalKind, Role, RolePermission};

    use super::*;

    fn test_key() -> VerifiedKey {
        VerifiedKey::from_roles(
            1,
            "testtest",
            "test-user",
            PrincipalKind::Human,
            vec![Role {
                id: 1,
                name: "trawl-analyst".into(),
                rate_rpm: None,
                permissions: vec![RolePermission {
                    app: "trawl".into(),
                    permission: "query".into(),
                }],
            }],
        )
    }

    #[test]
    fn start_and_complete() {
        let tracker = QueryTracker::new();
        let key = test_key();

        let id = 7;
        tracker.start(id, &key, "* | stats count()");
        assert_eq!(tracker.active().len(), 1);

        tracker.complete(id, 42);
        assert!(tracker.active().is_empty());

        let recent = tracker.recent();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].rows, Some(42));
        assert!(!recent[0].timed_out);
    }

    #[test]
    fn start_and_fail() {
        let tracker = QueryTracker::new();
        let key = test_key();

        let id = 7;
        tracker.start(id, &key, "bad query");
        tracker.fail(id, "parse error");

        let recent = tracker.recent();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].error.as_deref(), Some("parse error"));
    }

    #[test]
    fn start_and_timeout() {
        let tracker = QueryTracker::new();
        let key = test_key();

        let id = 7;
        tracker.start(id, &key, "slow query");
        tracker.timeout(id);

        let recent = tracker.recent();
        assert_eq!(recent.len(), 1);
        assert!(recent[0].timed_out);
    }

    #[test]
    fn owner_key_id_tracks_submitting_key() {
        let tracker = QueryTracker::new();
        let mut key = test_key();
        key.id = 42;

        let qid = 1;
        tracker.start(qid, &key, "*");
        assert_eq!(tracker.owner_key_id(qid), Some(42));

        // A different key id is not the owner — same name is irrelevant.
        let mut other = test_key();
        other.id = 43;
        let other_qid = 2;
        tracker.start(other_qid, &other, "*");
        assert_eq!(tracker.owner_key_id(other_qid), Some(43));

        // Finished or unknown queries have no owner.
        tracker.complete(qid, 0);
        assert_eq!(tracker.owner_key_id(qid), None);
        assert_eq!(tracker.owner_key_id(9999), None);
    }

    #[test]
    fn history_ring_buffer_evicts_oldest() {
        let tracker = QueryTracker {
            active: DashMap::new(),
            history: Mutex::new(VecDeque::with_capacity(3)),
            max_history: 3,
        };
        let key = test_key();

        for id in 1..=5 {
            tracker.start(id, &key, "*");
            tracker.complete(id, 1);
        }

        let recent = tracker.recent();
        assert_eq!(recent.len(), 3);
        // most recent first — IDs should be 5, 4, 3
        assert_eq!(recent[0].id, 5);
        assert_eq!(recent[2].id, 3);
    }
}
