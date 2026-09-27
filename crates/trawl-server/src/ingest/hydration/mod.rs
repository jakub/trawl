// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Boot hydration (ADR-0041 slice 2): load the WAL that survived a restart
//! into the hot buffer under its existing batch identities, before any
//! producer, compaction or the listener starts.
//!
//! [`selection`] decides which files load and [`recognizer`] decides
//! whether a file is exactly what the live writer produced. WAL that
//! existed at boot and did not become resident is the **overhang**: the
//! report says whether there is any, and the boot keeps corpus reads
//! refused until compaction's coverage proof
//! ([`crate::ingest::coverage_proof`]) clears it. Both list the WAL through
//! one walk, [`walk_wal`].
//!
//! Hydration charges the hot buffer's full caps once
//! ([`HotBuffer::hydrate`]). It never publishes to the event bus, never
//! counts as ingested traffic and needs no Postgres.

mod recognizer;
mod selection;

pub(crate) use selection::walk_wal;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crate::bus::IngestBatch;
use crate::hot_buffer::{Charge, HotBuffer, HydrateError, HydratedBatch};

/// What happened to one WAL file at boot: the `outcome` label on
/// [`crate::metrics::HYDRATION_FILES_TOTAL`]. Every outcome but
/// `hydrated` leaves overhang.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HydrationOutcome {
    /// Loaded into the hot buffer.
    Hydrated,
    /// Did not fit what the caps or the read budget had left.
    Capacity,
    /// Larger than a full cap: it can never fit.
    Oversized,
    /// Not exactly what the live writer produces, its name included.
    /// Compaction's decoder takes it.
    Undecodable,
    /// Not a regular file, a symlink, or a read that failed.
    Unreadable,
    /// In a service a pending publication marker blocks.
    Claimed,
    /// A scope that could not be listed, counted once per scope: the WAL
    /// root, a root entry, or an env directory.
    Unlisted,
}

impl HydrationOutcome {
    pub const ALL: [Self; 7] = [
        Self::Hydrated,
        Self::Capacity,
        Self::Oversized,
        Self::Undecodable,
        Self::Unreadable,
        Self::Claimed,
        Self::Unlisted,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Hydrated => "hydrated",
            Self::Capacity => "capacity",
            Self::Oversized => "oversized",
            Self::Undecodable => "undecodable",
            Self::Unreadable => "unreadable",
            Self::Claimed => "claimed",
            Self::Unlisted => "unlisted",
        }
    }
}

/// A count per [`HydrationOutcome`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutcomeCounts([u64; HydrationOutcome::ALL.len()]);

impl OutcomeCounts {
    pub fn get(&self, outcome: HydrationOutcome) -> u64 {
        self.0[outcome as usize]
    }

    fn add(&mut self, outcome: HydrationOutcome, count: u64) {
        let slot = &mut self.0[outcome as usize];
        *slot = slot.saturating_add(count);
    }

    /// Whether any file or scope was left out.
    fn any_left_out(&self) -> bool {
        HydrationOutcome::ALL
            .into_iter()
            .any(|outcome| outcome != HydrationOutcome::Hydrated && self.get(outcome) > 0)
    }
}

/// What one boot hydration did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydrationReport {
    /// Files per outcome; `unlisted` counts scopes.
    pub counts: OutcomeCounts,
    /// More WAL files existed than hydration examines
    /// (`hot_buffer_max_events + 1`).
    pub examine_bound_hit: bool,
    /// What the hydrated files charged the hot buffer.
    pub charge: Charge,
    /// Bytes read from WAL files, rejected files included.
    pub bytes_read: u64,
    /// Some WAL that existed at boot did not become resident: a file was
    /// not hydrated, a scope was claimed or unlisted, or the examine bound
    /// was hit. Corpus reads stay refused until compaction proves coverage.
    pub overhang: bool,
}

