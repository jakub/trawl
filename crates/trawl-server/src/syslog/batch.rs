//! Time/count micro-batcher for syslog events.
//!
//! Accumulates individual syslog events and flushes them through the
//! WAL pipeline in batches, matching the pattern used by HTTP ingest.

use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::Map;
use tokio::sync::{mpsc, watch};

use crate::config::SyslogConfig;
use crate::ingest::pipeline::{PipelineWriter, ServiceBatch};

/// A single syslog event ready for batching.
#[derive(Debug)]
pub struct SyslogEvent {
    /// Derived service name (already sanitized/mapped).
    pub service: String,
    /// Full event map with all fields.
    pub map: Map<String, serde_json::Value>,
    /// Transport that received this event ("udp" or "tcp").
    pub transport: &'static str,
}

/// Sender half for submitting events to the batcher.
pub type SyslogSender = mpsc::Sender<SyslogEvent>;

/// Accumulates syslog events and flushes to the ingest pipeline.
#[derive(Debug)]
pub struct SyslogBatcher {
    rx: mpsc::Receiver<SyslogEvent>,
    tx: mpsc::Sender<SyslogEvent>,
    pipeline: Arc<PipelineWriter>,
    batch_interval_ms: u64,
    batch_max_events: usize,
}

impl SyslogBatcher {
    pub fn new(config: &SyslogConfig, pipeline: Arc<PipelineWriter>) -> Self {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        Self {
            rx,
            tx,
            pipeline,
            batch_interval_ms: config.batch_interval_ms,
            batch_max_events: config.batch_max_events,
        }
    }

    /// Get a sender for submitting events.
    pub fn sender(&self) -> SyslogSender {
        self.tx.clone()
    }

    /// Run the batcher loop until shutdown.
    ///
    /// While a flush is in progress (awaiting `spawn_blocking`), the
    /// channel buffers incoming events (capacity 10k). Senders get
    /// back-pressure via `try_send` failures at the listener level.
    pub async fn run(mut self, mut shutdown_rx: watch::Receiver<bool>) {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(self.batch_interval_ms));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut pending: IndexMap<String, ServiceBatch> = IndexMap::new();
        let mut pending_count: usize = 0;
        let mut udp_count: u64 = 0;
        let mut tcp_count: u64 = 0;

        loop {
            tokio::select! {
                biased;

                // Check shutdown
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        // Flush remaining events before exiting
                        if pending_count > 0 {
                            self.flush(&mut pending, &mut pending_count, udp_count, tcp_count).await;
                        }
                        tracing::info!(
                            event_type = "syslog_batcher_shutdown",
                            "syslog batcher shutting down"
                        );
                        return;
                    }
                }

                // Receive events from listeners
                event = self.rx.recv() => {
                    let Some(evt) = event else {
                        // All senders dropped — flush and exit
                        if pending_count > 0 {
                            self.flush(&mut pending, &mut pending_count, udp_count, tcp_count).await;
                        }
                        return;
                    };

                    if evt.transport == "udp" {
                        udp_count += 1;
                    } else if evt.transport == "tcp" {
                        tcp_count += 1;
                    }
                    pending
                        .entry(evt.service)
                        .or_default()
                        .push(evt.map);
                    pending_count += 1;

                    if pending_count >= self.batch_max_events {
                        self.flush(&mut pending, &mut pending_count, udp_count, tcp_count).await;
                        udp_count = 0;
                        tcp_count = 0;
                    }
                }

