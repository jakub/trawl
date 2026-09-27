// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Compaction's coverage proof (ADR-0041 slice 2): the only way the restart
//! overhang clears.
//!
//! Boot hydration leaves overhang when some WAL that survived the restart
//! did not become resident. Overhang is a state of the publication gate,
//! not a list of files, so the proof asks the WAL itself: every `.ndjson`
//! file in the scope hydration listed ([`walk_wal`]) must be resident in
//! the hot buffer, or gone because compaction published and retired it.
//!
//! The listing takes no guard, so it may see a file a producer has
//! published and not yet inserted. Every producer holds the publication
//! read guard from before its WAL write through its hot insert, so the
//! files the listing found not resident are checked again under the write
//! guard: by then each is resident or gone, unless it really is left over.
//! The overhang clears under that same guard. A file written after the
//! listing needs no check, because its producer inserts it.
//!
//! The proof fails, and overhang stays, when:
//!
//! - some scope could not be listed ([`ProofOutcome::Incomplete`]);
//! - a publication marker is pending, even one whose files are gone
//!   ([`ProofOutcome::MarkerPending`]): its recovery may still retire or
//!   keep WAL, and the scopes it blocks were not hydrated;
//! - more than [`MAX_PROOF_CANDIDATES`] files are not resident
//!   ([`ProofOutcome::TooManyCandidates`]): the write guard holds off
//!   ingest and every read, so the re-check is bounded;
//! - a file is neither resident nor gone under the guard
//!   ([`ProofOutcome::NotResident`]).
//!
//! [`prove_coverage`] runs the whole sequence and returns only its outcome,
//! so no proof can be kept and applied after the WAL has changed.

use std::path::{Path, PathBuf};

use crate::hot_buffer::HotBuffer;
use crate::ingest::compaction::wal_batch_id;
use crate::ingest::hydration::walk_wal;
use crate::publication::PublicationGate;

/// The most WAL files that are not resident which the proof re-checks
/// under the publication write guard. More than this fails the proof
/// before it takes the guard.
pub(crate) const MAX_PROOF_CANDIDATES: usize = 1024;

/// What one coverage proof found. Only [`Settled`](Self::Settled) clears
/// the overhang.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProofOutcome {
    /// Every listed WAL file was resident or gone: the overhang is cleared.
    Settled,
    /// The WAL root, a root entry or an env directory could not be listed.
    Incomplete,
    /// A publication marker is pending, or the marker scan could not rule
    /// one out.
    MarkerPending,
    /// More than [`MAX_PROOF_CANDIDATES`] listed files were not resident.
    TooManyCandidates,
    /// A listed file was neither resident nor gone under the write guard.
    NotResident,
}

impl ProofOutcome {
    /// The `snake_case` literal the compaction pass logs.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::Incomplete => "incomplete",
            Self::MarkerPending => "marker_pending",
            Self::TooManyCandidates => "too_many_candidates",
            Self::NotResident => "not_resident",
        }
    }
}

/// Prove that the WAL under `wal_dir` is covered by `hot`, and clear the
/// overhang on `gate` when it is. `gate` must be the publication gate the
/// producers inserting into `hot` hold.
///
/// Blocking: it lists the WAL and takes the publication write guard, so
/// call it from a blocking thread, holding no publication guard.
pub(crate) fn prove_coverage(
    wal_dir: &Path,
    hot: &HotBuffer,
    gate: &PublicationGate,
) -> ProofOutcome {
    prove(wal_dir, hot, gate, |_| {})
}