/// Load the WAL under `wal_dir` into `hot`, which must be empty and not yet
/// hydrated, and report what was left out. `now` dates the files: a
/// hydrated batch's age is `now` minus the time in its file name, zero for
/// a name in the future.
///
/// Blocking file I/O: call it from a blocking thread, before any producer
/// or compaction starts. It calls [`HotBuffer::hydrate`] exactly once, with
/// an empty plan when nothing fits, so the buffer's one shot is spent
/// either way. An `Err` is a boot bug (the buffer was already hydrated or
/// not empty), never a property of the WAL.
///
/// Records `trawl_hydration_files_total{outcome}` and logs one INFO
/// `boot_hydration` event with the counts, never a path.
pub fn hydrate_at_boot(
    wal_dir: &Path,
    hot: &HotBuffer,
    now: SystemTime,
) -> Result<HydrationReport, HydrateError> {
    let started = Instant::now();
    let caps = Charge {
        events: hot.config().max_events,
        bytes: hot.config().max_bytes,
    };
    let selection = selection::select(wal_dir, caps);
    let batches = selection
        .files
        .into_iter()
        .map(|selected| {
            let age = file_age(now, selected.file.name.millis);
            HydratedBatch {
                batch: Arc::new(IngestBatch {
                    batch_id: selected.batch_id.into(),
                    service: selected.file.name.service.into(),
                    byte_size: selected.file.bytes,
                    events: selected.file.events,
                }),
                age,
            }
        })
        .collect();
    let charge = hot.hydrate(batches)?;

    let counts = selection.counts;
    let report = HydrationReport {
        counts,
        examine_bound_hit: selection.examine_bound_hit,
        charge,
        bytes_read: selection.bytes_read,
        overhang: selection.examine_bound_hit || counts.any_left_out(),
    };
    for outcome in HydrationOutcome::ALL {
        metrics::counter!(crate::metrics::HYDRATION_FILES_TOTAL, "outcome" => outcome.label())
            .increment(counts.get(outcome));
    }
    tracing::info!(
        event_type = "boot_hydration",
        hydrated = counts.get(HydrationOutcome::Hydrated),
        capacity = counts.get(HydrationOutcome::Capacity),
        oversized = counts.get(HydrationOutcome::Oversized),
        undecodable = counts.get(HydrationOutcome::Undecodable),
        unreadable = counts.get(HydrationOutcome::Unreadable),
        claimed = counts.get(HydrationOutcome::Claimed),
        unlisted = counts.get(HydrationOutcome::Unlisted),
        examine_bound_hit = report.examine_bound_hit,
        events = charge.events,
        bytes = charge.bytes,
        bytes_read = report.bytes_read,
        overhang = report.overhang,
        duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "boot hydration loaded the surviving WAL into the hot buffer"
    );
    Ok(report)
}

/// How old a WAL file named at `millis` is at `now`: zero for a name in the
/// future, saturating.
fn file_age(now: SystemTime, millis: u64) -> Duration {
    now.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_sub(Duration::from_millis(millis))
}

/// Make mode bits bind on the calling thread even when the tests run as
/// root, for a test that locks a directory with mode `000`: drop
/// `CAP_DAC_OVERRIDE` and `CAP_DAC_READ_SEARCH` from the thread's effective
/// set. Capabilities belong to a thread, so no other test loses them, and
/// the permitted set is kept.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn enforce_mode_bits_on_this_thread() {
    use rustix::thread::{CapabilitySet, capabilities, set_capabilities};
    let mut sets = capabilities(None).expect("capget on the test thread");
    sets.effective
        .remove(CapabilitySet::DAC_OVERRIDE | CapabilitySet::DAC_READ_SEARCH);
    set_capabilities(None, sets).expect("capset on the test thread");
}

#[cfg(test)]
mod test_support {
    use std::path::Path;

    use serde_json::json;

    use crate::hot_buffer::{HotBuffer, HotBufferConfig};
    use crate::ingest::pipeline::ServiceBatch;
    use crate::ingest::wal::WalName;

    pub(super) fn buffer(max_events: usize, max_bytes: usize) -> HotBuffer {
        HotBuffer::new(HotBufferConfig {
            max_events,
            max_bytes,
        })
    }

    /// Write `count` events through the live writer's line format as
    /// `wal/{env}/{service}_{millis}_0001.ndjson`, returning its batch id.
    pub(super) fn plant(wal: &Path, env: &str, service: &str, millis: u64, count: u32) -> String {
        let mut batch = ServiceBatch::default();
        for n in 0..count {
            let mut event = serde_json::Map::new();
            event.insert("n".into(), json!(n));
            event.insert("service".into(), json!(service));
            batch.push(event);
        }
        let name = WalName {
            service: service.into(),
            millis,
            nonce: 1,
        };
        std::fs::create_dir_all(wal.join(env)).unwrap();
        std::fs::write(wal.join(env).join(name.file_name()), &batch.ndjson).unwrap();
        format!("{env}/{}", name.file_name().trim_end_matches(".ndjson"))
    }

    /// Unix millis of `time`.
    pub(super) fn millis(time: std::time::SystemTime) -> u64 {
        u64::try_from(
            time.duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::test_support::{buffer, plant};
    use super::*;
    use crate::metrics::test_support::sample;

    fn series(outcome: HydrationOutcome) -> String {
        format!(
            "{}{{outcome=\"{}\"}}",
            crate::metrics::HYDRATION_FILES_TOTAL,
            outcome.label()
        )
    }

    #[test]
    fn hydrates_under_the_batch_identity_and_records_each_outcome() {
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            for outcome in HydrationOutcome::ALL {
                assert_eq!(sample(&handle, &series(outcome)), 0, "{outcome:?}");
            }
            let tmp = tempfile::tempdir().unwrap();
            let wal = tmp.path();
            let now = SystemTime::now();
            let base = test_support::millis(now) - 60_000;
            let first = plant(wal, "prod", "api", base, 2);
            let second = plant(wal, "lab", "web", base + 1, 3);
            // Four events do not fit the one left after the first two files.
            let left_out = plant(wal, "prod", "big", base + 2, 4);
            std::fs::write(wal.join("prod").join("stray.ndjson"), b"{}\n").unwrap();
            let hot = buffer(6, 1 << 20);

            let report = hydrate_at_boot(wal, &hot, now).unwrap();
            assert!(hot.is_resident(&first));
            assert!(hot.is_resident(&second));
            assert!(!hot.is_resident(&left_out));
            assert_eq!(report.charge.events, 5);
            assert_eq!(hot.charged(), report.charge);
            assert!(report.overhang);
            assert!(!report.examine_bound_hit);
            for (outcome, expected) in [
                (HydrationOutcome::Hydrated, 2),
                (HydrationOutcome::Capacity, 1),
                (HydrationOutcome::Undecodable, 1),
            ] {
                assert_eq!(report.counts.get(outcome), expected, "{outcome:?}");
            }
            for outcome in HydrationOutcome::ALL {
                assert_eq!(
                    sample(&handle, &series(outcome)),
                    report.counts.get(outcome),
                    "{outcome:?}"
                );
            }
        });
    }

