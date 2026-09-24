// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared ingest pipeline: WAL write → hot buffer → event bus.
//!
//! Events are durable before they are visible: only a batch whose WAL write
//! succeeded reaches the hot buffer and the event bus.
//!
//! Admission comes first (ADR-0043): a producer reserves hot-buffer space
//! for the exact [`ServiceBatch::charge`] before it takes the publication
//! gate or writes anything, and hands the [`Reservation`] to
//! [`PipelineWriter::write_admitted`] or [`PipelineWriter::publish_admitted`]
//! with the batch. A group whose write fails drops its reservation, which
//! releases the space.

use std::path::Path;
use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::Map;

use crate::bus::{EventBus as _, IngestBatch, LocalEventBus};
use crate::hot_buffer::{Charge, HotBuffer, Refusal, Reservation};
use crate::ingest::producer::ProducerKind;
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
///
/// Invariant: one map per ndjson line, so [`charge`](Self::charge) is exact
/// and equals the [`IngestBatch`] the batch publishes as.
#[derive(Debug, Default)]
pub struct ServiceBatch {
    /// Parsed event maps (for hot buffer / event bus).
    pub maps: Vec<Map<String, serde_json::Value>>,
    /// Serialized ndjson bytes (for WAL file writing).
    pub ndjson: Vec<u8>,
}

impl ServiceBatch {
    /// Add an event to this batch, serializing it to ndjson in the process.
    ///
    /// The map is kept only if its line was written, so maps and lines stay
    /// one to one. Serializing a `Map<String, Value>` into a `Vec` cannot
    /// fail (string keys, an infallible writer, and `Value` never refuses
    /// to serialize), so the refusal branch is unreachable in practice; it
    /// exists so a charge taken from this batch can never disagree with
    /// the WAL bytes.
    pub fn push(&mut self, map: Map<String, serde_json::Value>) {
        let start = self.ndjson.len();
        if serde_json::to_writer(&mut self.ndjson, &map).is_err() {
            self.ndjson.truncate(start);
            debug_assert!(false, "a JSON object failed to serialize");
            return;
        }
        self.ndjson.push(b'\n');
        self.maps.push(map);
    }

    /// The hot-buffer charge this batch publishes: `(maps, ndjson bytes)`,
    /// exactly `(IngestBatch::events.len(), IngestBatch::byte_size)`.
    pub fn charge(&self) -> Charge {
        Charge {
            events: self.maps.len(),
            bytes: self.ndjson.len(),
        }
    }
}

/// One `(env, service)` group whose hot-buffer space is already admitted.
#[derive(Debug)]
pub struct AdmittedGroup {
    pub key: BatchKey,
    pub batch: ServiceBatch,
    /// Holds exactly `batch.charge()`.
    pub reservation: Reservation,
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

