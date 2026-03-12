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

/// Channel capacity for the event queue between listeners and batcher.
const CHANNEL_CAPACITY: usize = 10_000;

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
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
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
                            self.flush(&mut pending, &mut pending_count, udp_count, tcp_count);
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
                            self.flush(&mut pending, &mut pending_count, udp_count, tcp_count);
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
                        self.flush(&mut pending, &mut pending_count, udp_count, tcp_count);
                        udp_count = 0;
                        tcp_count = 0;
                    }
                }

                // Timer tick — flush if we have pending events
                _ = interval.tick() => {
                    if pending_count > 0 {
                        self.flush(&mut pending, &mut pending_count, udp_count, tcp_count);
                        udp_count = 0;
                        tcp_count = 0;
                    }
                }
            }
        }
    }

    fn flush(
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

        let written = self.pipeline.write(batches);

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
