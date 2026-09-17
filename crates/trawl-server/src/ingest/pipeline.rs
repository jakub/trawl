// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared ingest pipeline: WAL write → hot buffer → event bus.
//!
//! Events are durable before they are visible: only a batch whose WAL write
//! succeeded reaches the hot buffer and the event bus.

use std::path::Path;
use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::Map;

use crate::bus::{EventBus as _, IngestBatch, LocalEventBus};
use crate::hot_buffer::HotBuffer;
use crate::ingest::wal::WalWriter;

/// The service-name rule (ADR-0009), re-exported from `trawl-config` so
/// that every ingestion path decides from one definition: HTTP ingest
/// rejects a violation, syslog ingest falls back to its configured default
/// service, and config load refuses to start.
pub use trawl_config::{MAX_SERVICE_NAME_LEN, is_valid_service_char, is_valid_service_name};

/// The batch key: `(env, service)`.
///
/// Two envs must never share a WAL batch, a hot-buffer drain key or a
/// parquet partition (ADR-0009), so the env is part of the key on every
/// lane, the HTTP handler's and the syslog batcher's alike. The type lives
/// next to the writer that consumes it: a batcher holding its own key type
/// is how a lane starts filing two envs under one name.
pub type BatchKey = (String, String);

/// Events for a single service within a batch, ready for WAL writing.
#[derive(Debug, Default)]
pub struct ServiceBatch {
    /// Parsed event maps (for hot buffer / event bus).
    pub maps: Vec<Map<String, serde_json::Value>>,
    /// Serialized ndjson bytes (for WAL file writing).
    pub ndjson: Vec<u8>,
}

impl ServiceBatch {
    /// Add an event to this batch, serializing it to ndjson in the process.
    pub fn push(&mut self, map: Map<String, serde_json::Value>) {
        if let Ok(line) = serde_json::to_vec(&map) {
            self.ndjson.extend_from_slice(&line);
            self.ndjson.push(b'\n');
        }
        self.maps.push(map);
    }
}

/// Shared pipeline writer that encapsulates WAL + hot buffer + event bus.
///
/// Used by both the syslog batcher and the HTTP ingest handler.
#[derive(Debug)]
pub struct PipelineWriter {
    wal_writer: Arc<WalWriter>,
    hot_buffer: Option<Arc<HotBuffer>>,
    event_bus: Option<Arc<LocalEventBus>>,
    #[cfg(any(test, feature = "test-support"))]
    pause_before_insert:
        parking_lot::Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

impl PipelineWriter {
    pub fn new(
        wal_writer: Arc<WalWriter>,
        hot_buffer: Option<Arc<HotBuffer>>,
        event_bus: Option<Arc<LocalEventBus>>,
    ) -> Self {
        Self {
            wal_writer,
            hot_buffer,
            event_bus,
            #[cfg(any(test, feature = "test-support"))]
            pause_before_insert: parking_lot::Mutex::new(None),
        }
    }

    pub fn wal_writer(&self) -> &Arc<WalWriter> {
        &self.wal_writer
    }

    /// Write batches through the pipeline: WAL → hot buffer → event bus.
    ///
    /// Returns the number of events successfully written. Events from
    /// groups that fail WAL writing are dropped (logged, not published).
    /// Syslog is the production caller of this method. HTTP writes groups
    /// separately so it can reject them; telemetry retains failed batches.
    ///
    /// The env comes from the key, never from a writer-held default: a
    /// batcher that grouped by service alone would file every env it
    /// received under one path root, silently.
    /// Call from a blocking thread because WAL I/O and the publication
    /// read guard can wait.
    pub fn write(&self, batches: IndexMap<BatchKey, ServiceBatch>) -> usize {
        let mut total_written = 0;

        for ((env, svc), batch) in batches {
            let event_count = batch.maps.len();
            // Compaction may read the WAL as soon as its rename completes.
            // Keep its publication and drain behind this hot insertion.
            let publication = self.hot_buffer.as_ref().map(|buf| buf.publication());
            let _ingest = publication.as_ref().map(|gate| gate.blocking_ingest());

            match self.wal_writer.write(&env, &svc, &batch.ndjson) {
                Ok(wal_path) => {
                    total_written += event_count;
                    self.publish(&env, &svc, batch, &wal_path);
                }
                Err(e) => {
                    metrics::counter!(crate::metrics::SYSLOG_WAL_EVENTS_DISCARDED_TOTAL)
                        .increment(event_count as u64);
                    tracing::warn!(
                        event_type = "pipeline_wal_write_failed",
                        batch_env = %env,
                        batch_service = %svc,
                        events_lost = event_count,
                        error = %e,
                        "WAL write failed for batch"
                    );
                }
            }
        }

        total_written
    }

