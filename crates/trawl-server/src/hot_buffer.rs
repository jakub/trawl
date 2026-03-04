//! Hot buffer: batch-keyed in-memory event store for query freshness.
//!
//! Events land here via the event bus immediately after WAL write.
//! The buffer makes fresh events visible to ALL queries by providing
//! a temporary ndjson file that the executor can `UNION ALL BY NAME`
//! with the parquet source.
//!
//! Key invariant: a batch is in exactly one place at any time — hot
//! buffer xor parquet. Compaction marks batches as draining (via
//! [`mark_draining`](HotBuffer::mark_draining)) before writing
//! parquet, then calls [`drain`](HotBuffer::drain) after the write
//! completes. Snapshots skip draining batches, eliminating the
//! TOCTOU window where events could appear in both sources.

use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};

use crate::bus::{EventBus as _, EventSubscriber as _, IngestBatch, LocalEventBus, RecvError};

/// Configuration for the hot buffer.
#[derive(Debug, Clone)]
pub struct HotBufferConfig {
    /// Maximum number of events across all batches.
    pub max_events: usize,
    /// Maximum estimated memory usage in bytes (from ndjson byte sizes).
    ///
    /// This is the serialized ndjson byte count, which underestimates
    /// actual in-memory usage by ~3-5x (in-memory `Map<String, Value>`
    /// has allocator overhead, hash table buckets, `String` headers, etc.).
    /// Set this conservatively — e.g. to 1/4 of the actual memory budget
    /// you want to allocate for the hot buffer.
    pub max_bytes: usize,
}

/// Batch-keyed in-memory event store.
///
/// Uses an `IndexMap` for insertion-order iteration (oldest-first
/// FIFO eviction) keyed by batch ID (WAL filename stem).
pub struct HotBuffer {
    batches: RwLock<IndexMap<Arc<str>, Arc<IngestBatch>>>,
    total_events: AtomicUsize,
    total_bytes: AtomicUsize,
    config: HotBufferConfig,
    /// Monotonic counter bumped on every mutation (insert, drain, `mark_draining`).
    /// Used to invalidate the snapshot cache.
    generation: AtomicU64,
    /// Cached snapshot: `(generation, temp_file)`. Reused across concurrent
    /// queries when the buffer hasn't changed, avoiding O(events × queries)
    /// I/O. Old snapshots stay alive via Arc until all queries using them finish.
    snapshot_cache: Mutex<Option<(u64, Arc<tempfile::NamedTempFile>)>>,
}

