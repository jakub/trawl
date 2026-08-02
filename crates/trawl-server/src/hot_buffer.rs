// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hot buffer: batch-keyed in-memory event store for query freshness.
//!
//! Events land here via synchronous insertion during ingest/telemetry.
//! The buffer makes fresh events visible to ALL queries by providing
//! a temporary ndjson file that the executor can `UNION ALL BY NAME`
//! with the parquet source.
//!
//! Events stay in the hot buffer until compaction writes parquet and
//! calls [`drain`](HotBuffer::drain). During the brief window between
//! parquet write and drain, events may appear in both sources — this
//! is acceptable (transient overcount). Invisible events (missing from
//! both sources) are not acceptable.

use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};

use crate::bus::IngestBatch;

/// One buffered event, as stored in [`IngestBatch::events`].
type Event = serde_json::Map<String, serde_json::Value>;

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
    /// Monotonic counter bumped on every mutation (insert, drain).
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

    /// Get a snapshot of all buffered events as a temporary ndjson file.
    ///
    /// Returns `None` if the buffer is empty.
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
    ///
    /// Events are written schema-pioneers-first (see [`schema_pioneers`]) so
    /// that the reader can rely on `DuckDB`'s cheap default schema sample.
    fn build_snapshot(&self) -> Option<tempfile::NamedTempFile> {
        let map = self.batches.read();
        if map.is_empty() {
            return None;
        }

        let events: Vec<(&Arc<str>, &Event)> = map
            .values()
            .flat_map(|batch| batch.events.iter().map(move |e| (&batch.batch_id, e)))
            .collect();
        let pioneer = schema_pioneers(events.iter().map(|(_, e)| *e));

        let mut tmpfile = tempfile::Builder::new().suffix(".ndjson").tempfile().ok()?;
        let mut wrote_any = false;

        let order = (0..events.len())
            .filter(|&i| pioneer[i])
            .chain((0..events.len()).filter(|&i| !pioneer[i]));
        for (batch_id, event) in order.map(|i| events[i]) {
            // Coerce object/array values to their JSON text so the
            // snapshot's read_json infers them as VARCHAR (see
            // `coerce_complex_values`). Returns None when there is
            // nothing to coerce, avoiding a clone on the common path.
            let coerced = coerce_complex_values(event);
            let event = coerced.as_ref().unwrap_or(event);

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
                        batch_id = %batch_id,
                        error = %e,
                        "failed to serialize event in hot buffer snapshot"
                    );
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

/// Flag the events that introduce a key no earlier event carried — the
/// "schema pioneers" of the snapshot.
///
/// `DuckDB`'s `read_json` auto-detection infers the schema from a bounded
/// prefix of the file (~20480 records) and then hard-errors — `unknown key`
/// — on any later record carrying a key outside it. `_repairs` is
/// sparse by construction (only repaired events carry it, ADR-0008) and the
/// buffer holds up to `max_events` (100k by default), so one repaired event
/// past the prefix used to break every query touching the hot buffer.
///
/// Making the reader detect over the whole file (`sample_size=-1`) cures
/// that at the cost of re-parsing the entire snapshot on every query and SSE
/// poll: ~2.7x the read (+135ms measured on a full 100k-event / 100 MiB
/// buffer, ~1s at half a million records), growing with the configured
/// buffer size. Writing the pioneers first instead puts the complete key set inside the
/// detection prefix for the price of one pass over the buffer, and does it
/// with the events' real values: an always-emitted null placeholder column
/// would be inferred as JSON, so `_repairs` would come back quoted
/// and numeric fields would stop being numbers.
///
/// A homogeneous buffer has exactly one pioneer (the first event), so the
/// snapshot order is unchanged in the common case.
fn schema_pioneers<'a>(events: impl Iterator<Item = &'a Event>) -> Vec<bool> {
    let mut seen: std::collections::HashSet<&'a str> = std::collections::HashSet::new();
    events
        .map(|event| {
            let mut novel = false;
            for key in event.keys() {
                novel |= seen.insert(key.as_str());
            }
            novel
        })
        .collect()
}