    /// Publish a successfully-written batch to hot buffer and event bus.
    ///
    /// Called after WAL writing succeeds to make events immediately
    /// visible to queries (via hot buffer) and SSE streams (via event bus).
    /// The caller holds the publication read guard from before the WAL
    /// write through this call. Reacquiring here can deadlock behind a
    /// queued compactor waiting for the caller's existing read guard.
    pub(crate) fn publish(&self, env: &str, svc: &str, batch: ServiceBatch, wal_path: &Path) {
        #[cfg(any(test, feature = "test-support"))]
        {
            let pause = self.pause_before_insert.lock().take();
            if let Some((entered, release)) = pause {
                let _ = entered.send(());
                let _ = release.recv();
            }
        }
        // `{env}/{stem}`: two envs must never collide on a hot-buffer
        // drain key (compaction derives the same shape from the env dir).
        let batch_id: Arc<str> = format!(
            "{env}/{}",
            wal_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
        )
        .into();

        let ingest_batch = Arc::new(IngestBatch {
            batch_id,
            service: Arc::from(svc),
            byte_size: batch.ndjson.len(),
            events: batch.maps,
        });

        if let Some(buf) = &self.hot_buffer {
            buf.insert(Arc::clone(&ingest_batch));
        }
        if let Some(bus) = &self.event_bus {
            let subscribers = bus.publish(ingest_batch);
            tracing::debug!(
                batch_service = %svc,
                subscribers,
                "published batch to event bus"
            );
        }
    }

    /// Pause one durable batch before hot insertion on its blocking thread.
    /// Dropping the release sender also releases the pause.
    #[cfg(any(test, feature = "test-support"))]
    pub fn pause_next_insert_for_test(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *self.pause_before_insert.lock() = Some((entered_tx, release_rx));
        (entered_rx, release_tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::test_support::sample;

    #[test]
    fn failed_wal_group_discards_only_its_events_between_durable_groups() {
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let tmp = tempfile::tempdir().unwrap();
            let wal = Arc::new(WalWriter::new(tmp.path().join("wal")));
            wal.ensure_dir().unwrap();
            // A regular file in place of this env directory deterministically
            // fails only the middle group, including when tests run as root.
            std::fs::write(wal.dir().join("blocked"), b"keep me").unwrap();
            let hot = Arc::new(HotBuffer::new(crate::hot_buffer::HotBufferConfig {
                max_events: 100,
                max_bytes: 1024 * 1024,
            }));
            let pipeline = PipelineWriter::new(Arc::clone(&wal), Some(Arc::clone(&hot)), None);
            let mut batches = IndexMap::new();
            for (env, service, count) in [
                ("prod", "before", 1),
                ("blocked", "failed", 3),
                ("prod", "after", 2),
            ] {
                let mut batch = ServiceBatch::default();
                for id in 0..count {
                    batch.push(
                        serde_json::json!({"env":env, "service":service, "id":id})
                            .as_object()
                            .unwrap()
                            .clone(),
                    );
                }
                batches.insert((env.to_owned(), service.to_owned()), batch);
            }
            assert_eq!(pipeline.write(batches), 3);
            assert_eq!(
                sample(&handle, crate::metrics::SYSLOG_WAL_EVENTS_DISCARDED_TOTAL),
                3
            );
            assert_eq!(
                sample(&handle, crate::metrics::SYSLOG_WRITE_TASKS_FAILED_TOTAL),
                0
            );
            assert_eq!(hot.event_count(), 3);
            let mut events = Vec::new();
            for entry in std::fs::read_dir(wal.dir().join("prod")).unwrap() {
                let path = entry.unwrap().path();
                assert_eq!(path.extension().unwrap(), "ndjson");
                events.extend(
                    std::fs::read_to_string(path)
                        .unwrap()
                        .lines()
                        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()),
                );
            }
            assert_eq!(events.len(), 3);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event["service"] == "before")
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event["service"] == "after")
                    .count(),
                2
            );
            assert_eq!(
                std::fs::read(wal.dir().join("blocked")).unwrap(),
                b"keep me"
            );
            assert_eq!(pipeline.write(IndexMap::new()), 0);
            assert_eq!(
                sample(&handle, crate::metrics::SYSLOG_WAL_EVENTS_DISCARDED_TOTAL),
                3
            );
        });
    }
}
