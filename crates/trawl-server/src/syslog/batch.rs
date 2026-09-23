// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Time/count micro-batcher for syslog events.
//!
//! Accumulates individual syslog events and flushes them through the
//! WAL pipeline in batches, matching the pattern used by HTTP ingest.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use indexmap::IndexMap;
use serde_json::Map;
use tokio::sync::{mpsc, watch};

use crate::config::SyslogConfig;
use crate::ingest::pipeline::{BatchKey, PipelineWriter, ServiceBatch};
use crate::state::SyslogStats;

/// A single canonicalized syslog event ready for batching.
///
/// `env` and `service` are the door's own verdict (`Canonical::env` /
/// `Canonical::service`), not the listener's guess, and together they are
/// the batch key: two envs must never share a WAL file, a hot-buffer
/// drain key or a parquet partition (ADR-0009).
#[derive(Debug)]
pub struct SyslogEvent {
    /// The env the door filed this event under.
    pub env: String,
    /// The service the door validated.
    pub service: String,
    /// The canonicalized event map — the full declared envelope.
    pub map: Map<String, serde_json::Value>,
    /// Transport that received this event ("udp" or "tcp").
    pub transport: &'static str,
}

/// Sender half for submitting events to the batcher.
pub type SyslogSender = mpsc::Sender<SyslogEvent>;

/// The shared TCP/UDP queue boundary. Both full and closed queues abandon
/// exactly this event; transport-specific logging stays with the listener.
pub(super) fn try_enqueue(
    sender: &SyslogSender,
    event: SyslogEvent,
    stats: Option<&Arc<SyslogStats>>,
) -> bool {
    if sender.try_send(event).is_ok() {
        return true;
    }
    metrics::counter!(crate::metrics::SYSLOG_EVENTS_DROPPED_TOTAL).increment(1);
    if let Some(stats) = stats {
        stats.dropped.fetch_add(1, Ordering::Relaxed);
    }
    false
}

/// Accumulates syslog events and flushes to the ingest pipeline.
#[derive(Debug)]
pub struct SyslogBatcher {
    rx: mpsc::Receiver<SyslogEvent>,
    tx: mpsc::Sender<SyslogEvent>,
    pipeline: Arc<PipelineWriter>,
    batch_interval_ms: u64,
    batch_max_events: usize,
    stats: Option<Arc<SyslogStats>>,
}

impl SyslogBatcher {
    pub fn new(
        config: &SyslogConfig,
        pipeline: Arc<PipelineWriter>,
        stats: Option<Arc<SyslogStats>>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        Self {
            rx,
            tx,
            pipeline,
            batch_interval_ms: config.batch_interval_ms,
            batch_max_events: config.batch_max_events,
            stats,
        }
    }

    pub fn sender(&self) -> SyslogSender {
        self.tx.clone()
    }

    /// Run the batcher loop until shutdown.
    ///
    /// While a flush is in progress (awaiting `spawn_blocking`), the
    /// channel buffers incoming events (`channel_capacity`, 10k by
    /// default). Senders get back-pressure via `try_send` failures at the
    /// listener level.
    pub async fn run(mut self, mut shutdown_rx: watch::Receiver<bool>) {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(self.batch_interval_ms));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut pending: IndexMap<BatchKey, ServiceBatch> = IndexMap::new();
        let mut pending_count: usize = 0;
        let mut udp_count: u64 = 0;
        let mut tcp_count: u64 = 0;

