// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Which surviving WAL files boot hydration loads (ADR-0041 slice 2).
//!
//! The scope is the WAL that compaction's zero-age scan takes: every env
//! directory [`try_list_env_dirs_observed`] finds, environments outside the
//! ingest allowlist included, and every `.ndjson` entry
//! [`compaction::scan_wal_files`] lists in it, grouped by service as
//! [`compaction::group_by_service`] groups them. A service that a
//! publication marker blocks ([`PublicationClaims::blocks_service`]) is
//! left out and its files count as `claimed`. A scope that cannot be listed
//! counts once as `unlisted`: the WAL root, a root entry that cannot be
//! inspected, or an env directory.
//!
//! Listing names costs what one compaction scan costs. Examining files is
//! bounded: a max-heap keeps the oldest `max_events + 1` writer-named
//! candidates by `(millis, env, file name)`, and only those are opened.
//! More candidates than that is overhang (`examine_bound_hit`), since at
//! most `max_events` non-empty files fit. A `.ndjson` entry without a
//! writer name is `undecodable` without being opened.
//!
//! The kept candidates are tried oldest first, first fit, against the full
//! caps and a read budget of `max_bytes`. Every byte read counts against the
//! budget, a rejected file's included. A file that does not fit is skipped,
//! and later files are still tried:
//!
//! - longer than the full byte cap: `oversized`, not read;
//! - longer than the remaining budget or byte capacity: `capacity`, not read;
//! - not a regular file, a symlink, or a read that failed or changed
//!   length: `unreadable`;
//! - not writer output ([`recognizer::recognize`]): `undecodable`;
//! - more events than the full event cap: `oversized`;
//! - more events than the remaining event capacity: `capacity`;
//! - otherwise `hydrated`.
//!
//! The same WAL gives the same selection: the order is total and the
//! decisions depend only on the files' bytes.

use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::recognizer::{self, Recognized, Rejection};
use super::{HydrationOutcome, OutcomeCounts};
use crate::env_dirs::try_list_env_dirs_observed;
use crate::hot_buffer::Charge;
use crate::ingest::compaction;
use crate::ingest::publication_marker::{self, PublicationClaims};
use crate::ingest::wal::WalName;

/// What [`select`] found in the WAL.
#[derive(Debug, Default)]
pub(crate) struct Selection {
    /// The files to hydrate, oldest first.
    pub files: Vec<Selected>,
    /// Files per outcome, `hydrated` included. `unlisted` counts scopes.
    pub counts: OutcomeCounts,
    /// More writer-named candidates existed than were examined.
    pub examine_bound_hit: bool,
    /// Bytes read from WAL files, rejected files included; at most the
    /// full byte cap.
    pub bytes_read: u64,
}

/// One WAL file recognized in full, which fits.
#[derive(Debug)]
pub(crate) struct Selected {
    /// The file's hot batch id ([`compaction::wal_batch_id`]), the key the
    /// drain after its publish uses.
    pub batch_id: String,
    pub file: Recognized,
}

/// A writer-named WAL file, ordered oldest first by the time in its name,
/// then by env and file name, which makes the order total.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Candidate {
    millis: u64,
    env: String,
    file_name: String,
}

/// Select the WAL files under `wal_dir` that fit `caps`, the hot buffer's
/// full caps.
pub(crate) fn select(wal_dir: &Path, caps: Charge) -> Selection {
    let mut selection = Selection::default();
    let candidates = list(
        wal_dir,
        caps.events.saturating_add(1),
        &mut selection.counts,
        &mut selection.examine_bound_hit,
    );
    let full_bytes = caps.bytes as u64;
    let mut remaining = caps;
    for candidate in candidates {
        let path = wal_dir.join(&candidate.env).join(&candidate.file_name);
        // Every hydrated byte was read, so the budget left never exceeds
        // the byte capacity left; both bound the read.
        let limit = full_bytes
            .saturating_sub(selection.bytes_read)
            .min(remaining.bytes as u64);
        let outcome = match recognizer::examine(&path, limit, &mut selection.bytes_read) {
            Err(Rejection::Name | Rejection::Unrecognized) => HydrationOutcome::Undecodable,
            Err(Rejection::Unreadable) => HydrationOutcome::Unreadable,
            Err(Rejection::TooLong { len }) if len > full_bytes => HydrationOutcome::Oversized,
            Err(Rejection::TooLong { .. }) => HydrationOutcome::Capacity,
            Ok(file) if file.events.len() > caps.events => HydrationOutcome::Oversized,
            Ok(file) if file.events.len() > remaining.events => HydrationOutcome::Capacity,
            Ok(file) => match compaction::wal_batch_id(&candidate.env, &path) {
                // A writer name always has a UTF-8 stem.
                None => HydrationOutcome::Undecodable,
                Some(batch_id) => {
                    // The limit bounded the bytes and the guard above the
                    // events, so neither subtraction underflows.
                    remaining.events -= file.events.len();
                    remaining.bytes -= file.bytes;
                    selection.files.push(Selected { batch_id, file });
                    HydrationOutcome::Hydrated
                }
            },
        };
        selection.counts.add(outcome, 1);
    }
    selection
}