/// [`prove_coverage`], calling `listed` with the number of candidates after
/// the unguarded listing and before the write guard is taken.
fn prove(
    wal_dir: &Path,
    hot: &HotBuffer,
    gate: &PublicationGate,
    listed: impl FnOnce(usize),
) -> ProofOutcome {
    // List without a guard. A file resident now stays resident until
    // compaction drains it after publishing it, and compaction is the
    // caller, so only the files not resident need the re-check. Each
    // candidate keeps its batch id (`None` for a stem that is not UTF-8,
    // which no batch carries) and its path.
    let mut candidates: Vec<(Option<String>, PathBuf)> = Vec::new();
    let mut too_many = false;
    let walk = walk_wal(wal_dir, |env, _service, files, _blocked| {
        for path in files {
            let batch_id = wal_batch_id(env, &path);
            if batch_id.as_deref().is_some_and(|id| hot.is_resident(id)) {
                continue;
            }
            if candidates.len() < MAX_PROOF_CANDIDATES {
                candidates.push((batch_id, path));
            } else {
                too_many = true;
            }
        }
    });
    let Some(walk) = walk else {
        return ProofOutcome::Incomplete;
    };
    if walk.unlisted > 0 {
        return ProofOutcome::Incomplete;
    }
    if walk.claims.any() {
        return ProofOutcome::MarkerPending;
    }
    if too_many {
        return ProofOutcome::TooManyCandidates;
    }
    listed(candidates.len());

    // Every producer that published a listed file inserts it before it
    // releases its read guard, so under the write guard a candidate is
    // resident, gone, or really left over. Only `NotFound` proves it gone.
    let _publication = gate.blocking_write();
    for (batch_id, path) in &candidates {
        let resident = batch_id.as_deref().is_some_and(|id| hot.is_resident(id));
        let gone = matches!(
            std::fs::symlink_metadata(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        );
        if !resident && !gone {
            return ProofOutcome::NotResident;
        }
    }
    gate.settle_overhang();
    ProofOutcome::Settled
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::bus::IngestBatch;
    use crate::hot_buffer::HotBufferConfig;
    use crate::ingest::pipeline::{PipelineWriter, ServiceBatch};
    use crate::ingest::publication_marker::{self, OutputIdentity, ValidatedMarker};
    use crate::ingest::wal::{WalName, WalWriter};

    const BASE: u64 = 1_790_000_000_000;

    /// A hot buffer whose gate hydration left in overhang.
    fn overhang() -> (Arc<HotBuffer>, Arc<PublicationGate>) {
        let gate = Arc::new(PublicationGate::starting());
        gate.finish_hydration(true).unwrap();
        let hot = HotBuffer::new(HotBufferConfig {
            max_events: 10_000,
            max_bytes: 1 << 24,
        })
        .with_publication_for_test(Arc::clone(&gate));
        (Arc::new(hot), gate)
    }

    /// A writer-named WAL file `wal/{env}/{service}_{millis}_0001.ndjson`
    /// holding one event, and its batch id.
    fn plant(wal: &Path, env: &str, service: &str, millis: u64) -> (PathBuf, String) {
        let name = WalName {
            service: service.into(),
            millis,
            nonce: 1,
        }
        .file_name();
        let dir = wal.join(env);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(&name);
        std::fs::write(&path, b"{\"n\":1}\n").unwrap();
        let id = wal_batch_id(env, &path).unwrap();
        (path, id)
    }

    /// Make `batch_id` resident, as a producer's insert does.
    fn make_resident(hot: &HotBuffer, batch_id: &str, service: &str) {
        let mut event = serde_json::Map::new();
        event.insert("n".into(), 1.into());
        hot.insert_for_test(Arc::new(IngestBatch {
            batch_id: batch_id.into(),
            service: service.into(),
            byte_size: 8,
            events: vec![event],
        }));
    }

    fn assert_overhang(gate: &PublicationGate) {
        assert!(gate.overhang(), "overhang stays");
        assert!(gate.awaits_coverage_proof());
    }

    fn assert_settled(gate: &PublicationGate) {
        assert!(!gate.overhang(), "overhang cleared");
        assert_eq!(gate.unsettled(), None);
    }

    #[test]
    fn a_file_neither_resident_nor_gone_keeps_overhang() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path();
        let (hot, gate) = overhang();
        let (_, resident) = plant(wal, "prod", "api", BASE);
        make_resident(&hot, &resident, "api");
        let (left_over, _) = plant(wal, "lab", "web", BASE + 1);

        assert_eq!(prove_coverage(wal, &hot, &gate), ProofOutcome::NotResident);
        assert_overhang(&gate);

        // Compaction published and retired it: gone is covered.
        std::fs::remove_file(left_over).unwrap();
        assert_eq!(prove_coverage(wal, &hot, &gate), ProofOutcome::Settled);
        assert_settled(&gate);
    }

    /// A file the listing saw, which is gone by the re-check: a write whose
    /// directory sync failed withdraws its name and inserts nothing. Gone
    /// is covered, so the proof settles.
    #[test]
    fn a_candidate_gone_by_the_re_check_is_covered() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path();
        let (hot, gate) = overhang();
        let (withdrawn, _) = plant(wal, "prod", "api", BASE);
        let outcome = prove(wal, &hot, &gate, |candidates| {
            assert_eq!(candidates, 1);
            std::fs::remove_file(&withdrawn).unwrap();
        });
        assert_eq!(outcome, ProofOutcome::Settled);
        assert_settled(&gate);
    }

    #[test]
    fn settles_when_every_listed_file_is_resident() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path();
        let (hot, gate) = overhang();
        for (env, service, millis) in [("prod", "api", BASE), ("prod", "web", BASE + 1)] {
            let (_, id) = plant(wal, env, service, millis);
            make_resident(&hot, &id, service);
        }
        // Siblings that are not WAL are not in the scope.
        std::fs::write(wal.join("prod").join("api_1_0001.ndjson.merged"), b"x").unwrap();
        std::fs::write(wal.join("prod").join("api_2_0001.ndjson.tmp"), b"x").unwrap();
        assert_eq!(prove_coverage(wal, &hot, &gate), ProofOutcome::Settled);
        assert_settled(&gate);
    }

    #[test]
    fn a_missing_wal_root_settles() {
        let tmp = tempfile::tempdir().unwrap();
        let (hot, gate) = overhang();
        let outcome = prove_coverage(&tmp.path().join("missing"), &hot, &gate);
        assert_eq!(outcome, ProofOutcome::Settled);
        assert_settled(&gate);
    }

    /// A scope that cannot be listed may hold WAL that is not resident. A
    /// symlink loop at the WAL root fails for any user.
    #[cfg(unix)]
    #[test]
    fn an_incomplete_listing_keeps_overhang() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path();
        let (hot, gate) = overhang();
        let (planted, id) = plant(wal, "prod", "api", BASE);
        make_resident(&hot, &id, "api");
        let cycle = wal.join("cycle");
        std::os::unix::fs::symlink("cycle", &cycle).unwrap();
        assert_eq!(prove_coverage(wal, &hot, &gate), ProofOutcome::Incomplete);
        assert_overhang(&gate);
        std::fs::remove_file(&cycle).unwrap();

        // A WAL root that is not a directory.
        assert_eq!(
            prove_coverage(&planted, &hot, &gate),
            ProofOutcome::Incomplete
        );
        assert_overhang(&gate);

        assert_eq!(prove_coverage(wal, &hot, &gate), ProofOutcome::Settled);
        assert_settled(&gate);
    }

    #[cfg(unix)]
    #[test]
    fn an_env_directory_that_cannot_be_listed_keeps_overhang() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path();
        let (hot, gate) = overhang();
        let (_, id) = plant(wal, "prod", "api", BASE);
        make_resident(&hot, &id, "api");
        plant(wal, "locked", "api", BASE + 1);
        let locked = wal.join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&locked).is_ok() {
            // Running as root: mode bits are not enforced.
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let outcome = prove_coverage(wal, &hot, &gate);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(outcome, ProofOutcome::Incomplete);
        assert_overhang(&gate);

        std::fs::remove_dir_all(&locked).unwrap();
        assert_eq!(prove_coverage(wal, &hot, &gate), ProofOutcome::Settled);
        assert_settled(&gate);
    }

    /// A marker whose WAL files are all gone still stops the proof: its
    /// recovery has not run, and the scope it blocks was not hydrated.
    #[test]
    fn a_pending_marker_keeps_overhang_though_its_files_are_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path();
        let (hot, gate) = overhang();
        let (_, id) = plant(wal, "prod", "api", BASE);
        make_resident(&hot, &id, "api");
        let marker = ValidatedMarker::new(
            "prod",
            "web",
            chrono::NaiveDate::from_ymd_opt(2026, 9, 27).unwrap(),
            7,
            vec![
                WalName {
                    service: "web".into(),
                    millis: BASE,
                    nonce: 2,
                }
                .file_name(),
            ],
            OutputIdentity {
                size: 4,
                hash: blake3::hash(b"PAR1"),
            },
        )
        .unwrap();
        publication_marker::write_marker(wal, &marker).unwrap();
        for path in marker.wal_paths(wal) {
            assert!(!path.exists());
        }
        assert_eq!(
            prove_coverage(wal, &hot, &gate),
            ProofOutcome::MarkerPending
        );
        assert_overhang(&gate);

        // A marker that cannot be read claims as much.
        let marker_path = marker.marker_path(wal);
        std::fs::write(&marker_path, b"not a marker").unwrap();
        assert_eq!(
            prove_coverage(wal, &hot, &gate),
            ProofOutcome::MarkerPending
        );
        assert_overhang(&gate);

        std::fs::remove_file(&marker_path).unwrap();
        assert_eq!(prove_coverage(wal, &hot, &gate), ProofOutcome::Settled);
        assert_settled(&gate);
    }

    /// More candidates than the bound fail the proof before it takes the
    /// write guard, so a proof that cannot succeed never holds off ingest.
    /// Exactly the bound is re-checked; resident files are no candidates.
    #[test]
    fn more_candidates_than_the_bound_keeps_overhang() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path();
        let (hot, gate) = overhang();
        let mut files: Vec<(PathBuf, String)> = (0..=MAX_PROOF_CANDIDATES as u64)
            .map(|n| plant(wal, "prod", &format!("s{n}"), BASE + n))
            .collect();

        let ingest = gate.blocking_ingest();
        let (done_tx, done_rx) = mpsc::channel();
        let proof = std::thread::spawn({
            let (hot, gate, wal) = (Arc::clone(&hot), Arc::clone(&gate), wal.to_path_buf());
            move || done_tx.send(prove_coverage(&wal, &hot, &gate)).unwrap()
        });
        let outcome = done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the proof fails without waiting for the write guard");
        drop(ingest);
        proof.join().unwrap();
        assert_eq!(outcome, ProofOutcome::TooManyCandidates);
        assert_overhang(&gate);

        let (removed, _) = files.pop().unwrap();
        std::fs::remove_file(removed).unwrap();
        assert_eq!(files.len(), MAX_PROOF_CANDIDATES);
        assert_eq!(prove_coverage(wal, &hot, &gate), ProofOutcome::NotResident);
        assert_overhang(&gate);

        // One more file, but every one of them resident.
        files.push(plant(wal, "lab", "extra", BASE));
        for (path, id) in &files {
            let service = path.file_name().unwrap().to_str().unwrap();
            make_resident(&hot, id, service.split('_').next().unwrap());
        }
        assert_eq!(prove_coverage(wal, &hot, &gate), ProofOutcome::Settled);
        assert_settled(&gate);
    }

    /// A producer paused after its WAL file is published and before its
    /// hot insert, holding the ingest read guard as the pipeline does. The
    /// unguarded listing sees that file as not resident, so the proof waits
    /// for the write guard, then finds it resident and settles. Deciding
    /// from the unguarded listing alone would leave overhang behind every
    /// write in flight.
    #[test]
    fn a_producer_between_publish_and_insert_makes_the_proof_wait_then_settle() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let (hot, gate) = overhang();
        let (_, before) = plant(&wal_dir, "prod", "old", BASE);
        make_resident(&hot, &before, "old");
        let writer = Arc::new(WalWriter::new(wal_dir.clone()));
        writer.ensure_dir().unwrap();
        let pipeline = Arc::new(PipelineWriter::new(writer, Some(Arc::clone(&hot)), None));
        let mut batch = ServiceBatch::default();
        let mut event = serde_json::Map::new();
        event.insert("message".into(), "in flight".into());
        batch.push(event);
        let groups = pipeline.admit_for_test(indexmap::IndexMap::from([(
            ("prod".to_owned(), "api".to_owned()),
            batch,
        )]));

        let (entered, release) = pipeline.pause_next_insert_for_test();
        let producer = std::thread::spawn({
            let pipeline = Arc::clone(&pipeline);
            move || pipeline.write(groups)
        });
        entered
            .recv_timeout(Duration::from_secs(30))
            .expect("the producer published its WAL file and paused before the insert");
        let in_flight: Vec<String> = std::fs::read_dir(wal_dir.join("prod"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("api_")
            })
            .map(|path| wal_batch_id("prod", &path).unwrap())
            .collect();
        let [in_flight] = in_flight.as_slice() else {
            panic!("one published file: {in_flight:?}");
        };
        assert!(!hot.is_resident(in_flight));

        let (listed_tx, listed_rx) = mpsc::channel();
        let proof = std::thread::spawn({
            let (hot, gate, wal_dir) = (Arc::clone(&hot), Arc::clone(&gate), wal_dir.clone());
            move || prove(&wal_dir, &hot, &gate, |n| listed_tx.send(n).unwrap())
        });
        assert_eq!(
            listed_rx.recv_timeout(Duration::from_secs(30)).unwrap(),
            1,
            "the unguarded listing found the in-flight file not resident"
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !proof.is_finished(),
            "the proof waits for the write guard the paused producer holds off"
        );
        assert_overhang(&gate);

        release.send(()).unwrap();
        assert_eq!(producer.join().unwrap(), 1, "the producer inserted");
        assert_eq!(proof.join().unwrap(), ProofOutcome::Settled);
        assert!(hot.is_resident(in_flight));
        assert_settled(&gate);
    }

    #[test]
    fn outcome_labels_are_stable() {
        let labels = [
            ProofOutcome::Settled,
            ProofOutcome::Incomplete,
            ProofOutcome::MarkerPending,
            ProofOutcome::TooManyCandidates,
            ProofOutcome::NotResident,
        ]
        .map(ProofOutcome::label);
        assert_eq!(
            labels,
            [
                "settled",
                "incomplete",
                "marker_pending",
                "too_many_candidates",
                "not_resident"
            ]
        );
    }
}