        loop {
            tokio::select! {
                biased;

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
                        .entry((evt.env, evt.service))
                        .or_default()
                        .push(evt.map);
                    pending_count += 1;

                    if pending_count >= self.batch_max_events {
                        self.flush(&mut pending, &mut pending_count, udp_count, tcp_count).await;
                        udp_count = 0;
                        tcp_count = 0;
                    }
                }

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
        pending: &mut IndexMap<BatchKey, ServiceBatch>,
        pending_count: &mut usize,
        udp_count: u64,
        tcp_count: u64,
    ) {
        let batches = std::mem::take(pending);
        let groups = batches.len();
        let events = *pending_count;
        *pending_count = 0;

        let pipeline = Arc::clone(&self.pipeline);
        let written = tokio::task::spawn_blocking(move || pipeline.write(batches))
            .await
            .unwrap_or_else(|e| {
                // The task may have published earlier groups or the current
                // WAL. Count one uncertain task, never infer batch event loss.
                metrics::counter!(crate::metrics::SYSLOG_WRITE_TASKS_FAILED_TOTAL).increment(1);
                tracing::error!(
                    event_type = "syslog_flush_panic",
                    error = %crate::error::join_failure_text("syslog pipeline flush", e),
                    "syslog pipeline flush task panicked"
                );
                0
            });

        metrics::counter!(crate::metrics::INGEST_EVENTS_TOTAL).increment(written as u64);
        if udp_count > 0 {
            metrics::counter!(crate::metrics::SYSLOG_EVENTS_TOTAL, "transport" => "udp")
                .increment(udp_count);
        }
        if tcp_count > 0 {
            metrics::counter!(crate::metrics::SYSLOG_EVENTS_TOTAL, "transport" => "tcp")
                .increment(tcp_count);
        }

        if let Some(ref stats) = self.stats {
            if udp_count > 0 {
                stats.events_udp.fetch_add(udp_count, Ordering::Relaxed);
            }
            if tcp_count > 0 {
                stats.events_tcp.fetch_add(tcp_count, Ordering::Relaxed);
            }
        }

        tracing::debug!(
            event_type = "syslog_batch_flushed",
            groups,
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

    #[test]
    fn queue_full_and_closed_count_each_abandoned_event_once() {
        use crate::metrics::test_support::sample;
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let stats = Arc::new(SyslogStats::default());
            let (sender, mut receiver) = mpsc::channel(1);
            assert!(try_enqueue(&sender, make_event("accepted"), Some(&stats)));
            assert_eq!(
                sample(&handle, crate::metrics::SYSLOG_EVENTS_DROPPED_TOTAL),
                0
            );
            for transport in ["tcp", "udp"] {
                let mut event = make_event("full");
                event.transport = transport;
                assert!(!try_enqueue(&sender, event, Some(&stats)));
            }
            assert_eq!(
                sample(&handle, crate::metrics::SYSLOG_EVENTS_DROPPED_TOTAL),
                2
            );
            assert_eq!(receiver.try_recv().unwrap().service, "accepted");
            assert!(receiver.try_recv().is_err());
            drop(receiver);
            for transport in ["tcp", "udp"] {
                let mut event = make_event("closed");
                event.transport = transport;
                assert!(!try_enqueue(&sender, event, Some(&stats)));
            }
            assert_eq!(
                sample(&handle, crate::metrics::SYSLOG_EVENTS_DROPPED_TOTAL),
                4
            );
            assert_eq!(stats.dropped.load(Ordering::Relaxed), 4);
            assert_eq!(
                sample(&handle, crate::metrics::SYSLOG_WAL_EVENTS_DISCARDED_TOTAL),
                0
            );
        });
    }

    #[test]
    fn flush_task_failure_after_durable_groups_does_not_claim_event_loss() {
        use crate::metrics::test_support::sample;
        let handle = crate::metrics::prometheus_builder()
            .install_recorder()
            .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // Observe the parent and every blocking worker. Nextest isolates this
        // process-global recorder from the other tests' recorder installations.
        runtime.block_on(async {
            crate::metrics::init_operational_alert_metrics();
            // Confirm the blocking worker's discard metric reaches this handle.
            tokio::task::spawn_blocking(|| {
                metrics::counter!(crate::metrics::SYSLOG_WAL_EVENTS_DISCARDED_TOTAL).increment(1);
            })
            .await
            .unwrap();
            let discarded_before =
                sample(&handle, crate::metrics::SYSLOG_WAL_EVENTS_DISCARDED_TOTAL);
            assert_eq!(discarded_before, 1);
            let (batcher, tmp) = test_batcher(1000, 100);
            batcher.pipeline.wal_writer().panic_after_writes_for_test(2);
            let mut pending = IndexMap::new();
            for service in ["first", "second", "never_started"] {
                let event = make_event(service);
                let mut batch = ServiceBatch::default();
                batch.push(event.map);
                pending.insert((event.env, event.service), batch);
            }
            let mut count = 3;
            batcher.flush(&mut pending, &mut count, 3, 0).await;
            assert!(pending.is_empty());
            assert_eq!(count, 0);
            assert_eq!(
                sample(&handle, crate::metrics::SYSLOG_WRITE_TASKS_FAILED_TOTAL),
                1
            );
            assert_eq!(
                sample(&handle, crate::metrics::SYSLOG_WAL_EVENTS_DISCARDED_TOTAL),
                discarded_before
            );
            let mut services = Vec::new();
            for entry in std::fs::read_dir(tmp.path().join("prod")).unwrap() {
                let path = entry.unwrap().path();
                assert_eq!(path.extension().unwrap(), "ndjson");
                let event: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
                services.push(event["service"].as_str().unwrap().to_owned());
            }
            services.sort();
            assert_eq!(services, ["first", "second"]);
            batcher.flush(&mut pending, &mut count, 0, 0).await;
            assert_eq!(
                sample(&handle, crate::metrics::SYSLOG_WRITE_TASKS_FAILED_TOTAL),
                1
            );
        });
    }

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