/// What [`walk_wal`] saw of the WAL besides its files.
#[derive(Debug)]
pub(crate) struct WalWalk {
    /// Scopes that could not be listed, each counted once: a WAL root entry
    /// that could not be inspected, or an env directory.
    pub unlisted: u64,
    /// The pending publication markers, from one scan of the WAL root.
    pub claims: PublicationClaims,
}

/// Walk the WAL under `wal_dir` in compaction's zero-age scope: every env
/// directory [`try_list_env_dirs_observed`] finds and every `.ndjson`
/// entry [`compaction::scan_wal_files`] lists in it. `visit` gets each
/// `(env, service)` group, grouped as [`compaction::group_by_service`]
/// groups it, and whether a publication marker blocks the service
/// ([`PublicationClaims::blocks_service`]).
///
/// This one listing is the WAL boot hydration selects from and the WAL
/// compaction's coverage proof checks (ADR-0041 slice 2), so the two agree
/// on what existed. Listing names costs what one compaction scan costs;
/// nothing is opened.
///
/// `None` when the WAL root or its publication markers could not be
/// listed: that hides every scope, and nothing was visited. A missing root
/// is a cold start: an empty walk.
pub(crate) fn walk_wal(
    wal_dir: &Path,
    mut visit: impl FnMut(&str, &str, Vec<PathBuf>, bool),
) -> Option<WalWalk> {
    let claims = publication_marker::scan_claims(wal_dir).ok()?;
    let mut unlisted = 0;
    let envs = try_list_env_dirs_observed(wal_dir, || unlisted += 1).ok()?;
    for (env, env_dir) in envs {
        let Ok(files) = compaction::scan_wal_files(&env_dir, Duration::ZERO) else {
            unlisted += 1;
            continue;
        };
        for (service, files) in compaction::group_by_service(files) {
            let blocked = claims.blocks_service(&env, &service);
            visit(&env, &service, files, blocked);
        }
    }
    Some(WalWalk { unlisted, claims })
}

/// List the WAL in compaction's zero-age scope ([`walk_wal`]) and keep the
/// oldest `bound` writer-named candidates, oldest first. Every other entry
/// is counted here: `claimed`, `unlisted` and non-writer names as
/// `undecodable`. Candidates beyond the bound set `bound_hit` and are not
/// counted.
fn list(
    wal_dir: &Path,
    bound: usize,
    counts: &mut OutcomeCounts,
    bound_hit: &mut bool,
) -> Vec<Candidate> {
    let mut heap = BinaryHeap::new();
    let walk = walk_wal(wal_dir, |env, _service, files, blocked| {
        if blocked {
            counts.add(HydrationOutcome::Claimed, files.len() as u64);
        } else {
            keep_oldest(env, files, bound, &mut heap, counts, bound_hit);
        }
    });
    // An unreadable root hides every scope, markers included.
    counts.add(
        HydrationOutcome::Unlisted,
        walk.map_or(1, |walk| walk.unlisted),
    );
    heap.into_sorted_vec()
}

