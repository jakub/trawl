//! Query lifecycle tracking — active queries, history ring buffer, and timeout recording.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use dashmap::DashMap;
use fleet_auth::keys::VerifiedKey;
use serde::Serialize;

/// Tracks active and recently completed queries.
#[derive(Debug)]
pub struct QueryTracker {
    active: DashMap<u64, ActiveQuery>,
    history: Mutex<VecDeque<CompletedQuery>>,
    next_id: AtomicU64,
    max_history: usize,
}

/// A currently executing query.
#[derive(Debug, Clone)]
pub struct ActiveQuery {
    /// Monotonic query ID.
    pub id: u64,
    /// Authenticated user name.
    pub user: String,
    /// User's role.
    pub role: String,
    /// The DSL query string.
    pub query: String,
    /// When execution started.
    pub started_at: Instant,
}

/// A completed (or failed/timed-out) query.
#[derive(Debug, Clone, Serialize)]
pub struct CompletedQuery {
    /// Monotonic query ID.
    pub id: u64,
    /// Authenticated user name.
    pub user: String,
    /// The DSL query string.
    pub query: String,
    /// Execution duration in milliseconds.
    pub duration_ms: u64,
    /// Row count (if successful).
    pub rows: Option<usize>,
    /// Error message (if failed).
    pub error: Option<String>,
    /// Whether the query exceeded the timeout.
    pub timed_out: bool,
}

/// Serializable snapshot of an active query (for the API response).
#[derive(Debug, Clone, Serialize)]
pub struct ActiveQuerySnapshot {
    pub id: u64,
    pub user: String,
    pub role: String,
    pub query: String,
    pub running_ms: u64,
}

/// Default ring buffer capacity for query history.
const DEFAULT_MAX_HISTORY: usize = 1000;

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
        Self {
            active: DashMap::new(),
            history: Mutex::new(VecDeque::with_capacity(DEFAULT_MAX_HISTORY)),
            next_id: AtomicU64::new(1),
            max_history: DEFAULT_MAX_HISTORY,
        }
    }

    /// Record the start of a query. Returns a query ID for later completion.
    pub fn start(&self, verified: &VerifiedKey, query: &str) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.active.insert(
            id,
            ActiveQuery {
                id,
                user: verified.name.clone(),
                role: verified.role.to_string(),
                query: query.to_owned(),
                started_at: Instant::now(),
            },
        );
        id
    }

    /// Record successful completion of a query.
    pub fn complete(&self, id: u64, rows: usize) {
        if let Some((_, active)) = self.active.remove(&id) {
            self.push_history(CompletedQuery {
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
            self.push_history(CompletedQuery {
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
            self.push_history(CompletedQuery {
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
    pub fn recent(&self) -> Vec<CompletedQuery> {
        let history = self.history.lock().expect("history lock poisoned");
        history.iter().rev().cloned().collect()
    }

    /// Push a completed query into the ring buffer, evicting the oldest if full.
    fn push_history(&self, entry: CompletedQuery) {
        let mut history = self.history.lock().expect("history lock poisoned");
        if history.len() >= self.max_history {
            history.pop_front();
        }
        history.push_back(entry);
    }
}

#[cfg(test)]
mod tests {
    use fleet_auth::roles::Role;

    use super::*;

    fn test_key() -> VerifiedKey {
        VerifiedKey {
            id: 1,
            prefix: "testtest".into(),
            name: "test-user".into(),
            role: Role::Analyst,
        }
    }

    #[test]
    fn start_and_complete() {
        let tracker = QueryTracker::new();
        let key = test_key();

        let id = tracker.start(&key, "* | stats count()");
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

        let id = tracker.start(&key, "bad query");
        tracker.fail(id, "parse error");

        let recent = tracker.recent();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].error.as_deref(), Some("parse error"));
    }

    #[test]
    fn start_and_timeout() {
        let tracker = QueryTracker::new();
        let key = test_key();

        let id = tracker.start(&key, "slow query");
        tracker.timeout(id);

        let recent = tracker.recent();
        assert_eq!(recent.len(), 1);
        assert!(recent[0].timed_out);
    }

    #[test]
    fn history_ring_buffer_evicts_oldest() {
        let tracker = QueryTracker {
            active: DashMap::new(),
            history: Mutex::new(VecDeque::with_capacity(3)),
            next_id: AtomicU64::new(1),
            max_history: 3,
        };
        let key = test_key();

        for _ in 0..5 {
            let id = tracker.start(&key, "*");
            tracker.complete(id, 1);
        }

        let recent = tracker.recent();
        assert_eq!(recent.len(), 3);
        // most recent first — IDs should be 5, 4, 3
        assert_eq!(recent[0].id, 5);
        assert_eq!(recent[2].id, 3);
    }
}