        let batcher = SyslogBatcher::new(&config, pipeline, None);
        (batcher, tmp)
    }

    fn make_event(service: &str) -> SyslogEvent {
        make_event_in("prod", service)
    }

    fn make_event_in(env: &str, service: &str) -> SyslogEvent {
        let mut map = Map::new();
        map.insert("env".into(), json!(env));
        map.insert("service".into(), json!(service));
        map.insert("message".into(), json!("test"));
        SyslogEvent {
            env: env.to_owned(),
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

        sender.send(make_event("test-svc")).await.unwrap();

        // Wait for timer flush (~10ms + some margin)
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let _ = shutdown_tx.send(true);
        handle.await.unwrap();

        let files: Vec<_> = std::fs::read_dir(tmp.path().join("prod"))
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

        let files: Vec<_> = std::fs::read_dir(tmp.path().join("prod"))
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

        let _ = shutdown_tx.send(true);
        handle.await.unwrap();

        let files: Vec<_> = std::fs::read_dir(tmp.path().join("prod"))
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

    /// WAL files under one env dir, by service-name prefix.
    fn wal_files(root: &std::path::Path, env: &str) -> Vec<String> {
        std::fs::read_dir(root.join(env))
            .map(|dir| {
                dir.filter_map(Result::ok)
                    .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Drive the batcher to a flush and return once it has settled.
    async fn drain(batcher: SyslogBatcher, events: Vec<SyslogEvent>) {
        let sender = batcher.sender();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            batcher.run(shutdown_rx).await;
        });
        for event in events {
            sender.send(event).await.unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let _ = shutdown_tx.send(true);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn batcher_multi_service_grouping() {
        let (batcher, tmp) = test_batcher(60_000, 10_000);
        drain(
            batcher,
            vec![make_event("nginx"), make_event("sshd"), make_event("nginx")],
        )
        .await;

        // One WAL file per service, both under the one env.
        let files = wal_files(tmp.path(), "prod");
        assert_eq!(
            files.iter().filter(|f| f.starts_with("nginx")).count(),
            1,
            "should have one WAL file for nginx: {files:?}"
        );
        assert_eq!(
            files.iter().filter(|f| f.starts_with("sshd")).count(),
            1,
            "should have one WAL file for sshd: {files:?}"
        );
    }

    /// The batch key is `(env, service)`, not service alone: one service
    /// name arriving under two envs in one interval must not be
    /// concatenated into a single WAL file under a single path root, which
    /// would be a misfile no error and no repair code ever mentions.
    #[tokio::test]
    async fn batcher_keys_on_env_and_service_together() {
        let (batcher, tmp) = test_batcher(60_000, 10_000);
        drain(
            batcher,
            vec![
                make_event_in("prod", "nginx"),
                make_event_in("lab", "nginx"),
                make_event_in("prod", "nginx"),
            ],
        )
        .await;

        for env in ["prod", "lab"] {
            let files = wal_files(tmp.path(), env);
            assert_eq!(
                files.iter().filter(|f| f.starts_with("nginx")).count(),
                1,
                "env {env} must get its OWN nginx WAL file: {files:?}"
            );
        }

        // And the events did not cross: prod carries two, lab one.
        let counts: Vec<usize> = ["prod", "lab"]
            .iter()
            .map(|env| {
                let name = &wal_files(tmp.path(), env)[0];
                std::fs::read_to_string(tmp.path().join(env).join(name))
                    .unwrap()
                    .lines()
                    .count()
            })
            .collect();
        assert_eq!(counts, vec![2, 1], "events must not cross env boundaries");
    }
}