    /// Reserve hot-buffer space for `charge` on behalf of `producer`
    /// ([`HotBuffer::reserve`]).
    ///
    /// A pipeline without a hot buffer (embedded and test wiring) meters
    /// nothing: it always admits, with an unmetered reservation whose drop
    /// releases nothing. There is no buffer to protect and no drain to
    /// wait for.
    #[allow(
        dead_code,
        reason = "#253 transition: producers adopt admission in later checkpoints"
    )]
    pub(crate) fn reserve(
        &self,
        producer: ProducerKind,
        charge: Charge,
    ) -> Result<Reservation, Refusal> {
        match &self.hot_buffer {
            Some(buf) => buf.reserve(producer, charge),
            None => Ok(Reservation::unmetered(charge)),
        }
    }

    /// The most `producer` may have charged at once
    /// ([`HotBuffer::ceiling`]); unbounded without a hot buffer.
    #[allow(
        dead_code,
        reason = "#253 transition: producers adopt admission in later checkpoints"
    )]
    pub(crate) fn ceiling(&self, producer: ProducerKind) -> Charge {
        self.hot_buffer.as_ref().map_or(
            Charge {
                events: usize::MAX,
                bytes: usize::MAX,
            },
            |buf| buf.ceiling(producer),
        )
    }

    /// Write admitted groups through the pipeline: WAL → hot buffer → event
    /// bus, each group under its own publication read guard.
    ///
    /// Returns the number of events successfully written. A group whose
    /// WAL write fails, a failed directory fsync included, is dropped
    /// (logged, counted, not published) and its reservation released; that
    /// holds for a file left visible to compaction too (ADR-0043), because
    /// it was never acknowledged.
    ///
    /// The env comes from the key, never from a writer-held default.
    /// Call from a blocking thread because WAL I/O and the publication
    /// read guard can wait; the caller must not wait for capacity while
    /// holding the gate, which is why admission happened before this call.
    pub fn write_admitted(&self, groups: Vec<AdmittedGroup>) -> usize {
        let mut total_written = 0;

        for AdmittedGroup {
            key: (env, svc),
            batch,
            reservation,
        } in groups
        {
            let event_count = batch.maps.len();
            // Compaction may read the WAL as soon as its rename completes.
            // Keep its publication and drain behind this hot insertion.
            let publication = self.hot_buffer.as_ref().map(|buf| buf.publication());
            let _ingest = publication.as_ref().map(|gate| gate.blocking_ingest());

            match self.wal_writer.write(&env, &svc, &batch.ndjson) {
                Ok(wal_path) => {
                    total_written += event_count;
                    self.publish_admitted(&env, &svc, batch, &wal_path, reservation);
                }
                Err(e) => {
                    drop(reservation);
                    Self::record_discarded_group(&env, &svc, event_count, &e);
                }
            }
        }

        total_written
    }

    /// Write batches through the pipeline: WAL → hot buffer → event bus.
    ///
    /// Returns the number of events successfully written. Events from
    /// groups that fail WAL writing, a failed directory fsync included,
    /// are dropped (logged, counted, not published).
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
                Err(e) => Self::record_discarded_group(&env, &svc, event_count, &e),
            }
        }

        total_written
    }

    fn record_discarded_group(
        env: &str,
        svc: &str,
        event_count: usize,
        e: &crate::ingest::wal::WalWriteError,
    ) {
        metrics::counter!(crate::metrics::SYSLOG_WAL_EVENTS_DISCARDED_TOTAL)
            .increment(event_count as u64);
        tracing::warn!(
            event_type = "pipeline_wal_write_failed",
            batch_env = %env,
            batch_service = %svc,
            events_lost = event_count,
            left_visible = e.left_visible(),
            error = %e,
            "WAL write failed for batch"
        );
    }

    /// Publish a successfully-written batch to hot buffer and event bus.
    ///
    /// Called after WAL writing succeeds to make events immediately
    /// visible to queries (via hot buffer) and SSE streams (via event bus).
    /// The caller holds the publication read guard from before the WAL
    /// write through this call. Reacquiring here can deadlock behind a
    /// queued compactor waiting for the caller's existing read guard.
    pub(crate) fn publish(&self, env: &str, svc: &str, batch: ServiceBatch, wal_path: &Path) {
        self.publish_with(env, svc, batch, wal_path, |buf, ingest_batch| {
            buf.insert_evicting(ingest_batch);
        });
    }

    /// Publish a successfully-written, admitted batch: the reservation's
    /// charge moves into the resident batch. Without a hot buffer the
    /// (unmetered) reservation is simply dropped.
    ///
    /// Same guard discipline as [`publish`](Self::publish): the caller holds
    /// the publication read guard from before the WAL write through this
    /// call.
    pub fn publish_admitted(
        &self,
        env: &str,
        svc: &str,
        batch: ServiceBatch,
        wal_path: &Path,
        reservation: Reservation,
    ) {
        debug_assert_eq!(
            reservation.charge(),
            batch.charge(),
            "publish_admitted with a reservation that does not match its batch"
        );
        self.publish_with(env, svc, batch, wal_path, move |buf, ingest_batch| {
            buf.insert(reservation, ingest_batch);
        });
    }

    fn publish_with(
        &self,
        env: &str,
        svc: &str,
        batch: ServiceBatch,
        wal_path: &Path,
        insert: impl FnOnce(&HotBuffer, Arc<IngestBatch>),
    ) {
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
            insert(buf, Arc::clone(&ingest_batch));
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

    #[test]
    fn directory_sync_failure_discards_the_group_and_publishes_nothing() {
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let tmp = tempfile::tempdir().unwrap();
            let wal = Arc::new(WalWriter::new(tmp.path().join("wal")));
            wal.ensure_dir().unwrap();
            let hot = Arc::new(HotBuffer::new(crate::hot_buffer::HotBufferConfig {
                max_events: 100,
                max_bytes: 1024 * 1024,
            }));
            let pipeline = PipelineWriter::new(Arc::clone(&wal), Some(Arc::clone(&hot)), None);
            let mut batch = ServiceBatch::default();
            for id in 0..2 {
                batch.push(
                    serde_json::json!({"service": "syslog", "id": id})
                        .as_object()
                        .unwrap()
                        .clone(),
                );
            }
            // Make `prod` durable first, so the injected failure hits the
            // env directory sync after the rename, not the root sync.
            std::fs::remove_file(wal.write("prod", "warm", b"{}\n").unwrap()).unwrap();
            wal.fail_next_directory_sync_for_test();
            let batches = IndexMap::from([(("prod".to_owned(), "syslog".to_owned()), batch)]);
            assert_eq!(pipeline.write(batches), 0);
            assert_eq!(
                sample(&handle, crate::metrics::SYSLOG_WAL_EVENTS_DISCARDED_TOTAL),
                2
            );
            assert_eq!(
                sample(
                    &handle,
                    "trawl_wal_durability_failures_total{operation=\"parent_directory_sync\"}"
                ),
                1
            );
            assert_eq!(hot.event_count(), 0, "nothing is published");
            let files: Vec<_> = std::fs::read_dir(wal.dir().join("prod"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert!(files.is_empty(), "no WAL file for compaction: {files:?}");
        });
    }

    // -- admitted writes (ADR-0043) -------------------------------------------

    use crate::hot_buffer::Charge;

    fn syslog_batch(env: &str, service: &str, count: usize) -> ServiceBatch {
        let mut batch = ServiceBatch::default();
        for id in 0..count {
            batch.push(
                serde_json::json!({"env": env, "service": service, "id": id})
                    .as_object()
                    .unwrap()
                    .clone(),
            );
        }
        batch
    }

    fn admit(pipeline: &PipelineWriter, env: &str, service: &str, count: usize) -> AdmittedGroup {
        let batch = syslog_batch(env, service, count);
        let reservation = pipeline
            .reserve(ProducerKind::Syslog, batch.charge())
            .expect("fits an empty buffer");
        AdmittedGroup {
            key: (env.to_owned(), service.to_owned()),
            batch,
            reservation,
        }
    }

    fn test_hot() -> Arc<HotBuffer> {
        Arc::new(HotBuffer::new(crate::hot_buffer::HotBufferConfig {
            max_events: 100,
            max_bytes: 1024 * 1024,
        }))
    }

    #[test]
    fn push_keeps_one_map_per_line_and_charge_is_exact() {
        let batch = syslog_batch("prod", "svc", 3);
        let text = std::str::from_utf8(&batch.ndjson).unwrap();
        assert_eq!(text.lines().count(), batch.maps.len());
        assert_eq!(
            batch.charge(),
            Charge {
                events: 3,
                bytes: batch.ndjson.len()
            }
        );
    }

    #[test]
    fn admitted_failed_group_releases_only_its_reservation() {
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
            let hot = test_hot();
            let pipeline = PipelineWriter::new(Arc::clone(&wal), Some(Arc::clone(&hot)), None);
            let groups = vec![
                admit(&pipeline, "prod", "before", 1),
                admit(&pipeline, "blocked", "failed", 3),
                admit(&pipeline, "prod", "after", 2),
            ];
            let durable = groups[0]
                .batch
                .charge()
                .checked_add(groups[2].batch.charge())
                .unwrap();
            assert_eq!(hot.charged().events, 6, "all three groups reserved");

            assert_eq!(pipeline.write_admitted(groups), 3);
            assert_eq!(
                hot.charged(),
                durable,
                "the failed group's share is released, the durable ones resident"
            );
            assert_eq!(
                (hot.event_count(), hot.byte_count()),
                (durable.events, durable.bytes)
            );
            assert_eq!(
                sample(&handle, crate::metrics::SYSLOG_WAL_EVENTS_DISCARDED_TOTAL),
                3
            );
            assert_eq!(
                std::fs::read(wal.dir().join("blocked")).unwrap(),
                b"keep me"
            );
            assert_eq!(pipeline.write_admitted(Vec::new()), 0);
        });
    }

    #[test]
    fn admitted_directory_sync_failure_returns_the_charge() {
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let tmp = tempfile::tempdir().unwrap();
            let wal = Arc::new(WalWriter::new(tmp.path().join("wal")));
            wal.ensure_dir().unwrap();
            let hot = test_hot();
            let pipeline = PipelineWriter::new(Arc::clone(&wal), Some(Arc::clone(&hot)), None);
            let before = hot.charged();
            let group = admit(&pipeline, "prod", "syslog", 2);
            // Make `prod` durable first, so the injected failure hits the
            // env directory sync after the rename, not the root sync.
            std::fs::remove_file(wal.write("prod", "warm", b"{}\n").unwrap()).unwrap();
            wal.fail_next_directory_sync_for_test();
            assert_eq!(pipeline.write_admitted(vec![group]), 0);
            assert_eq!(
                hot.charged(),
                before,
                "NotPublished releases the reservation"
            );
            assert_eq!(hot.event_count(), 0, "nothing is published");
            assert_eq!(
                sample(&handle, crate::metrics::SYSLOG_WAL_EVENTS_DISCARDED_TOTAL),
                2
            );
            let files: Vec<_> = std::fs::read_dir(wal.dir().join("prod"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert!(files.is_empty(), "no WAL file for compaction: {files:?}");
        });
    }

    #[test]
    fn admitted_write_left_visible_releases_the_charge_and_inserts_nothing() {
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        metrics::with_local_recorder(&recorder, || {
            let tmp = tempfile::tempdir().unwrap();
            let wal = Arc::new(WalWriter::new(tmp.path().join("wal")));
            wal.ensure_dir().unwrap();
            let hot = test_hot();
            let pipeline = PipelineWriter::new(Arc::clone(&wal), Some(Arc::clone(&hot)), None);
            let before = hot.charged();
            let mut released = hot.subscribe_released();
            released.borrow_and_update();
            let group = admit(&pipeline, "prod", "stuck", 2);
            let bytes = group.batch.ndjson.clone();
            std::fs::remove_file(wal.write("prod", "warm", b"{}\n").unwrap()).unwrap();
            // The env sync fails and so does the withdrawal: the file stays
            // under its final name (`WalWriteError::LeftVisible`).
            wal.fail_next_directory_sync_for_test();
            wal.fail_next_withdraw_for_test();
            assert_eq!(pipeline.write_admitted(vec![group]), 0);
            assert_eq!(
                hot.charged(),
                before,
                "a write left visible was never acknowledged: its charge is released"
            );
            assert_eq!(hot.event_count(), 0, "nothing is inserted");
            assert!(released.has_changed().unwrap());
            let files: Vec<_> = std::fs::read_dir(wal.dir().join("prod"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert_eq!(files.len(), 1, "the file stays for compaction: {files:?}");
            assert_eq!(std::fs::read(&files[0]).unwrap(), bytes);
        });
    }

    #[test]
    fn pipeline_without_a_hot_buffer_admits_unmetered() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = Arc::new(WalWriter::new(tmp.path().join("wal")));
        wal.ensure_dir().unwrap();
        let pipeline = PipelineWriter::new(Arc::clone(&wal), None, None);
        let unbounded = Charge {
            events: usize::MAX,
            bytes: usize::MAX,
        };
        assert_eq!(pipeline.ceiling(ProducerKind::Http), unbounded);
        let huge = pipeline.reserve(ProducerKind::Http, unbounded).unwrap();
        assert_eq!(huge.charge(), unbounded);
        drop(huge);
        let group = admit(&pipeline, "prod", "svc", 2);
        assert_eq!(pipeline.write_admitted(vec![group]), 2);
        assert_eq!(
            std::fs::read_dir(wal.dir().join("prod")).unwrap().count(),
            1
        );
    }

    #[test]
    fn publish_admitted_moves_the_reservation_into_the_hot_buffer() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = Arc::new(WalWriter::new(tmp.path().join("wal")));
        wal.ensure_dir().unwrap();
        let hot = test_hot();
        let pipeline = PipelineWriter::new(Arc::clone(&wal), Some(Arc::clone(&hot)), None);
        let AdmittedGroup {
            key: (env, svc),
            batch,
            reservation,
        } = admit(&pipeline, "prod", "svc", 3);
        let charge = batch.charge();
        let path = wal.write(&env, &svc, &batch.ndjson).unwrap();
        pipeline.publish_admitted(&env, &svc, batch, &path, reservation);
        assert_eq!(hot.charged(), charge);
        assert_eq!(
            (hot.event_count(), hot.byte_count()),
            (charge.events, charge.bytes)
        );
        let stem = path.file_stem().unwrap().to_str().unwrap();
        hot.drain(&[&format!("prod/{stem}")]);
        assert_eq!(hot.charged(), Charge::ZERO);
    }
}
