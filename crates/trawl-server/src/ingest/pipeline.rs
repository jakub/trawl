// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared ingest pipeline: WAL write → hot buffer → event bus.
//!
//! This module provides the core pipeline logic that both the HTTP ingest
//! handler and the syslog listener use to durably store events and make
//! them immediately queryable.

use std::path::Path;
use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::Map;

use crate::bus::{EventBus as _, IngestBatch, LocalEventBus};
use crate::hot_buffer::HotBuffer;
use crate::ingest::wal::WalWriter;

/// The service-name rule (ADR-0009), re-exported from `trawl-config` so
/// that every ingestion path — HTTP ingest (which rejects violations),
/// syslog ingest (which maps into the charset) and config load (which
/// refuses to start) — decides from one definition.
pub use trawl_config::{MAX_SERVICE_NAME_LEN, is_valid_service_char, is_valid_service_name};

/// The batch key: `(env, service)`.
///
/// Two envs must never share a WAL batch, a hot-buffer drain key or a
/// parquet partition (ADR-0009), so the env is part of the key on EVERY
/// lane — the HTTP handler's and the syslog batcher's alike. It lives
/// here, next to the writer that consumes it, because a batcher holding
/// its own key type is exactly how the syslog lane came to file two envs
/// under one name.
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
        // Serialize to ndjson (newline-delimited JSON)
        if let Ok(line) = serde_json::to_vec(&map) {
            self.ndjson.extend_from_slice(&line);
            self.ndjson.push(b'\n');
        }
        self.maps.push(map);
    }
}

/// Shared pipeline writer that encapsulates WAL + hot buffer + event bus.
///
/// Shared pipeline writer that encapsulates WAL + hot buffer + event bus.
///
/// Used by both the syslog batcher and the HTTP ingest handler to write
/// events through the ingest pipeline.
#[derive(Debug)]
pub struct PipelineWriter {
    wal_writer: Arc<WalWriter>,
    hot_buffer: Option<Arc<HotBuffer>>,
    event_bus: Option<Arc<LocalEventBus>>,
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
        }
    }

    /// Access the underlying WAL writer.
    pub fn wal_writer(&self) -> &Arc<WalWriter> {
        &self.wal_writer
    }

    /// Write batches through the pipeline: WAL → hot buffer → event bus.
    ///
    /// Returns the number of events successfully written. Events from
    /// groups that fail WAL writing are dropped (logged, not published).
    ///
    /// The env comes from the KEY, never from a writer-held default: a
    /// batcher that grouped by service alone would file every env it
    /// received under one path root, silently. Deleting the field is what
    /// makes that unrepresentable rather than merely fixed.
    pub fn write(&self, batches: IndexMap<BatchKey, ServiceBatch>) -> usize {
        let mut total_written = 0;

        for ((env, svc), batch) in batches {
            let event_count = batch.maps.len();

            match self.wal_writer.write(&env, &svc, &batch.ndjson) {
                Ok(wal_path) => {
                    total_written += event_count;
                    self.publish(&env, &svc, batch, &wal_path);
                }
                Err(e) => {
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
    pub(crate) fn publish(&self, env: &str, svc: &str, batch: ServiceBatch, wal_path: &Path) {
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
}
