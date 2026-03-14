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

/// Maximum service name length (shared between HTTP and syslog ingest).
pub const MAX_SERVICE_NAME_LEN: usize = 128;

/// Check if a byte is valid in a service name.
///
/// Allows alphanumeric, dash, underscore, dot, and space.
/// Shared between HTTP ingest (which rejects invalid chars) and
/// syslog ingest (which strips them).
pub fn is_valid_service_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b' '
}

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
/// Used by both the syslog batcher and (indirectly) the HTTP ingest handler
/// to write events through the ingest pipeline.
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

    /// Write batches through the pipeline: WAL → hot buffer → event bus.
    ///
    /// Returns the number of events successfully written. Events from
    /// services that fail WAL writing are dropped (logged, not published).
    pub fn write(&self, batches: IndexMap<String, ServiceBatch>) -> usize {
        let mut total_written = 0;

        for (svc, batch) in batches {
            let event_count = batch.maps.len();

            match self.wal_writer.write(&svc, &batch.ndjson) {
                Ok(wal_path) => {
                    total_written += event_count;
                    self.publish(&svc, batch, &wal_path);
                }
                Err(e) => {
                    tracing::warn!(
                        event_type = "syslog_wal_write_failed",
                        service = %svc,
                        events_lost = event_count,
                        error = %e,
                        "WAL write failed for syslog batch"
                    );
                }
            }
        }

        total_written
    }

    /// Publish a successfully-written batch to hot buffer and event bus.
    fn publish(&self, svc: &str, batch: ServiceBatch, wal_path: &Path) {
        let batch_id: Arc<str> = wal_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
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
            tracing::debug!(service = %svc, subscribers, "published syslog batch to event bus");
        }
    }
}