                // Timer tick — flush if we have pending events
                _ = interval.tick() => {
                    if pending_count > 0 {
                        self.flush(&mut pending, &mut pending_count, udp_count, tcp_count).await;
                        udp_count = 0;
                        tcp_count = 0;
                    }
                }
            }
        }
    }

    /// Flush pending batches through the pipeline in a blocking task.
    ///
    /// WAL writes (`std::fs::write` + `std::fs::rename`) are synchronous
    /// I/O, so we run them on the blocking thread pool to avoid stalling
    /// the tokio runtime.
    async fn flush(
        &self,
        pending: &mut IndexMap<String, ServiceBatch>,
        pending_count: &mut usize,
        udp_count: u64,
        tcp_count: u64,
    ) {
        let batches = std::mem::take(pending);
        let services = batches.len();
        let events = *pending_count;
        *pending_count = 0;

        let pipeline = Arc::clone(&self.pipeline);
        let written = tokio::task::spawn_blocking(move || pipeline.write(batches))
            .await
            .unwrap_or_else(|e| {
                tracing::error!(
                    event_type = "syslog_flush_panic",
                    error = %e,
                    "syslog pipeline flush task panicked"
                );
                0
            });

        // Update metrics
        metrics::counter!(crate::metrics::INGEST_EVENTS_TOTAL).increment(written as u64);
        if udp_count > 0 {
            metrics::counter!(crate::metrics::SYSLOG_EVENTS_TOTAL, "transport" => "udp")
                .increment(udp_count);
        }
        if tcp_count > 0 {
            metrics::counter!(crate::metrics::SYSLOG_EVENTS_TOTAL, "transport" => "tcp")
                .increment(tcp_count);
        }

        tracing::debug!(
            event_type = "syslog_batch_flushed",
            services,
            events,
            written,
            "flushed syslog batch to pipeline"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SyslogConfig;
    use crate::ingest::wal::WalWriter;
    use serde_json::json;

    /// Create a batcher with a real WAL writer pointing at a temp dir.
    fn test_batcher(
        batch_interval_ms: u64,
        batch_max_events: usize,
    ) -> (SyslogBatcher, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let wal = Arc::new(WalWriter::new(tmp.path().to_path_buf()));
        wal.ensure_dir().unwrap();
        let pipeline = Arc::new(PipelineWriter::new(wal, None, None));

        let config = SyslogConfig {
            batch_interval_ms,
            batch_max_events,
            ..SyslogConfig::default()
        };

        let batcher = SyslogBatcher::new(&config, pipeline);
        (batcher, tmp)
    }

    fn make_event(service: &str) -> SyslogEvent {
        let mut map = Map::new();
        map.insert("service".into(), json!(service));
        map.insert("message".into(), json!("test"));
        SyslogEvent {
            service: service.to_owned(),
            map,
            transport: "udp",
        }
    }

    #[tokio::test]
    async fn batcher_timer_flush() {
        let (batcher, tmp) = test_batcher(10, 1000); // 10ms interval, high max
        let sender = batcher.sender();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(async move {
            batcher.run(shutdown_rx).await;
        });

        // Send one event
        sender.send(make_event("test-svc")).await.unwrap();

        // Wait for timer flush (~10ms + some margin)
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Shut down and wait
        let _ = shutdown_tx.send(true);
        handle.await.unwrap();

        // Verify WAL file was created
        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();
        assert!(!files.is_empty(), "WAL file should have been created");
    }

    #[tokio::test]
    async fn batcher_max_events_flush() {
        let (batcher, tmp) = test_batcher(60_000, 3); // long interval, max 3 events
        let sender = batcher.sender();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(async move {
            batcher.run(shutdown_rx).await;
        });

        // Send exactly 3 events — should trigger immediate flush
        for _ in 0..3 {
            sender.send(make_event("test-svc")).await.unwrap();
        }

        // Give spawn_blocking time to complete
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let _ = shutdown_tx.send(true);
        handle.await.unwrap();

        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();
        assert!(
            !files.is_empty(),
            "WAL file should have been created by max events flush"
        );
    }

    #[tokio::test]
    async fn batcher_shutdown_flushes_remaining() {
        let (batcher, tmp) = test_batcher(60_000, 10_000); // long interval, high max
        let sender = batcher.sender();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(async move {
            batcher.run(shutdown_rx).await;
        });

        // Send events (won't trigger timer or max events flush)
        sender.send(make_event("svc-a")).await.unwrap();
        sender.send(make_event("svc-b")).await.unwrap();

        // Small delay to ensure events are received
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Shutdown should flush remaining events
        let _ = shutdown_tx.send(true);
        handle.await.unwrap();

        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();
        assert!(
            files.len() >= 2,
            "should have WAL files for both services, got {}",
            files.len()
        );
    }

    #[tokio::test]
    async fn batcher_multi_service_grouping() {
        let (batcher, tmp) = test_batcher(60_000, 10_000);
        let sender = batcher.sender();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(async move {
            batcher.run(shutdown_rx).await;
        });

        // Send events for different services
        sender.send(make_event("nginx")).await.unwrap();
        sender.send(make_event("sshd")).await.unwrap();
        sender.send(make_event("nginx")).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let _ = shutdown_tx.send(true);
        handle.await.unwrap();

        // Should have separate WAL files for each service
        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();

        let nginx_files = files
            .iter()
            .filter(|f| f.file_name().to_string_lossy().starts_with("nginx"))
            .count();
        let sshd_files = files
            .iter()
            .filter(|f| f.file_name().to_string_lossy().starts_with("sshd"))
            .count();
        assert_eq!(nginx_files, 1, "should have one WAL file for nginx");
        assert_eq!(sshd_files, 1, "should have one WAL file for sshd");
    }
}