/// Push one unclaimed service's writer-named files into `heap`, dropping
/// the newest candidate whenever it holds more than `bound`.
fn keep_oldest(
    env: &str,
    files: Vec<PathBuf>,
    bound: usize,
    heap: &mut BinaryHeap<Candidate>,
    counts: &mut OutcomeCounts,
    bound_hit: &mut bool,
) {
    for path in files {
        let Some(name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(WalName::parse)
        else {
            counts.add(HydrationOutcome::Undecodable, 1);
            continue;
        };
        heap.push(Candidate {
            millis: name.millis,
            env: env.to_owned(),
            file_name: name.file_name(),
        });
        if heap.len() > bound {
            heap.pop();
            *bound_hit = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ingest::pipeline::ServiceBatch;

    const HOUR_MS: u64 = 3_600_000;
    const BASE: u64 = 1_790_000_000_000;

    /// `count` events from the live writer, each padded with `pad` bytes
    /// of message text.
    fn batch(count: usize, pad: usize) -> ServiceBatch {
        let mut batch = ServiceBatch::default();
        for n in 0..u32::try_from(count).unwrap() {
            let mut event = serde_json::Map::new();
            event.insert("n".into(), json!(n));
            event.insert("msg".into(), json!("x".repeat(pad)));
            event.insert("ratio".into(), json!(0.1 * f64::from(n)));
            batch.push(event);
        }
        batch
    }

    /// Write `batch` as the WAL file `wal/{env}/{service}_{millis}_{nonce}`.
    fn plant(wal: &Path, env: &str, service: &str, millis: u64, batch: &ServiceBatch) -> String {
        let name = WalName {
            service: service.into(),
            millis,
            nonce: 0x0001,
        }
        .file_name();
        let dir = wal.join(env);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(&name), &batch.ndjson).unwrap();
        format!("{env}/{name}")
    }

    /// The selected files as `{env}/{file name}`, in selection order.
    fn selected(selection: &Selection) -> Vec<String> {
        selection
            .files
            .iter()
            .map(|s| format!("{}.ndjson", s.batch_id))
            .collect()
    }

    fn caps(events: usize, bytes: usize) -> Charge {
        Charge { events, bytes }
    }

    fn count(selection: &Selection, outcome: HydrationOutcome) -> u64 {
        selection.counts.get(outcome)
    }

    /// Every outcome's count, in [`HydrationOutcome::ALL`] order.
    fn counts(selection: &Selection) -> Vec<(&'static str, u64)> {
        HydrationOutcome::ALL
            .iter()
            .map(|&o| (o.label(), selection.counts.get(o)))
            .filter(|&(_, n)| n > 0)
            .collect()
    }

    #[test]
    fn oldest_first_then_first_fit() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path();
        // Planted newest first, so directory order is not file-name order.
        let d = plant(wal, "prod", "d", BASE + 3 * HOUR_MS, &batch(1, 8));
        let b = plant(wal, "prod", "b", BASE + 2 * HOUR_MS, &batch(5, 8));
        // Same millis as `b`: `lab` sorts before `prod`.
        let c = plant(wal, "lab", "c", BASE + 2 * HOUR_MS, &batch(3, 8));
        let a = plant(wal, "prod", "a", BASE + HOUR_MS, &batch(4, 8));

        let selection = select(wal, caps(8, 1 << 20));
        // a (4) fits, c (3) fits, b (5) does not fit the 1 left, d (1) does.
        assert_eq!(selected(&selection), [a.as_str(), &c, &d]);
        assert_eq!(counts(&selection), [("hydrated", 3), ("capacity", 1)]);
        assert!(!selection.examine_bound_hit);
        // b was read before its event count refused it.
        let all: u64 = [&a, &b, &c, &d]
            .iter()
            .map(|name| std::fs::metadata(wal.join(name)).unwrap().len())
            .sum();
        assert_eq!(selection.bytes_read, all);
    }

    #[test]
    fn the_selected_files_carry_the_writer_charge() {
        let tmp = tempfile::tempdir().unwrap();
        let written = batch(3, 20);
        plant(tmp.path(), "prod", "svc", BASE, &written);
        let selection = select(tmp.path(), caps(100, 1 << 20));
        let [file] = selection.files.as_slice() else {
            panic!("one file: {selection:?}");
        };
        assert_eq!(file.file.events, written.maps);
        assert_eq!(
            Charge {
                events: file.file.events.len(),
                bytes: file.file.bytes,
            },
            written.charge()
        );
        let path = tmp.path().join("prod").join(file.file.name.file_name());
        assert_eq!(
            Some(file.batch_id.as_str()),
            compaction::wal_batch_id("prod", &path).as_deref()
        );
    }

    #[test]
    fn an_oversized_oldest_file_is_skipped_and_later_files_hydrate() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path();
        // Over the full event cap: read, then refused on its count.
        plant(wal, "prod", "big", BASE, &batch(6, 1));
        let b = plant(wal, "prod", "b", BASE + 1, &batch(2, 1));
        let c = plant(wal, "prod", "c", BASE + 2, &batch(2, 1));
        let selection = select(wal, caps(5, 1 << 20));
        assert_eq!(selected(&selection), [b, c]);
        assert_eq!(counts(&selection), [("hydrated", 2), ("oversized", 1)]);

        // Over the full byte cap: refused on its length, never read.
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path();
        let small = batch(1, 10);
        let cap = 3 * small.ndjson.len();
        let long = batch(1, cap);
        assert!(long.ndjson.len() > cap);
        plant(wal, "prod", "long", BASE, &long);
        let b = plant(wal, "prod", "b", BASE + 1, &small);
        let c = plant(wal, "prod", "c", BASE + 2, &small);
        let selection = select(wal, caps(100, cap));
        assert_eq!(selected(&selection), [b, c]);
        assert_eq!(counts(&selection), [("hydrated", 2), ("oversized", 1)]);
        assert_eq!(selection.bytes_read, 2 * small.ndjson.len() as u64);
    }

    #[test]
    fn a_file_between_fifteen_sixteenths_and_the_full_cap_hydrates() {
        let tmp = tempfile::tempdir().unwrap();
        let written = batch(97, 40);
        let name = plant(tmp.path(), "prod", "svc", BASE, &written);
        let len = written.ndjson.len();
        let full = caps(100, len + len / 32);
        let ceiling = crate::hot_buffer::external_ceiling(full);
        assert!(written.charge().events > ceiling.events);
        assert!(written.charge().bytes > ceiling.bytes);
        let selection = select(tmp.path(), full);
        assert_eq!(counts(&selection), [("hydrated", 1)]);
        assert_eq!(selected(&selection), [name]);

        // Exactly the full cap still fits.
        let selection = select(tmp.path(), caps(97, len));
        assert_eq!(counts(&selection), [("hydrated", 1)]);
    }

    #[test]
    fn examines_at_most_max_events_plus_one_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path();
        let one = batch(1, 4);
        let kept: Vec<String> = (0..4)
            .map(|n| plant(wal, "prod", &format!("s{n}"), BASE + n, &one))
            .collect();
        // Newer entries that would count as unreadable if they were opened.
        for n in 4..10 {
            let name = WalName {
                service: format!("s{n}"),
                millis: BASE + n,
                nonce: 1,
            }
            .file_name();
            std::fs::create_dir(wal.join("prod").join(name)).unwrap();
        }
        let selection = select(wal, caps(3, 1 << 20));
        assert_eq!(selected(&selection), kept[..3]);
        assert_eq!(counts(&selection), [("hydrated", 3), ("capacity", 1)]);
        assert!(selection.examine_bound_hit);

        // Exactly `max_events + 1` candidates: nothing beyond the bound.
        let tmp = tempfile::tempdir().unwrap();
        for n in 0..4 {
            plant(tmp.path(), "prod", &format!("s{n}"), BASE + n, &one);
        }
        // A non-writer name takes no candidate slot and is never opened.
        std::fs::create_dir(tmp.path().join("prod").join("stray.ndjson")).unwrap();
        let selection = select(tmp.path(), caps(3, 1 << 20));
        assert_eq!(
            counts(&selection),
            [("hydrated", 3), ("capacity", 1), ("undecodable", 1)]
        );
        assert!(!selection.examine_bound_hit);
    }

    #[test]
    fn reads_at_most_max_bytes_counting_rejected_files() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path();
        let cap = 1000;
        // 600 bytes that are not writer output: a blank line at the end.
        let mut bad = batch(1, 540).ndjson;
        bad.push(b'\n');
        let bad_name = WalName {
            service: "bad".into(),
            millis: BASE,
            nonce: 1,
        }
        .file_name();
        std::fs::create_dir_all(wal.join("prod")).unwrap();
        std::fs::write(wal.join("prod").join(bad_name), &bad).unwrap();
        let fits_capacity_not_budget = batch(1, 440);
        let small = batch(1, 240);
        assert!(bad.len() + fits_capacity_not_budget.ndjson.len() > cap);
        assert!(fits_capacity_not_budget.ndjson.len() + small.ndjson.len() <= cap);
        assert!(bad.len() + small.ndjson.len() <= cap);
        plant(wal, "prod", "mid", BASE + 1, &fits_capacity_not_budget);
        let small_name = plant(wal, "prod", "small", BASE + 2, &small);

        let selection = select(wal, caps(100, cap));
        assert_eq!(selected(&selection), [small_name]);
        assert_eq!(
            counts(&selection),
            [("hydrated", 1), ("capacity", 1), ("undecodable", 1)]
        );
        assert_eq!(
            selection.bytes_read,
            (bad.len() + small.ndjson.len()) as u64
        );
        assert!(selection.bytes_read <= cap as u64);
    }

    /// Plant the same WAL in `wal`, in the given file order.
    fn plant_mixed(wal: &Path, order: impl Iterator<Item = usize>) {
        for n in order {
            let env = ["lab", "prod", "stage"][n % 3];
            // Sizes vary with n so first fit skips some files.
            let written = batch(1 + n * 7 % 5, 10 + n * 13 % 90);
            // Every fourth file shares its millis with the one before.
            let millis = BASE + u64::try_from(n - usize::from(n % 4 == 0)).unwrap();
            plant(wal, env, &format!("svc{n}"), millis, &written);
        }
    }

    #[test]
    fn the_same_wal_selects_the_same_files() {
        let forward = tempfile::tempdir().unwrap();
        plant_mixed(forward.path(), 1..40);
        let backward = tempfile::tempdir().unwrap();
        plant_mixed(backward.path(), (1..40).rev());
        let limits = caps(30, 2000);
        let first = select(forward.path(), limits);
        let second = select(forward.path(), limits);
        let reversed = select(backward.path(), limits);
        assert!(first.examine_bound_hit, "{first:?}");
        assert!(count(&first, HydrationOutcome::Hydrated) > 1);
        assert!(count(&first, HydrationOutcome::Capacity) > 0);
        for other in [&second, &reversed] {
            assert_eq!(selected(other), selected(&first));
            assert_eq!(counts(other), counts(&first));
            assert_eq!(other.bytes_read, first.bytes_read);
        }
    }

    #[test]
    fn claimed_and_unlisted_scopes_are_not_hydrated() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path();
        let one = batch(1, 4);
        let api = plant(wal, "prod", "api", BASE, &one);
        plant(wal, "prod", "nginx", BASE + 1, &one);
        plant(wal, "prod", "nginx", BASE + 2, &one);
        // Any marker blocks its service, readable or not.
        std::fs::write(
            wal.join("prod")
                .join(publication_marker::marker_file_name("nginx")),
            b"not a marker",
        )
        .unwrap();
        // The same service in another env is not blocked.
        let lab = plant(wal, "lab", "nginx", BASE + 3, &one);
        // A root entry that cannot be inspected is one unlisted scope.
        std::os::unix::fs::symlink("cycle", wal.join("cycle")).unwrap();

        let selection = select(wal, caps(100, 1 << 20));
        assert_eq!(selected(&selection), [api, lab]);
        assert_eq!(
            counts(&selection),
            [("hydrated", 2), ("claimed", 2), ("unlisted", 1)]
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_env_that_cannot_be_listed_is_one_unlisted_scope() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path();
        let one = batch(1, 4);
        let prod = plant(wal, "prod", "api", BASE, &one);
        plant(wal, "locked", "api", BASE + 1, &one);
        plant(wal, "locked", "web", BASE + 2, &one);
        let locked = wal.join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&locked).is_ok() {
            // Running as root: mode bits are not enforced.
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let selection = select(wal, caps(100, 1 << 20));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(selected(&selection), [prod]);
        assert_eq!(counts(&selection), [("hydrated", 1), ("unlisted", 1)]);
    }

    #[test]
    fn an_unreadable_root_is_one_unlisted_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("wal");
        std::fs::write(&root, b"not a directory").unwrap();
        let selection = select(&root, caps(100, 1 << 20));
        assert!(selection.files.is_empty());
        assert_eq!(counts(&selection), [("unlisted", 1)]);

        // A missing root is a cold start: nothing to hydrate, no overhang.
        let selection = select(&tmp.path().join("missing"), caps(100, 1 << 20));
        assert!(counts(&selection).is_empty());
    }
}