/// Replace top-level object/array values in an event with their JSON-text
/// serialization, returning `None` when the event has no such values.
///
/// Compaction coerces complex-typed parquet columns (STRUCT/JSON/LIST) to
/// VARCHAR for a stable on-disk schema (see
/// `compaction::coerce_complex_columns_to_varchar`). The hot buffer must
/// match: if a field is an object here but VARCHAR in parquet, the
/// query-time `UNION ALL BY NAME` of the hot and cold sources hits a type
/// conflict and the executor silently drops the cold (parquet) rows.
/// Stringifying nested values makes the snapshot's `read_json` infer the
/// column as VARCHAR, keeping both sides aligned. Scalars are untouched.
fn coerce_complex_values(
    event: &serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    use serde_json::Value;

    if !event.values().any(|v| v.is_object() || v.is_array()) {
        return None;
    }

    let mut coerced = event.clone();
    for value in coerced.values_mut() {
        if value.is_object() || value.is_array() {
            *value = Value::String(value.to_string());
        }
    }
    Some(coerced)
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
            byte_size: n * 50, // rough estimate
            events,
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
    fn snapshot_stringifies_complex_values() {
        // An object-valued field must be serialized as a JSON string so the
        // snapshot's read_json infers it as VARCHAR — matching the parquet
        // side and avoiding a hot/cold union type conflict that would
        // silently drop cold rows. Scalars stay as-is.
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });

        let mut ev = serde_json::Map::new();
        ev.insert("service".into(), serde_json::Value::String("k".into()));
        ev.insert("count".into(), serde_json::Value::from(5));
        ev.insert(
            "containerID".into(),
            serde_json::json!({"id": "abc", "rt": "containerd"}),
        );
        buf.insert(Arc::new(IngestBatch {
            batch_id: "b1".into(),
            service: "k".into(),
            byte_size: 100,
            events: vec![ev],
        }));

        let tmpfile = buf.snapshot().expect("should have events");
        let content = std::fs::read_to_string(tmpfile.path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();

        assert!(
            parsed["containerID"].is_string(),
            "object field should be stringified: {content}"
        );
        assert!(
            parsed["containerID"].as_str().unwrap().contains("abc"),
            "stringified JSON should preserve content"
        );
        assert!(
            parsed["count"].is_number(),
            "scalar field must not be stringified"
        );
        assert!(parsed["service"].is_string());
    }

    /// Build `n` plain events plus one carrying the sparse `_repairs`
    /// key, with the sparse one last — the shape that used to break queries.
    fn events_with_trailing_sparse_key(n: usize) -> Vec<Event> {
        let mut events: Vec<Event> = (0..n)
            .map(|i| {
                let mut m = serde_json::Map::new();
                m.insert(
                    "_time".into(),
                    serde_json::Value::String("2024-01-15T10:00:00Z".into()),
                );
                m.insert("service".into(), serde_json::Value::String("svc".into()));
                m.insert("message".into(), serde_json::Value::String(format!("m{i}")));
                m
            })
            .collect();
        let mut repaired = serde_json::Map::new();
        repaired.insert(
            "_time".into(),
            serde_json::Value::String("2024-01-15T10:00:00Z".into()),
        );
        repaired.insert("service".into(), serde_json::Value::String("svc".into()));
        repaired.insert(
            "message".into(),
            serde_json::Value::String("repaired".into()),
        );
        repaired.insert(
            "_repairs".into(),
            serde_json::Value::String("time.from_ingest".into()),
        );
        events.push(repaired);
        events
    }

    #[test]
    fn snapshot_hoists_schema_pioneers_to_the_front() {
        // DuckDB infers the snapshot's schema from a bounded prefix and then
        // hard-errors on a later record with a key outside it. `_repairs`
        // is sparse by construction (ADR-0008), so the writer moves the events
        // that introduce a new key to the front — cheaper than making every
        // query re-detect over the whole file.
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        let events = events_with_trailing_sparse_key(50);
        buf.insert(Arc::new(IngestBatch {
            batch_id: "b1".into(),
            service: "svc".into(),
            byte_size: 100,
            events,
        }));

        let tmpfile = buf.snapshot().expect("should have events");
        let content = std::fs::read_to_string(tmpfile.path()).unwrap();
        let lines: Vec<&str> = content.lines().collect();

        assert_eq!(
            lines.len(),
            51,
            "hoisting must not drop or duplicate events"
        );
        let sparse_at = lines
            .iter()
            .position(|l| l.contains("_repairs"))
            .expect("the repaired event must still be in the snapshot");
        assert!(
            sparse_at < 2,
            "the only event carrying the sparse key must be hoisted into the \
             schema-detection prefix, found at line {sparse_at}"
        );
        let messages: std::collections::HashSet<String> = lines
            .iter()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["message"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(messages.len(), 51, "every event must survive reordering");
    }

    #[test]
    fn sparse_key_survives_a_snapshot_larger_than_the_detection_prefix() {
        // End-to-end: a buffer bigger than DuckDB's ~20480-record JSON sample
        // whose only repaired event is the last one inserted. Both hot reader
        // call sites are exercised — the hot-only reader (no parquet yet) and
        // the hot+cold union.
        use trawl_engine::executor::Executor;

        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 100_000,
            max_bytes: 100 * 1024 * 1024,
        });
        buf.insert(Arc::new(IngestBatch {
            batch_id: "b1".into(),
            service: "svc".into(),
            byte_size: 1000,
            events: events_with_trailing_sparse_key(30_000),
        }));
        let snapshot = buf.snapshot().expect("should have events");
        let hot = snapshot.path().to_str().unwrap();

        for with_cold in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            if with_cold {
                let conn = duckdb::Connection::open_in_memory().unwrap();
                conn.execute_batch(&format!(
                    "COPY (SELECT CAST('2024-01-15 10:00:00' AS TIMESTAMP) AS \"timestamp\", \
                                  'svc' AS service, 'cold row' AS message) \
                     TO '{}' (FORMAT PARQUET)",
                    dir.path().join("cold.parquet").display()
                ))
                .unwrap();
            }

            let exec = Executor::new().expect("executor should initialize");
            let source = format!("{}/*.parquet", dir.path().display());
            let result = exec
                .run_query_with_hot("*", &source, hot, usize::MAX, 0)
                .expect("hot query must succeed");

            let col_names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
            assert!(
                col_names.contains(&"_repairs"),
                "the preserved original must survive (with_cold={with_cold}); \
                 got columns {col_names:?}"
            );
        }
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
    fn snapshot_includes_all_batches() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        buf.insert(make_batch("batch_001", 2));
        buf.insert(make_batch("batch_002", 3));

        let tmpfile = buf.snapshot().expect("should have events");
        let content = std::fs::read_to_string(tmpfile.path()).unwrap();
        // All 5 events from both batches should be in the snapshot.
        assert_eq!(content.lines().count(), 5);
    }

    #[test]
    fn drain_after_snapshot_leaves_snapshot_valid() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        buf.insert(make_batch("batch_001", 2));
        buf.insert(make_batch("batch_002", 3));

        // Take a snapshot (Arc-wrapped temp file).
        let snapshot = buf.snapshot().expect("should have events");
        let content_before = std::fs::read_to_string(snapshot.path()).unwrap();
        assert_eq!(content_before.lines().count(), 5);

        // Drain batch_001 — simulates compaction finishing.
        buf.drain(&["batch_001"]);

        // The pre-drain snapshot file is still valid (Arc keeps it alive).
        let content_after = std::fs::read_to_string(snapshot.path()).unwrap();
        assert_eq!(content_after, content_before);
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
}