impl std::fmt::Debug for HotBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HotBuffer")
            .field("total_events", &self.total_events.load(Ordering::Relaxed))
            .field("total_bytes", &self.total_bytes.load(Ordering::Relaxed))
            .field("generation", &self.generation.load(Ordering::Relaxed))
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl HotBuffer {
    /// Create a new hot buffer with the given limits.
    pub fn new(config: HotBufferConfig) -> Self {
        Self {
            batches: RwLock::new(IndexMap::new()),
            total_events: AtomicUsize::new(0),
            total_bytes: AtomicUsize::new(0),
            config,
            generation: AtomicU64::new(0),
            snapshot_cache: Mutex::new(None),
        }
    }

    /// Insert a batch into the buffer.
    ///
    /// If insertion would exceed either the event or byte limit,
    /// the oldest batches are evicted first (logged as warnings).
    pub fn insert(&self, batch: Arc<IngestBatch>) {
        let event_count = batch.events.len();
        let byte_count = batch.byte_size;

        // Evict oldest batches if over either limit.
        {
            let mut map = self.batches.write();
            while self.over_limit(event_count, byte_count) {
                if let Some((evicted_id, evicted)) = map.shift_remove_index(0) {
                    self.total_events
                        .fetch_sub(evicted.events.len(), Ordering::Relaxed);
                    self.total_bytes
                        .fetch_sub(evicted.byte_size, Ordering::Relaxed);
                    tracing::warn!(
                        event_type = "hot_buffer_eviction",
                        batch_id = %evicted_id,
                        events = evicted.events.len(),
                        bytes = evicted.byte_size,
                        "hot buffer evicted batch (over limit)"
                    );
                } else {
                    break;
                }
            }

            self.total_events.fetch_add(event_count, Ordering::Relaxed);
            self.total_bytes.fetch_add(byte_count, Ordering::Relaxed);
            map.insert(Arc::clone(&batch.batch_id), batch);
        }

        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Check if adding `extra_events` / `extra_bytes` would exceed limits.
    fn over_limit(&self, extra_events: usize, extra_bytes: usize) -> bool {
        self.total_events.load(Ordering::Relaxed) + extra_events > self.config.max_events
            || self.total_bytes.load(Ordering::Relaxed) + extra_bytes > self.config.max_bytes
    }

    /// Mark batches as draining before compaction writes parquet.
    ///
    /// Sets the `draining` flag on matching batches so that
    /// [`snapshot`](Self::snapshot) skips them.
    /// Uses a read lock only — `AtomicBool` provides interior mutability.
    pub fn mark_draining(&self, batch_ids: &[&str]) {
        let map = self.batches.read();
        for id in batch_ids {
            if let Some(batch) = map.get(*id) {
                batch
                    .draining
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Remove batches that have been compacted to parquet.
    ///
    /// Called after compaction writes parquet — the same batch IDs
    /// are drained from the hot buffer.
    pub fn drain(&self, batch_ids: &[&str]) {
        let mut map = self.batches.write();
        for id in batch_ids {
            if let Some(removed) = map.shift_remove(*id) {
                self.total_events
                    .fetch_sub(removed.events.len(), Ordering::Relaxed);
                self.total_bytes
                    .fetch_sub(removed.byte_size, Ordering::Relaxed);
            }
        }
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Get a snapshot of all non-draining buffered events as a temporary ndjson file.
    ///
    /// Returns `None` if the buffer is empty or all batches are draining.
    /// Uses a generation-based cache: concurrent queries against an unchanged
    /// buffer share a single snapshot file (1 disk write instead of N).
    /// The `Arc` ensures the temp file stays alive until all queries using it finish.
    ///
    /// The snapshot cache mutex is held for the entire build to serialize
    /// concurrent misses — one thread builds while others wait ~40ms and
    /// get the cached result, preventing thundering herd I/O.
    pub fn snapshot(&self) -> Option<Arc<tempfile::NamedTempFile>> {
        // Fast path: no events at all → skip locking entirely.
        if self.total_events.load(Ordering::Relaxed) == 0 {
            return None;
        }

        let current_gen = self.generation.load(Ordering::Relaxed);

        // Hold the mutex for the full check-then-build cycle to serialize
        // concurrent cache misses (only one thread builds).
        let mut cache = self.snapshot_cache.lock();

        if let Some((cached_gen, ref file)) = *cache
            && cached_gen == current_gen
        {
            return Some(Arc::clone(file));
        }

        // Cache miss — build under lock so concurrent queries wait.
        let snapshot = Arc::new(self.build_snapshot()?);
        *cache = Some((current_gen, Arc::clone(&snapshot)));

        Some(snapshot)
    }

    /// Build a fresh snapshot file from the current buffer contents.
    fn build_snapshot(&self) -> Option<tempfile::NamedTempFile> {
        let map = self.batches.read();
        if map.is_empty() {
            return None;
        }

        let mut tmpfile = tempfile::Builder::new().suffix(".ndjson").tempfile().ok()?;
        let mut wrote_any = false;

        for batch in map.values() {
            // Skip batches being drained by compaction.
            if batch.draining.load(Ordering::Relaxed) {
                continue;
            }

            for event in &batch.events {
                // Serialization failure here is very unlikely (we parsed it
                // successfully during ingest), but log and skip rather than
                // poisoning the entire snapshot.
                match serde_json::to_writer(&mut tmpfile, event) {
                    Ok(()) => {
                        if let Err(e) = tmpfile.write_all(b"\n") {
                            tracing::error!(event_type = "hot_buffer_error", error = %e, "hot buffer snapshot write failed");
                            return None;
                        }
                        wrote_any = true;
                    }
                    Err(e) => {
                        tracing::error!(
                            event_type = "hot_buffer_error",
                            batch_id = %batch.batch_id,
                            error = %e,
                            "failed to serialize event in hot buffer snapshot"
                        );
                    }
                }
            }
        }

        if !wrote_any {
            return None;
        }

        // Flush to ensure DuckDB can read the file.
        if let Err(e) = tmpfile.flush() {
            tracing::error!(event_type = "hot_buffer_error", error = %e, "hot buffer snapshot flush failed");
            return None;
        }

        Some(tmpfile)
    }

    /// Total number of events across all batches.
    pub fn event_count(&self) -> usize {
        self.total_events.load(Ordering::Relaxed)
    }

    /// Total estimated bytes across all batches.
    pub fn byte_count(&self) -> usize {
        self.total_bytes.load(Ordering::Relaxed)
    }

    /// Number of batches in the buffer.
    pub fn batch_count(&self) -> usize {
        self.batches.read().len()
    }

    /// Buffer configuration (max events, max bytes).
    pub fn config(&self) -> &HotBufferConfig {
        &self.config
    }
}

/// Spawn a background task that subscribes to the event bus and
/// inserts batches into the hot buffer.
///
/// The task runs until the bus is closed (server shutdown) or
/// the shutdown signal fires.
pub fn spawn_hot_buffer_consumer(
    bus: &Arc<LocalEventBus>,
    buffer: Arc<HotBuffer>,
    mut shutdown_rx: tokio::sync::watch::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    let mut subscriber = bus.subscribe();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = subscriber.recv() => {
                    match result {
                        Ok(batch) => {
                            buffer.insert(batch);
                        }
                        Err(RecvError::Lagged(n)) => {
                            tracing::warn!(
                                event_type = "hot_buffer_lag",
                                missed = n,
                                "hot buffer consumer lagged — \
                                 missed events are still in the WAL"
                            );
                        }
                        Err(RecvError::Closed) => {
                            tracing::info!(event_type = "hot_buffer_shutdown", "event bus closed, hot buffer consumer shutting down");
                            break;
                        }
                    }
                }
                _ = shutdown_rx.changed() => {
                    tracing::info!(event_type = "hot_buffer_shutdown", "hot buffer consumer received shutdown signal");
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;

    fn make_batch(id: &str, n: usize) -> Arc<IngestBatch> {
        let events: Vec<_> = (0..n)
            .map(|i| {
                let mut m = serde_json::Map::new();
                m.insert(
                    "message".into(),
                    serde_json::Value::String(format!("event_{i}")),
                );
                m.insert("service".into(), serde_json::Value::String("test".into()));
                m
            })
            .collect();
        Arc::new(IngestBatch {
            batch_id: id.into(),
            service: "test".into(),
            byte_size: n * 50, // rough estimate
            events,
            draining: AtomicBool::new(false),
        })
    }

    fn make_batch_with_bytes(id: &str, n: usize, byte_size: usize) -> Arc<IngestBatch> {
        let events: Vec<_> = (0..n)
            .map(|i| {
                let mut m = serde_json::Map::new();
                m.insert(
                    "message".into(),
                    serde_json::Value::String(format!("event_{i}")),
                );
                m.insert("service".into(), serde_json::Value::String("test".into()));
                m
            })
            .collect();
        Arc::new(IngestBatch {
            batch_id: id.into(),
            service: "test".into(),
            byte_size,
            events,
            draining: AtomicBool::new(false),
        })
    }

    #[test]
    fn insert_and_snapshot() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        buf.insert(make_batch("batch_001", 3));
        assert_eq!(buf.event_count(), 3);
        assert_eq!(buf.batch_count(), 1);

        let tmpfile = buf.snapshot().expect("should have events");
        let content = std::fs::read_to_string(tmpfile.path()).unwrap();
        assert_eq!(content.lines().count(), 3);
        assert!(content.contains("event_0"));
        assert!(content.contains("event_2"));
    }

    #[test]
    fn drain_removes_batches() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        buf.insert(make_batch("batch_001", 2));
        buf.insert(make_batch("batch_002", 3));
        assert_eq!(buf.event_count(), 5);

        buf.drain(&["batch_001"]);
        assert_eq!(buf.event_count(), 3);
        assert_eq!(buf.batch_count(), 1);

        buf.drain(&["batch_002"]);
        assert_eq!(buf.event_count(), 0);
        assert!(buf.snapshot().is_none());
    }

    #[test]
    fn eviction_removes_oldest_by_insertion_order() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 5,
            max_bytes: 10_000_000,
        });
        // Insert "zzz" first, then "aaa" — FIFO should evict "zzz" first
        // even though it sorts last lexicographically.
        buf.insert(make_batch("zzz_001", 3));
        buf.insert(make_batch("aaa_002", 3));
        // Inserting 3 more events when limit is 5: must evict zzz (oldest inserted).
        assert_eq!(buf.event_count(), 3);
        assert_eq!(buf.batch_count(), 1);

        // Only aaa_002 should remain.
        let tmpfile = buf.snapshot().unwrap();
        let content = std::fs::read_to_string(tmpfile.path()).unwrap();
        assert_eq!(content.lines().count(), 3);
    }

    #[test]
    fn eviction_by_bytes_limit() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1_000_000, // effectively unlimited
            max_bytes: 500,
        });
        buf.insert(make_batch_with_bytes("batch_001", 2, 300));
        buf.insert(make_batch_with_bytes("batch_002", 2, 300));
        // 300 + 300 = 600 > 500, so batch_001 should be evicted.
        assert_eq!(buf.event_count(), 2);
        assert_eq!(buf.byte_count(), 300);
        assert_eq!(buf.batch_count(), 1);
    }

    #[test]
    fn drain_updates_byte_count() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        buf.insert(make_batch_with_bytes("batch_001", 2, 200));
        buf.insert(make_batch_with_bytes("batch_002", 3, 400));
        assert_eq!(buf.byte_count(), 600);

        buf.drain(&["batch_001"]);
        assert_eq!(buf.byte_count(), 400);
    }

    #[test]
    fn mark_draining_excludes_from_snapshot() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        buf.insert(make_batch("batch_001", 2));
        buf.insert(make_batch("batch_002", 3));

        buf.mark_draining(&["batch_001"]);

        let tmpfile = buf.snapshot().expect("should have non-draining events");
        let content = std::fs::read_to_string(tmpfile.path()).unwrap();
        // Only batch_002's 3 events should be in the snapshot.
        assert_eq!(content.lines().count(), 3);
    }

    #[test]
    fn all_draining_returns_none() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        buf.insert(make_batch("batch_001", 2));
        buf.mark_draining(&["batch_001"]);

        assert!(buf.snapshot().is_none());
    }

    #[test]
    fn empty_buffer_returns_none() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 100,
            max_bytes: 10_000_000,
        });
        assert!(buf.snapshot().is_none());
    }

    #[test]
    fn drain_nonexistent_is_noop() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 100,
            max_bytes: 10_000_000,
        });
        buf.insert(make_batch("batch_001", 2));
        buf.drain(&["nonexistent"]);
        assert_eq!(buf.event_count(), 2);
    }

    #[tokio::test]
    async fn consumer_inserts_from_bus() {
        let bus = Arc::new(LocalEventBus::new(16));
        let buf = Arc::new(HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        }));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let handle = spawn_hot_buffer_consumer(&bus, Arc::clone(&buf), shutdown_rx);

        // Publish a batch.
        bus.publish(make_batch("test_001", 5));

        // Give the consumer task a moment to process.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(buf.event_count(), 5);
        assert_eq!(buf.batch_count(), 1);

        // Shut down.
        drop(shutdown_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
    }
}
