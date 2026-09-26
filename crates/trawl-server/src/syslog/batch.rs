// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Time/count micro-batcher for syslog events.
//!
//! Accumulates individual syslog events and flushes them through the
//! WAL pipeline in batches, matching the pattern used by HTTP ingest.
//!
//! # Backpressure (ADR-0043)
//!
//! Every flushed group is admitted against the hot buffer before it is
//! written. A group the buffer refuses as full stays at the front of the
//! batcher's queue, with every group behind it, and the batcher stops
//! taking events from its channel until charge is released. The channel
//! then fills: a TCP listener waits in [`SyslogSender::send`], holding its
//! connection open, and a UDP listener drops the datagram, counted as
//! `backpressure` rather than `queue_full` while the batcher is blocked.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::Map;
use tokio::sync::{mpsc, watch};

use crate::config::SyslogConfig;
use crate::hot_buffer::{Charge, Refusal};
use crate::ingest::pipeline::{AdmittedGroup, BatchKey, PipelineWriter, ServiceBatch};
use crate::ingest::producer::ProducerKind;
use crate::metrics::SyslogDropReason;
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

/// Sender half for submitting events to the batcher: the shared TCP/UDP
/// queue boundary.
///
/// `backpressure` is set by the batcher while hot-buffer admission refuses
/// its front group and it has stopped receiving, so a full queue can be
/// told apart from one that is merely slow.
#[derive(Debug, Clone)]
pub struct SyslogSender {
    tx: mpsc::Sender<SyslogEvent>,
    backpressure: Arc<AtomicBool>,
}

impl SyslogSender {
    /// Enqueue without waiting (UDP). A full or closed queue abandons
    /// exactly this event, counted once; transport-specific logging stays
    /// with the listener.
    pub(super) fn try_enqueue(&self, event: SyslogEvent, stats: Option<&Arc<SyslogStats>>) -> bool {
        match self.tx.try_send(event) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                record_dropped(self.full_reason(), 1, stats);
                false
            }
            // A closed queue is lumped with a full one: the batcher is gone.
            Err(mpsc::error::TrySendError::Closed(_)) => {
                record_dropped(SyslogDropReason::QueueFull, 1, stats);
                false
            }
        }
    }

    /// Enqueue, waiting for room (TCP). The wait is the backpressure a TCP
    /// sender feels: the connection stays open and unread while the queue
    /// is full. Returns `false`, counting the event dropped, only if the
    /// batcher has gone away.
    ///
    /// Cancel-safe in the sense the listener needs: dropping the future
    /// abandons only this event, which the caller then counts with
    /// [`abandon`](Self::abandon).
    pub(super) async fn send(&self, event: SyslogEvent, stats: Option<&Arc<SyslogStats>>) -> bool {
        if self.tx.send(event).await.is_ok() {
            return true;
        }
        record_dropped(SyslogDropReason::QueueFull, 1, stats);
        false
    }

    /// Count one event a listener gave up on while waiting for room (a TCP
    /// send cancelled by shutdown).
    pub(super) fn abandon(&self, stats: Option<&Arc<SyslogStats>>) {
        record_dropped(self.full_reason(), 1, stats);
    }

    /// Why a full queue is full, sampled once per drop.
    fn full_reason(&self) -> SyslogDropReason {
        if self.backpressure.load(Ordering::Acquire) {
            SyslogDropReason::Backpressure
        } else {
            SyslogDropReason::QueueFull
        }
    }
}

#[cfg(test)]
impl SyslogSender {
    /// A sender on a bare channel, with no batcher behind it.
    pub(super) fn channel_for_test(capacity: usize) -> (Self, mpsc::Receiver<SyslogEvent>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            Self {
                tx,
                backpressure: Arc::new(AtomicBool::new(false)),
            },
            rx,
        )
    }

    pub(super) fn backpressure_for_test(&self) -> bool {
        self.backpressure.load(Ordering::Acquire)
    }

    pub(super) fn set_backpressure_for_test(&self, blocked: bool) {
        self.backpressure.store(blocked, Ordering::Release);
    }
}

