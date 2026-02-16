//! Hot buffer: batch-keyed in-memory event store for query freshness.
//!
//! Events land here via the event bus immediately after WAL write.
//! The buffer makes fresh events visible to ALL queries by providing
//! a temporary ndjson file that the executor can `UNION ALL BY NAME`
//! with the parquet source.
//!
//! Key invariant: a batch is in exactly one place at any time — hot
//! buffer xor parquet. Compaction drains batches by their WAL filename
//! stem (batch ID), eliminating dedup concerns.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::RwLock;

use crate::bus::{EventBus as _, EventSubscriber as _, IngestBatch, LocalEventBus, RecvError};

/// Configuration for the hot buffer.
#[derive(Debug, Clone)]
pub struct HotBufferConfig {
    /// Maximum number of events across all batches.
    pub max_events: usize,
    /// Maximum estimated memory usage in bytes (rough heuristic).
    pub max_bytes: usize,
}

/// Batch-keyed in-memory event store.
///
/// Uses a `BTreeMap` for deterministic iteration order (oldest-first
/// eviction) keyed by batch ID (WAL filename stem).
#[derive(Debug)]
pub struct HotBuffer {
    batches: RwLock<BTreeMap<Arc<str>, Arc<IngestBatch>>>,
    total_events: AtomicUsize,
    config: HotBufferConfig,
}

impl HotBuffer {
    /// Create a new hot buffer with the given limits.
    pub fn new(config: HotBufferConfig) -> Self {
        Self {
            batches: RwLock::new(BTreeMap::new()),
            total_events: AtomicUsize::new(0),
            config,
        }
    }

    /// Insert a batch into the buffer.
    ///
    /// If insertion would exceed limits, the oldest batches are evicted
    /// first (logged as warnings).
    pub fn insert(&self, batch: Arc<IngestBatch>) {
        let event_count = batch.events.len();

        // Evict oldest batches if over the event limit.
        {
            let mut map = self.batches.write();
            while self.total_events.load(Ordering::Relaxed) + event_count > self.config.max_events {
                if let Some((evicted_id, evicted)) = map.pop_first() {
                    self.total_events
                        .fetch_sub(evicted.events.len(), Ordering::Relaxed);
                    tracing::warn!(
                        batch_id = %evicted_id,
                        events = evicted.events.len(),
                        "hot buffer evicted batch (over event limit)"
                    );
                } else {
                    break;
                }
            }

            self.total_events.fetch_add(event_count, Ordering::Relaxed);
            map.insert(Arc::clone(&batch.batch_id), batch);
        }
    }

    /// Remove batches that have been compacted to parquet.
    ///
    /// Called after compaction deletes WAL files — the same batch IDs
    /// are drained from the hot buffer.
    pub fn drain(&self, batch_ids: &[&str]) {
        let mut map = self.batches.write();
        for id in batch_ids {
            if let Some(removed) = map.remove(*id) {
                self.total_events
                    .fetch_sub(removed.events.len(), Ordering::Relaxed);
            }
        }
    }

    /// Write all buffered events to a temporary ndjson file.
    ///
    /// Returns `None` if the buffer is empty. The returned `NamedTempFile`
    /// auto-deletes on drop, so the caller must hold it alive for the
    /// duration of the query.
    pub fn snapshot_to_tempfile(&self) -> Option<tempfile::NamedTempFile> {
        let map = self.batches.read();
        if map.is_empty() {
            return None;
        }

        let mut tmpfile = tempfile::Builder::new().suffix(".ndjson").tempfile().ok()?;

        for batch in map.values() {
            for event in &batch.events {
                // Serialization failure here is very unlikely (we parsed it
                // successfully during ingest), but log and skip rather than
                // poisoning the entire snapshot.
                match serde_json::to_writer(&mut tmpfile, event) {
                    Ok(()) => {
                        let _ = tmpfile.write_all(b"\n");
                    }
                    Err(e) => {
                        tracing::error!(
                            batch_id = %batch.batch_id,
                            error = %e,
                            "failed to serialize event in hot buffer snapshot"
                        );
                    }
                }
            }
        }

        // Flush to ensure DuckDB can read the file.
        let _ = tmpfile.flush();

        Some(tmpfile)
    }

    /// Total number of events across all batches.
    pub fn event_count(&self) -> usize {
        self.total_events.load(Ordering::Relaxed)
    }

    /// Number of batches in the buffer.
    pub fn batch_count(&self) -> usize {
        self.batches.read().len()
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
                                missed = n,
                                "hot buffer consumer lagged — \
                                 missed events are still in the WAL"
                            );
                        }
                        Err(RecvError::Closed) => {
                            tracing::info!("event bus closed, hot buffer consumer shutting down");
                            break;
                        }
                    }
                }
                _ = shutdown_rx.changed() => {
                    tracing::info!("hot buffer consumer received shutdown signal");
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
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
            events,
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

        let tmpfile = buf.snapshot_to_tempfile().expect("should have events");
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
        assert!(buf.snapshot_to_tempfile().is_none());
    }

    #[test]
    fn eviction_removes_oldest() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 5,
            max_bytes: 10_000_000,
        });
        buf.insert(make_batch("aaa_001", 3));
        buf.insert(make_batch("bbb_002", 3));
        // Inserting 3 more events when we already have 3 (aaa evicted, bbb stays)
        // should evict aaa_001 (oldest per BTreeMap order).
        assert_eq!(buf.event_count(), 3);
        assert_eq!(buf.batch_count(), 1);

        // Only bbb_002 should remain.
        let tmpfile = buf.snapshot_to_tempfile().unwrap();
        let content = std::fs::read_to_string(tmpfile.path()).unwrap();
        assert_eq!(content.lines().count(), 3);
    }

    #[test]
    fn empty_buffer_returns_none() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 100,
            max_bytes: 10_000_000,
        });
        assert!(buf.snapshot_to_tempfile().is_none());
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