    #[test]
    fn a_wal_that_fits_whole_leaves_no_overhang() {
        let tmp = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let id = plant(tmp.path(), "prod", "api", test_support::millis(now), 3);
        let hot = buffer(100, 1 << 20);
        let report = hydrate_at_boot(tmp.path(), &hot, now).unwrap();
        assert!(!report.overhang, "{report:?}");
        assert!(hot.is_resident(&id));
        assert_eq!(report.counts.get(HydrationOutcome::Hydrated), 1);
    }

    #[test]
    fn nothing_to_hydrate_still_spends_the_one_shot() {
        let tmp = tempfile::tempdir().unwrap();
        let hot = buffer(100, 1 << 20);
        let report = hydrate_at_boot(&tmp.path().join("missing"), &hot, SystemTime::now()).unwrap();
        assert!(!report.overhang);
        assert_eq!(report.charge, Charge::ZERO);
        assert_eq!(
            hydrate_at_boot(tmp.path(), &hot, SystemTime::now()),
            Err(HydrateError::AlreadyHydrated)
        );
    }

    #[test]
    fn hitting_the_examine_bound_is_overhang() {
        let tmp = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let base = test_support::millis(now);
        for n in 0..3 {
            plant(tmp.path(), "prod", &format!("s{n}"), base + n, 1);
        }
        // Bound 2: the third file is never examined, so it has no outcome.
        let hot = buffer(1, 1 << 20);
        let report = hydrate_at_boot(tmp.path(), &hot, now).unwrap();
        assert!(report.examine_bound_hit);
        assert!(report.overhang);
        assert_eq!(report.counts.get(HydrationOutcome::Hydrated), 1);
        assert_eq!(report.counts.get(HydrationOutcome::Capacity), 1);
        assert_eq!(report.charge.events, 1);
    }
}

#[cfg(test)]
mod age {
    //! A hydrated batch is as old as its WAL file (ADR-0041): the hot
    //! buffer's oldest-batch age, which the
    //! `trawl_hot_buffer_oldest_batch_age_seconds` gauge reports, starts
    //! from the time in the file name.

    use std::time::{Duration, Instant, SystemTime};

    use super::test_support::{buffer, millis, plant};
    use super::*;

    #[test]
    fn the_oldest_hydrated_batch_is_at_least_as_old_as_its_file() {
        let tmp = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let hour_ago = millis(now) - 3_600_000;
        plant(tmp.path(), "prod", "api", hour_ago + 3_000_000, 1);
        let oldest = plant(tmp.path(), "lab", "api", hour_ago, 1);
        let hot = buffer(100, 1 << 20);
        hydrate_at_boot(tmp.path(), &hot, now).unwrap();
        assert!(hot.is_resident(&oldest));
        let age = hot.oldest_batch_age().unwrap();
        assert!(age >= Duration::from_secs(3_600), "{age:?}");
    }

    #[test]
    fn a_file_named_in_the_future_is_zero_old() {
        let tmp = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let id = plant(tmp.path(), "prod", "api", millis(now) + 3_600_000, 1);
        let hot = buffer(100, 1 << 20);
        let before = Instant::now();
        hydrate_at_boot(tmp.path(), &hot, now).unwrap();
        assert!(hot.is_resident(&id));
        // Only the time since the install, no back-dating.
        assert!(hot.oldest_batch_age().unwrap() <= before.elapsed());
    }

    #[test]
    fn file_age_saturates() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_millis(10_000);
        assert_eq!(file_age(now, 4_000), Duration::from_millis(6_000));
        assert_eq!(file_age(now, 10_000), Duration::ZERO);
        assert_eq!(file_age(now, 10_001), Duration::ZERO);
        assert_eq!(file_age(now, u64::MAX), Duration::ZERO);
        assert_eq!(file_age(SystemTime::UNIX_EPOCH, 0), Duration::ZERO);
        // A clock before the epoch dates nothing.
        let before_epoch = SystemTime::UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(file_age(before_epoch, 0), Duration::ZERO);
    }
}