fn record_dropped(reason: SyslogDropReason, events: u64, stats: Option<&Arc<SyslogStats>>) {
    metrics::counter!(crate::metrics::SYSLOG_EVENTS_DROPPED_TOTAL, "reason" => reason.label())
        .increment(events);
    if let Some(stats) = stats {
        stats.dropped.fetch_add(events, Ordering::Relaxed);
    }
}

/// Split `batch` at event boundaries into chunks that each fit `ceiling`
/// on both dimensions, keeping event order. An event that alone exceeds
/// the ceiling becomes a chunk of its own, which admission then refuses
/// as [`Refusal::Oversized`].
fn split_to_fit(batch: ServiceBatch, ceiling: Charge) -> Vec<ServiceBatch> {
    if batch.charge().fits(ceiling) {
        return vec![batch];
    }
    let ServiceBatch { maps, ndjson } = batch;
    // `ServiceBatch::push` keeps one map per line, and a serialized JSON
    // object never contains a raw newline, so the lines pair with the maps.
    let mut lines = ndjson.split_inclusive(|byte| *byte == b'\n');
    let mut chunks = Vec::new();
    let mut current = ServiceBatch::default();
    for map in maps {
        let line = lines.next().expect("one ndjson line per map");
        let grown = current.charge().checked_add(Charge {
            events: 1,
            bytes: line.len(),
        });
        if !current.maps.is_empty() && !grown.is_some_and(|charge| charge.fits(ceiling)) {
            chunks.push(std::mem::take(&mut current));
        }
        current.maps.push(map);
        current.ndjson.extend_from_slice(line);
    }
    if !current.maps.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Accumulates syslog events and flushes to the ingest pipeline.
#[derive(Debug)]
pub struct SyslogBatcher {
    rx: mpsc::Receiver<SyslogEvent>,
    sender: SyslogSender,
    pipeline: Arc<PipelineWriter>,
    /// Advances whenever hot-buffer charge is released: the wake for a
    /// blocked batcher. `None` without a hot buffer, which never refuses.
    released: Option<watch::Receiver<u64>>,
    /// The retry poll while blocked, in case a release is not observed
    /// (`compaction_interval_secs`).
    blocked_poll: Duration,
    /// Flushed groups not yet admitted, in first-seen order. Non-empty
    /// only while admission refuses the front group.
    queue: VecDeque<(BatchKey, ServiceBatch)>,
    batch_interval_ms: u64,
    batch_max_events: usize,
    stats: Option<Arc<SyslogStats>>,
}

impl SyslogBatcher {
    /// `blocked_poll` bounds how long a batcher blocked on hot-buffer
    /// admission waits between retries when no release wakes it.
    pub fn new(
        config: &SyslogConfig,
        pipeline: Arc<PipelineWriter>,
        blocked_poll: Duration,
        stats: Option<Arc<SyslogStats>>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let released = pipeline.subscribe_released();
        Self {
            rx,
            sender: SyslogSender {
                tx,
                backpressure: Arc::new(AtomicBool::new(false)),
            },
            pipeline,
            released,
            blocked_poll,
            queue: VecDeque::new(),
            batch_interval_ms: config.batch_interval_ms,
            batch_max_events: config.batch_max_events,
            stats,
        }
    }

    pub fn sender(&self) -> SyslogSender {
        self.sender.clone()
    }

    /// Run the batcher loop until shutdown.
    ///
    /// While a flush is in progress (awaiting `spawn_blocking`), the
    /// channel buffers incoming events (`channel_capacity`, 10k by
    /// default). While hot-buffer admission refuses the front group, the
    /// batcher stops receiving altogether and waits for a release, a
    /// shutdown, or the `blocked_poll` fallback; the batch interval does
    /// not tick in that state.
    pub async fn run(mut self, mut shutdown_rx: watch::Receiver<bool>) {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(self.batch_interval_ms));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut pending: IndexMap<BatchKey, ServiceBatch> = IndexMap::new();
        let mut pending_count: usize = 0;
        let mut udp_count: u64 = 0;
        let mut tcp_count: u64 = 0;

        loop {
            if !self.queue.is_empty() {
                if self.wait_while_blocked(&mut shutdown_rx).await {
                    self.shut_down(&mut pending, &mut pending_count, udp_count, tcp_count)
                        .await;
                    return;
                }
                self.admit_and_write().await;
                continue;
            }

            tokio::select! {
                biased;

                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        self.shut_down(&mut pending, &mut pending_count, udp_count, tcp_count)
                            .await;
                        return;
                    }
                }

                event = self.rx.recv() => {
                    let Some(evt) = event else {
                        // All senders dropped — flush and exit
                        if pending_count > 0 {
                            self.flush(&mut pending, &mut pending_count, udp_count, tcp_count).await;
                        }
                        self.drop_refused();
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

    /// Wait, while admission refuses the front group, for a release, the
    /// fallback poll, or shutdown. Returns `true` on shutdown.
    ///
    /// The released generation was marked seen before the refused attempt
    /// (in [`admit_and_write`](Self::admit_and_write)), so a release that
    /// raced the refusal still wakes this wait.
    async fn wait_while_blocked(&mut self, shutdown_rx: &mut watch::Receiver<bool>) -> bool {
        let poll = self.blocked_poll;
        let released_rx = self.released.as_mut();
        let released = async move {
            match released_rx {
                // A closed sender means the buffer is gone: only the
                // fallback poll can still wake us.
                Some(rx) => {
                    if rx.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            biased;

            changed = shutdown_rx.changed() => changed.is_err() || *shutdown_rx.borrow(),
            () = released => false,
            () = tokio::time::sleep(poll) => false,
        }
    }

    /// Stop: take whatever is still queued in the channel, make one final
    /// ordered attempt at everything pending, and count what admission
    /// still refuses as dropped for backpressure.
    async fn shut_down(
        &mut self,
        pending: &mut IndexMap<BatchKey, ServiceBatch>,
        pending_count: &mut usize,
        mut udp_count: u64,
        mut tcp_count: u64,
    ) {
        self.rx.close();
        while let Ok(evt) = self.rx.try_recv() {
            if evt.transport == "udp" {
                udp_count += 1;
            } else if evt.transport == "tcp" {
                tcp_count += 1;
            }
            pending
                .entry((evt.env, evt.service))
                .or_default()
                .push(evt.map);
            *pending_count += 1;
        }
        if *pending_count > 0 {
            self.flush(pending, pending_count, udp_count, tcp_count)
                .await;
        } else if !self.queue.is_empty() {
            self.admit_and_write().await;
        }
        self.drop_refused();
        tracing::info!(
            event_type = "syslog_batcher_shutdown",
            "syslog batcher shutting down"
        );
    }

    /// Count everything still refused as dropped for backpressure.
    fn drop_refused(&mut self) {
        let refused: usize = self
            .queue
            .drain(..)
            .map(|(_, batch)| batch.maps.len())
            .sum();
        if refused > 0 {
            record_dropped(
                SyslogDropReason::Backpressure,
                refused as u64,
                self.stats.as_ref(),
            );
            tracing::warn!(
                event_type = "syslog_backpressure_shutdown_drop",
                events = refused,
                "syslog events still refused by hot-buffer admission at shutdown were dropped"
            );
        }
    }

    /// Move pending batches behind the admission queue, split to fit the
    /// syslog ceiling, and admit and write what the hot buffer accepts.
    async fn flush(
        &mut self,
        pending: &mut IndexMap<BatchKey, ServiceBatch>,
        pending_count: &mut usize,
        udp_count: u64,
        tcp_count: u64,
    ) {
        let batches = std::mem::take(pending);
        *pending_count = 0;

        let ceiling = self.pipeline.ceiling(ProducerKind::Syslog);
        for (key, batch) in batches {
            for chunk in split_to_fit(batch, ceiling) {
                self.queue.push_back((key.clone(), chunk));
            }
        }

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

        self.admit_and_write().await;
    }

    /// Reserve hot-buffer space for queued groups front first, then write
    /// the admitted ones. A `Full` refusal stops at that group, keeping it
    /// and everything behind it, and raises the backpressure flag; a queue
    /// admitted to the end clears it.
    ///
    /// The released generation is marked seen before the first attempt, so
    /// a release after a refusal is never missed by the blocked wait.
    async fn admit_and_write(&mut self) {
        if let Some(rx) = self.released.as_mut() {
            rx.borrow_and_update();
        }
        let mut admitted = Vec::new();
        let mut blocked = false;
        while let Some((key, batch)) = self.queue.pop_front() {
            match self.pipeline.reserve(ProducerKind::Syslog, batch.charge()) {
                Ok(reservation) => admitted.push(AdmittedGroup {
                    key,
                    batch,
                    reservation,
                }),
                Err(Refusal::Full) => {
                    self.queue.push_front((key, batch));
                    blocked = true;
                    break;
                }
                // Only a lone event larger than the ceiling reaches here:
                // `split_to_fit` sized every other chunk to fit. `reserve`
                // has counted the refusal.
                Err(Refusal::Oversized) => {
                    let events = batch.maps.len();
                    if let Some(ref stats) = self.stats {
                        stats.dropped.fetch_add(events as u64, Ordering::Relaxed);
                    }
                    tracing::warn!(
                        event_type = "syslog_event_oversized",
                        batch_env = %key.0,
                        batch_service = %key.1,
                        events,
                        bytes = batch.ndjson.len(),
                        "syslog event larger than the hot-buffer admission ceiling dropped"
                    );
                }
            }
        }
        let was_blocked = self.sender.backpressure.swap(blocked, Ordering::AcqRel);
        if blocked && !was_blocked {
            tracing::warn!(
                event_type = "syslog_backpressure",
                queued_events = self.queue.iter().map(|(_, b)| b.maps.len()).sum::<usize>(),
                "hot buffer refused syslog events; holding them until compaction drains space"
            );
        } else if !blocked && was_blocked {
            tracing::info!(
                event_type = "syslog_backpressure_cleared",
                "hot buffer admitted the held syslog events; receiving again"
            );
        }
        if !admitted.is_empty() {
            self.write(admitted).await;
        }
    }

    /// Write admitted groups through the pipeline in a blocking task.
    ///
    /// WAL writes (`std::fs::write` + `std::fs::rename`) are synchronous
    /// I/O, and the pipeline takes the publication gate per group, so we
    /// run them on the blocking thread pool to avoid stalling the tokio
    /// runtime.
    async fn write(&self, groups: Vec<AdmittedGroup>) {
        let group_count = groups.len();
        let events: usize = groups.iter().map(|g| g.batch.maps.len()).sum();

        let pipeline = Arc::clone(&self.pipeline);
        let written = tokio::task::spawn_blocking(move || pipeline.write(groups))
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

        tracing::debug!(
            event_type = "syslog_batch_flushed",
            groups = group_count,
            events,
            written,
            "flushed syslog batch to pipeline"
        );
    }
}

/// Admission fixtures shared by the syslog listener tests.
#[cfg(test)]
pub(super) mod test_support {
    use std::sync::Arc;

    use crate::bus::IngestBatch;
    use crate::hot_buffer::{HotBuffer, HotBufferConfig};
    use crate::ingest::pipeline::PipelineWriter;
    use crate::ingest::wal::WalWriter;

    /// The drain key of the [`admission_pipeline`] filler batch.
    pub const FILLER_ID: &str = "prod/filler";

    /// A real pipeline over a real WAL in `tmp` and a hot buffer of
    /// `max_events` / `max_bytes`, pre-filled with `filler_events`
    /// self-telemetry events (one byte in all) under [`FILLER_ID`].
    pub fn admission_pipeline(
        tmp: &std::path::Path,
        max_events: usize,
        max_bytes: usize,
        filler_events: usize,
    ) -> (Arc<PipelineWriter>, Arc<HotBuffer>) {
        let wal = Arc::new(WalWriter::new(tmp.to_path_buf()));
        wal.ensure_dir().unwrap();
        let hot = Arc::new(HotBuffer::new(HotBufferConfig {
            max_events,
            max_bytes,
        }));
        if filler_events > 0 {
            hot.insert_for_test(Arc::new(IngestBatch {
                batch_id: FILLER_ID.into(),
                service: "filler".into(),
                events: vec![serde_json::Map::new(); filler_events],
                byte_size: 1,
            }));
        }
        let pipeline = Arc::new(PipelineWriter::new(wal, Some(Arc::clone(&hot)), None));
        (pipeline, hot)
    }

    /// Poll `condition` until it holds, failing after five seconds.
    pub async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !condition() {
            assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// Every WAL line under `root/env`, across files.
    pub fn wal_lines(root: &std::path::Path, env: &str) -> Vec<String> {
        let Ok(dir) = std::fs::read_dir(root.join(env)) else {
            return Vec::new();
        };
        dir.filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .flat_map(|e| {
                std::fs::read_to_string(e.path())
                    .unwrap()
                    .lines()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SyslogConfig;
    use crate::ingest::wal::WalWriter;
    use serde_json::json;

    const QUEUE_FULL_DROPS: &str = "trawl_syslog_events_dropped_total{reason=\"queue_full\"}";
    const BACKPRESSURE_DROPS: &str = "trawl_syslog_events_dropped_total{reason=\"backpressure\"}";
    const SYSLOG_FULL_REFUSALS: &str =
        "trawl_hot_buffer_admission_refusals_total{producer=\"syslog\",kind=\"full\"}";
    const SYSLOG_OVERSIZED_REFUSALS: &str =
        "trawl_hot_buffer_admission_refusals_total{producer=\"syslog\",kind=\"oversized\"}";

    fn current_thread() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn queue_full_and_closed_count_each_abandoned_event_once() {
        use crate::metrics::test_support::sample;
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let stats = Arc::new(SyslogStats::default());
            let (sender, mut receiver) = SyslogSender::channel_for_test(1);
            assert!(sender.try_enqueue(make_event("accepted"), Some(&stats)));
            assert_eq!(sample(&handle, QUEUE_FULL_DROPS), 0);
            for transport in ["tcp", "udp"] {
                let mut event = make_event("full");
                event.transport = transport;
                assert!(!sender.try_enqueue(event, Some(&stats)));
            }
            assert_eq!(sample(&handle, QUEUE_FULL_DROPS), 2);
            assert_eq!(receiver.try_recv().unwrap().service, "accepted");
            assert!(receiver.try_recv().is_err());
            drop(receiver);
            // A closed queue is `queue_full` even while backpressure is
            // raised: the batcher is gone, not blocked.
            sender.set_backpressure_for_test(true);
            for transport in ["tcp", "udp"] {
                let mut event = make_event("closed");
                event.transport = transport;
                assert!(!sender.try_enqueue(event, Some(&stats)));
            }
            assert_eq!(sample(&handle, QUEUE_FULL_DROPS), 4);
            assert_eq!(sample(&handle, BACKPRESSURE_DROPS), 0);
            assert_eq!(stats.dropped.load(Ordering::Relaxed), 4);
            assert_eq!(
                sample(&handle, crate::metrics::SYSLOG_WAL_EVENTS_DISCARDED_TOTAL),
                0
            );
        });
    }

    /// AC12: a UDP datagram dropped on a full queue is `backpressure` while
    /// the batcher is blocked on hot-buffer admission and `queue_full`
    /// otherwise. The flag is the real batcher's, raised by a real `Full`
    /// refusal and cleared by a real drain.
    #[test]
    fn udp_drop_reason_is_backpressure_only_while_admission_blocks_the_batcher() {
        use crate::metrics::test_support::sample;
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            current_thread().block_on(async {
                let stats = Arc::new(SyslogStats::default());
                // External ceiling 15 of 16, all 15 already charged.
                let tmp = tempfile::tempdir().unwrap();
                let (pipeline, hot) = test_support::admission_pipeline(tmp.path(), 16, 1 << 20, 15);
                let config = SyslogConfig {
                    channel_capacity: 1,
                    batch_max_events: 1,
                    ..SyslogConfig::default()
                };
                let batcher = SyslogBatcher::new(
                    &config,
                    pipeline,
                    Duration::from_secs(3600),
                    Some(Arc::clone(&stats)),
                );
                let sender = batcher.sender();
                let (shutdown_tx, shutdown_rx) = watch::channel(false);

                // Before the batcher runs, a full queue is only full.
                assert!(sender.try_enqueue(make_event("first"), Some(&stats)));
                assert!(!sender.try_enqueue(make_event("unblocked"), Some(&stats)));
                assert_eq!(sample(&handle, QUEUE_FULL_DROPS), 1);
                assert_eq!(sample(&handle, BACKPRESSURE_DROPS), 0);

                let task = tokio::spawn(batcher.run(shutdown_rx));
                test_support::eventually("the batcher blocks", || sender.backpressure_for_test())
                    .await;
                assert!(sample(&handle, SYSLOG_FULL_REFUSALS) >= 1);
                // The batcher stopped receiving: one slot fills, then drops.
                assert!(sender.try_enqueue(make_event("second"), Some(&stats)));
                assert!(!sender.try_enqueue(make_event("blocked"), Some(&stats)));
                assert_eq!(sample(&handle, BACKPRESSURE_DROPS), 1);
                assert_eq!(sample(&handle, QUEUE_FULL_DROPS), 1);

                hot.drain(&[test_support::FILLER_ID]);
                test_support::eventually("the held events land", || hot.event_count() == 2).await;
                assert!(!sender.backpressure_for_test(), "a drained queue clears it");
                let _ = shutdown_tx.send(true);
                task.await.unwrap();

                let lines = test_support::wal_lines(tmp.path(), "prod");
                assert_eq!(lines.len(), 2, "{lines:?}");
                assert_eq!(stats.dropped.load(Ordering::Relaxed), 2);
                assert_eq!(sample(&handle, BACKPRESSURE_DROPS), 1);
                assert_eq!(sample(&handle, QUEUE_FULL_DROPS), 1);
            });
        });
    }

    /// Shutdown makes one final ordered attempt, taking what the channel
    /// still holds; whatever admission still refuses is counted dropped
    /// for backpressure, and nothing reaches the WAL.
    #[test]
    fn shutdown_counts_events_still_refused_as_backpressure_drops() {
        use crate::metrics::test_support::sample;
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            current_thread().block_on(async {
                let stats = Arc::new(SyslogStats::default());
                let tmp = tempfile::tempdir().unwrap();
                let (pipeline, hot) = test_support::admission_pipeline(tmp.path(), 16, 1 << 20, 15);
                let config = SyslogConfig {
                    channel_capacity: 10,
                    batch_max_events: 1,
                    ..SyslogConfig::default()
                };
                let batcher = SyslogBatcher::new(
                    &config,
                    pipeline,
                    Duration::from_secs(3600),
                    Some(Arc::clone(&stats)),
                );
                let sender = batcher.sender();
                let (shutdown_tx, shutdown_rx) = watch::channel(false);
                let task = tokio::spawn(batcher.run(shutdown_rx));
                for service in ["a", "b", "c"] {
                    assert!(sender.try_enqueue(make_event(service), Some(&stats)));
                }
                test_support::eventually("the batcher blocks", || sender.backpressure_for_test())
                    .await;

                let _ = shutdown_tx.send(true);
                task.await.unwrap();
                assert_eq!(sample(&handle, BACKPRESSURE_DROPS), 3);
                assert_eq!(sample(&handle, QUEUE_FULL_DROPS), 0);
                assert_eq!(stats.dropped.load(Ordering::Relaxed), 3);
                assert_eq!(hot.event_count(), 15, "only the filler is resident");
                assert!(test_support::wal_lines(tmp.path(), "prod").is_empty());
            });
        });
    }

    fn batch_of(messages: &[&str]) -> ServiceBatch {
        let mut batch = ServiceBatch::default();
        for message in messages {
            batch.push(json!({"message": message}).as_object().unwrap().clone());
        }
        batch
    }

    fn messages(batch: &ServiceBatch) -> Vec<String> {
        let from_maps: Vec<String> = batch
            .maps
            .iter()
            .map(|map| map["message"].as_str().unwrap().to_owned())
            .collect();
        let from_lines: Vec<String> = std::str::from_utf8(&batch.ndjson)
            .unwrap()
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["message"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(from_maps, from_lines, "maps and lines stay paired");
        from_maps
    }

    #[test]
    fn split_to_fit_cuts_at_event_boundaries_in_order() {
        let unbounded = Charge {
            events: usize::MAX,
            bytes: usize::MAX,
        };
        let whole = split_to_fit(batch_of(&["a", "b", "c"]), unbounded);
        assert_eq!(whole.len(), 1);
        assert_eq!(messages(&whole[0]), ["a", "b", "c"]);

        // Bounded by events.
        let chunks = split_to_fit(
            batch_of(&["a", "b", "c", "d", "e"]),
            Charge {
                events: 2,
                bytes: usize::MAX,
            },
        );
        let got: Vec<Vec<String>> = chunks.iter().map(messages).collect();
        assert_eq!(got, [vec!["a", "b"], vec!["c", "d"], vec!["e"]]);

        // Bounded by bytes: every one-letter line has the same length.
        let line = batch_of(&["x"]).ndjson.len();
        let chunks = split_to_fit(
            batch_of(&["a", "b", "c"]),
            Charge {
                events: usize::MAX,
                bytes: 2 * line + 1,
            },
        );
        let got: Vec<Vec<String>> = chunks.iter().map(messages).collect();
        assert_eq!(got, [vec!["a", "b"], vec!["c"]]);

        // An event over the ceiling on its own becomes a chunk of its own.
        let big = "y".repeat(100);
        let chunks = split_to_fit(
            batch_of(&["a", &big, "b"]),
            Charge {
                events: usize::MAX,
                bytes: 3 * line,
            },
        );
        let got: Vec<Vec<String>> = chunks.iter().map(messages).collect();
        assert_eq!(got, [vec!["a".to_owned()], vec![big], vec!["b".to_owned()]]);
    }

    /// An oversized group is split to fit the syslog ceiling; the lone
    /// event that cannot fit at all is dropped and counted as an
    /// `oversized` refusal, and its neighbours still land.
    #[test]
    fn oversized_group_is_split_and_a_lone_oversized_event_is_dropped_and_counted() {
        use crate::metrics::test_support::sample;
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            current_thread().block_on(async {
                let stats = Arc::new(SyslogStats::default());
                let tmp = tempfile::tempdir().unwrap();
                // External byte ceiling: 1500 of 1600.
                let (pipeline, hot) = test_support::admission_pipeline(tmp.path(), 100, 1600, 0);
                let config = SyslogConfig {
                    batch_max_events: 3,
                    ..SyslogConfig::default()
                };
                let batcher = SyslogBatcher::new(
                    &config,
                    pipeline,
                    Duration::from_secs(3600),
                    Some(Arc::clone(&stats)),
                );
                let sender = batcher.sender();
                let (shutdown_tx, shutdown_rx) = watch::channel(false);
                let task = tokio::spawn(batcher.run(shutdown_rx));

                let mut big = make_event("svc");
                big.map.insert("message".into(), json!("z".repeat(2000)));
                let mut first = make_event("svc");
                first.map.insert("message".into(), json!("first"));
                let mut last = make_event("svc");
                last.map.insert("message".into(), json!("last"));
                for event in [first, big, last] {
                    assert!(sender.try_enqueue(event, Some(&stats)));
                }
                test_support::eventually("the neighbours land", || hot.event_count() == 2).await;
                let _ = shutdown_tx.send(true);
                task.await.unwrap();

                assert_eq!(sample(&handle, SYSLOG_OVERSIZED_REFUSALS), 1);
                assert_eq!(sample(&handle, SYSLOG_FULL_REFUSALS), 0);
                assert_eq!(stats.dropped.load(Ordering::Relaxed), 1);
                assert_eq!(sample(&handle, BACKPRESSURE_DROPS), 0);
                assert_eq!(sample(&handle, QUEUE_FULL_DROPS), 0);
                let lines = test_support::wal_lines(tmp.path(), "prod");
                assert_eq!(lines.len(), 2, "{lines:?}");
                assert!(lines.iter().any(|l| l.contains("\"first\"")));
                assert!(lines.iter().any(|l| l.contains("\"last\"")));
                assert_eq!(hot.charged().events, 2, "the dropped event holds nothing");
            });
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
            let (mut batcher, tmp) = test_batcher(1000, 100);
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

        let batcher = SyslogBatcher::new(&config, pipeline, Duration::from_secs(3600), None);
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

        assert!(sender.send(make_event("test-svc"), None).await);

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
            assert!(sender.send(make_event("test-svc"), None).await);
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
        assert!(sender.send(make_event("svc-a"), None).await);
        assert!(sender.send(make_event("svc-b"), None).await);

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
            assert!(sender.send(event, None).await);
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
