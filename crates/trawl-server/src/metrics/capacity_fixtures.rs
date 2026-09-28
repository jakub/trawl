// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Health page's capacity fixtures are producer output (ADR-0042).
//!
//! Each scenario plants its partitions as sparse `.parquet` files of the
//! scenario's byte sizes under a temporary data root, then reads them the
//! way the dashboard does: the Parquet attempt ([`super::scan_parquet`],
//! its repin fence included) through a [`super::StorageCache`] read back by
//! [`super::read_parquet_scan`], the reader behind the dashboard's
//! Parquet totals. A headroom attempt through `capacity::sample_headroom`
//! runs over a stat seam on the same data root, retention evidence is
//! recorded through `retention::RetentionEvidence`, and the headroom and
//! WAL measurements are fixed inputs. `capacity::assemble` turns them into
//! the capacity object on 2026-09-27, the same call the dashboard snapshot
//! makes. The test serializes each scenario's snapshot slice and compares
//! it with the committed fixture the e2e harness loads, so a fixture is a
//! state the server can emit by construction, and a storage scan
//! regression fails it.
//!
//! A lib unit test rather than an integration test: as a child of
//! `metrics`, it reaches the private scan and cache without widening
//! their visibility.
//!
//! To regenerate the fixtures after a deliberate change to the producer or
//! a scenario, run
//! `TRAWL_REGEN_CAPACITY_FIXTURES=1 cargo test -p trawl-server --lib capacity_fixtures`:
//! it rewrites each fixture that differs instead of failing, the base
//! snapshot's slice into `health-dashboard.json` included, and leaves a
//! matching one untouched. Review the diff, then rerun the health-page
//! spec and recapture per `visual-evidence/issue-202/README.md`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{Days, NaiveDate};
use trawl_api::{Capacity, StorageMeasurement, StorageMeasurementStatus as Status, SweepOutcome};

use super::{StorageCache, StorageScan, read_parquet_scan, scan_parquet};
use crate::capacity::{CapacityReadings, EnvDateBytes, HeadroomSample, assemble, sample_headroom};
use crate::repin::JobGeneration;
use crate::retention::{RemovalTrigger, RetentionEvidence};

const M: u64 = 1_000_000;
const G: u64 = 1_000_000_000;
const GIB: u64 = 1_073_741_824;
const REGEN: &str = "TRAWL_REGEN_CAPACITY_FIXTURES";

fn date(month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, month, day).unwrap()
}

/// The UTC date every fixture is assembled on.
fn today() -> NaiveDate {
    date(9, 27)
}

/// The oldest observed day: today − 8.
fn window_start() -> NaiveDate {
    date(9, 19)
}

/// The snapshot slice a capacity fixture carries: the capacity object
/// with the Parquet and WAL fields it was assembled beside, each the
/// `DashboardSnapshot` field of the same name, in its order.
#[derive(serde::Serialize)]
struct CapacitySnapshotPart {
    parquet_files: u64,
    parquet_bytes: u64,
    parquet_measurement: StorageMeasurement,
    wal_files: u64,
    wal_bytes: u64,
    wal_measurement: StorageMeasurement,
    capacity: Capacity,
}

/// One filesystem as the stat seam reports it: `(device, total, available)`.
type Stat = (u64, u64, u64);

/// A headroom attempt: each role's filesystem, and whether a repin marker
/// stands in the data root while the fence reads it. The marker is planted
/// after the Parquet attempt, so only the headroom sample sees it.
struct Headroom {
    data: Stat,
    /// `None` when ingest is off.
    wal: Option<Stat>,
    spill: Stat,
    repin: bool,
}

/// Retention's evidence as the loop records it.
struct Evidence {
    removals_age: u64,
    removals_disk_pressure: u64,
    pressure_attempts: u64,
    /// The last tick's outcome and its age at the snapshot.
    last_sweep: Option<(SweepOutcome, u64)>,
}

impl Evidence {
    const NONE: Self = Self {
        removals_age: 0,
        removals_disk_pressure: 0,
        pressure_attempts: 0,
        last_sweep: None,
    };
}

/// What the Parquet cache holds when the snapshot reads it.
#[derive(Clone, Copy)]
enum Parquet {
    /// No attempt yet.
    NotSampled,
    /// One attempt over the planted data root, `age` seconds before the
    /// read.
    Complete { age: u64 },
    /// One attempt over a data root that does not exist, with no complete
    /// scan before it.
    FailedFirst,
}

struct Scenario {
    /// The partitions planted in the data root; `None` plants none.
    partitions: Option<EnvDateBytes>,
    parquet: Parquet,
    headroom_measurement: StorageMeasurement,
    /// The complete sample the headroom cache holds; `None` before one.
    headroom: Option<Headroom>,
    wal: (u64, u64, StorageMeasurement),
    evidence: Evidence,
    floor: u64,
    /// Per-env `max_age_days` overrides of the 90-day default.
    env_max_age: &'static [(&'static str, u64)],
}

fn measurement(status: Status, age: Option<u64>) -> StorageMeasurement {
    StorageMeasurement {
        status,
        sample_age_secs: age,
    }
}

fn complete(age: u64) -> StorageMeasurement {
    measurement(Status::Complete, Some(age))
}

/// `env`'s partitions from `from` through `to`, each of `bytes(date)`.
fn fill(
    partitions: &mut EnvDateBytes,
    env: &str,
    from: NaiveDate,
    to: NaiveDate,
    bytes: impl Fn(NaiveDate) -> u64,
) {
    let mut day = from;
    while day <= to {
        partitions.insert((env.to_owned(), day), bytes(day));
        day = day.checked_add_days(Days::new(1)).unwrap();
    }
}

/// Plant each partition as one sparse `.parquet` file of its bytes under
/// `data_root/{env}/{date}/`. `set_len` writes no data, so a scenario's
/// gigabytes cost no disk.
fn plant(data_root: &Path, partitions: &EnvDateBytes) {
    for ((env, date), bytes) in partitions {
        let dir = data_root.join(env).join(date.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::File::create(dir.join("part.parquet"))
            .unwrap()
            .set_len(*bytes)
            .unwrap();
    }
}

/// The Parquet cache after the scenario's attempts, read back the way the
/// dashboard reads it.
fn parquet_reading(
    parquet: Parquet,
    data_root: &Path,
    jobs: &JobGeneration,
) -> (super::StorageSnapshot, Option<std::sync::Arc<StorageScan>>) {
    let cache = StorageCache::<std::sync::Arc<StorageScan>>::default();
    let attempted = Instant::now();
    let mut read_at = attempted;
    let root = match parquet {
        Parquet::NotSampled => None,
        Parquet::Complete { age } => {
            read_at = attempted + Duration::from_secs(age);
            Some(data_root.to_owned())
        }
        Parquet::FailedFirst => Some(data_root.join("missing")),
    };
    if let Some(root) = root {
        cache.collect(
            || attempted,
            || scan_parquet(&root, jobs).map(std::sync::Arc::new),
            |_| {},
        );
    }
    read_parquet_scan(&cache, read_at)
}

/// One headroom attempt through the real sampling function and repin
/// fence, on the scenario's data root.
fn sample(
    headroom: &Headroom,
    tmp: &Path,
    data_root: &Path,
    jobs: &JobGeneration,
) -> HeadroomSample {
    if headroom.repin {
        std::fs::write(crate::repin::marker::marker_path(data_root), "{}").unwrap();
    }
    let wal_dir = tmp.join("wal");
    let spill_dir = tmp.join("spill");
    let stat = |path: &Path| {
        Ok(if path == data_root {
            headroom.data
        } else if path == wal_dir {
            headroom.wal.expect("no WAL role without a WAL")
        } else {
            assert_eq!(path, spill_dir);
            headroom.spill
        })
    };
    sample_headroom(
        data_root,
        headroom.wal.map(|_| wal_dir.as_path()),
        &spill_dir,
        jobs,
        stat,
    )
    .unwrap()
}

/// Retention's evidence, recorded through the loop's own methods and read
/// back through the snapshot the dashboard takes.
fn pressure(evidence: &Evidence) -> trawl_api::PressureEvidence {
    let recorded = RetentionEvidence::default();
    for _ in 0..evidence.removals_age {
        recorded.record_removal(RemovalTrigger::Age);
    }
    for _ in 0..evidence.removals_disk_pressure {
        recorded.record_removal(RemovalTrigger::DiskPressure);
    }
    for _ in 0..evidence.pressure_attempts {
        recorded.record_pressure_attempt();
    }
    let mut now = Instant::now();
    if let Some((outcome, age_secs)) = evidence.last_sweep {
        recorded.record_sweep(outcome);
        // Read the clock after the record, so the age is at least
        // `age_secs` and under a second more.
        now = Instant::now() + Duration::from_secs(age_secs);
    }
    recorded.snapshot(now)
}

fn retention(scenario: &Scenario) -> trawl_config::RetentionConfig {
    let mut config = trawl_config::RetentionConfig {
        max_age_days: 90,
        min_free_disk_bytes: scenario.floor,
        ..trawl_config::RetentionConfig::default()
    };
    for (env, max_age_days) in scenario.env_max_age {
        config.env.insert(
            (*env).to_owned(),
            trawl_config::EnvRetention {
                max_age_days: *max_age_days,
            },
        );
    }
    config
}

fn produce(scenario: &Scenario) -> CapacitySnapshotPart {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = tmp.path().join("data");
    std::fs::create_dir(&data_root).unwrap();
    if let Some(partitions) = &scenario.partitions {
        plant(&data_root, partitions);
    }
    let jobs = JobGeneration::default();
    let (parquet, scan) = parquet_reading(scenario.parquet, &data_root, &jobs);
    let sample = scenario
        .headroom
        .as_ref()
        .map(|h| sample(h, tmp.path(), &data_root, &jobs));
    let capacity = assemble(
        today(),
        CapacityReadings {
            parquet: &parquet.measurement,
            env_dates: scan.as_deref().map(|scan| &scan.env_dates),
            parquet_repin_in_flight: scan.as_deref().is_some_and(|scan| scan.repin_in_flight),
            headroom: &scenario.headroom_measurement,
            sample: sample.as_ref(),
        },
        pressure(&scenario.evidence),
        &retention(scenario),
    );
    let (wal_files, wal_bytes, wal_measurement) = scenario.wal;
    CapacitySnapshotPart {
        parquet_files: parquet.files,
        parquet_bytes: parquet.bytes,
        parquet_measurement: parquet.measurement,
        wal_files,
        wal_bytes,
        wal_measurement,
        capacity,
    }
}

/// Complete, floor 1 GiB: two rows, a completed sweep, three rated envs
/// sharing one fraction per end, a keep-forever env, and a rate-less env.
fn complete_scenario() -> Scenario {
    let mut p = EnvDateBytes::new();
    let start = window_start();
    fill(&mut p, "archive", date(3, 14), today(), |d| {
        if d >= start { 120 * M } else { 100 * M }
    });
    fill(&mut p, "k8s", date(9, 24), today(), |_| 9 * M);
    fill(&mut p, "lab", date(9, 20), today(), |_| 60 * M);
    fill(&mut p, "prod", date(8, 15), today(), |d| {
        if d == date(9, 23) {
            3_600 * M
        } else {
            2_500 * M
        }
    });
    fill(&mut p, "staging", date(8, 28), today(), |_| 200 * M);
    Scenario {
        partitions: Some(p),
        parquet: Parquet::Complete { age: 2 },
        headroom_measurement: complete(2),
        headroom: Some(Headroom {
            data: (1, 250 * G, 26 * G),
            wal: Some((1, 250 * G, 26 * G)),
            spill: (2, 68_719_476_736, 64_424_509_440),
            repin: false,
        }),
        wal: (7, 2048, complete(2)),
        evidence: Evidence {
            removals_age: 14,
            last_sweep: Some((SweepOutcome::Completed, 754)),
            ..Evidence::NONE
        },
        floor: GIB,
        env_max_age: &[("archive", 0), ("k8s", 14), ("lab", 7), ("staging", 30)],
    }
}

/// The headroom attempt failed an hour after its last complete sample,
/// which held a deficit. The complete Parquet scan holds an env whose only
/// partition is dated tomorrow, as an agent with a fast clock writes one.
fn failed_retained_scenario() -> Scenario {
    let mut p = EnvDateBytes::new();
    fill(&mut p, "archive", date(3, 14), today(), |_| 100 * M);
    fill(&mut p, "edge", date(9, 28), date(9, 28), |_| 3 * M);
    fill(&mut p, "k8s", date(9, 24), today(), |_| 9 * M);
    fill(&mut p, "prod", date(8, 15), today(), |_| 2_500 * M);
    Scenario {
        partitions: Some(p),
        parquet: Parquet::Complete { age: 2 },
        headroom_measurement: measurement(Status::Failed, Some(3600)),
        headroom: Some(Headroom {
            data: (1, 500 * G, 800 * M),
            wal: Some((2, 64 * G, 60 * G)),
            spill: (1, 500 * G, 800 * M),
            repin: false,
        }),
        wal: (7, 2048, complete(2)),
        evidence: Evidence {
            removals_age: 14,
            removals_disk_pressure: 3,
            pressure_attempts: 2,
            last_sweep: Some((SweepOutcome::Failed, 41)),
        },
        floor: GIB,
        env_max_age: &[("archive", 0), ("k8s", 14)],
    }
}

/// Both samples complete; a repin marker stood in the data root while the
/// headroom attempt's fence read it, and retention stood down for it.
fn repin_suppressed_scenario() -> Scenario {
    let mut p = EnvDateBytes::new();
    fill(&mut p, "k8s", date(9, 24), today(), |_| 9 * M);
    fill(&mut p, "prod", date(8, 15), today(), |_| 2_500 * M);
    fill(&mut p, "staging", date(8, 28), today(), |_| 200 * M);
    let one = (1, 250 * G, 90 * G);
    Scenario {
        partitions: Some(p),
        parquet: Parquet::Complete { age: 3 },
        headroom_measurement: complete(3),
        headroom: Some(Headroom {
            data: one,
            wal: Some(one),
            spill: one,
            repin: true,
        }),
        wal: (7, 2048, complete(3)),
        evidence: Evidence {
            removals_age: 14,
            last_sweep: Some((SweepOutcome::Suppressed, 300)),
            ..Evidence::NONE
        },
        floor: GIB,
        env_max_age: &[("k8s", 14), ("staging", 30)],
    }
}

/// Floor 0: pressure deletion off. At the largest observed day the disk
/// fills first; at the mean day both rated envs keep their policy.
fn floor_zero_scenario() -> Scenario {
    let mut p = EnvDateBytes::new();
    fill(&mut p, "fresh", date(9, 25), today(), |_| 4 * M);
    fill(&mut p, "lab", date(9, 20), today(), |d| {
        if d == date(9, 22) { 90 * M } else { 50 * M }
    });
    fill(&mut p, "prod", date(7, 1), today(), |d| {
        if d == date(9, 23) {
            1_600 * M
        } else {
            1_000 * M
        }
    });
    let one = (1, 200 * G, 20 * G);
    Scenario {
        partitions: Some(p),
        parquet: Parquet::Complete { age: 5 },
        headroom_measurement: complete(5),
        headroom: Some(Headroom {
            data: one,
            wal: Some(one),
            spill: one,
            repin: false,
        }),
        wal: (7, 2048, complete(5)),
        evidence: Evidence {
            removals_age: 3,
            last_sweep: Some((SweepOutcome::Completed, 120)),
            ..Evidence::NONE
        },
        floor: 0,
        env_max_age: &[("fresh", 30), ("lab", 7)],
    }
}

/// The sweep ran out of candidates below the floor, so every env holds
/// only today's partition and none has a rate.
fn pressure_scenario() -> Scenario {
    let mut p = EnvDateBytes::new();
    fill(&mut p, "archive", today(), today(), |_| 1_200 * M);
    fill(&mut p, "lab", today(), today(), |_| 300 * M);
    fill(&mut p, "prod", today(), today(), |_| 4_100 * M);
    let one = (1, 100 * G, 600 * M);
    Scenario {
        partitions: Some(p),
        parquet: Parquet::Complete { age: 1 },
        headroom_measurement: complete(1),
        headroom: Some(Headroom {
            data: one,
            wal: Some(one),
            spill: one,
            repin: false,
        }),
        wal: (7, 2048, complete(1)),
        evidence: Evidence {
            removals_age: 30,
            removals_disk_pressure: 12,
            pressure_attempts: 5,
            last_sweep: Some((SweepOutcome::ExhaustedBelowFloor, 61)),
        },
        floor: GIB,
        env_max_age: &[("archive", 0), ("lab", 7)],
    }
}

/// Before the first attempt of every sample, and before the first sweep.
fn awaiting_scenario() -> Scenario {
    let not_sampled = measurement(Status::NotSampled, None);
    Scenario {
        partitions: None,
        parquet: Parquet::NotSampled,
        headroom_measurement: not_sampled,
        headroom: None,
        wal: (0, 0, not_sampled),
        evidence: Evidence::NONE,
        floor: GIB,
        env_max_age: &[],
    }
}

/// A Parquet scan failed with nothing retained; headroom is complete.
fn scan_failed_scenario() -> Scenario {
    let one = (1, 250 * G, 180 * G);
    Scenario {
        partitions: None,
        parquet: Parquet::FailedFirst,
        headroom_measurement: complete(4),
        headroom: Some(Headroom {
            data: one,
            wal: Some(one),
            spill: one,
            repin: false,
        }),
        wal: (7, 2048, complete(4)),
        evidence: Evidence::NONE,
        floor: GIB,
        env_max_age: &[],
    }
}

/// The base snapshot every Health page case loads: one env projected at
/// the full policy, with 20 small partitions from 2026-07-01 on.
fn base_dashboard_scenario() -> Scenario {
    let mut p = EnvDateBytes::new();
    p.insert(("prod".to_owned(), date(7, 1)), 402);
    fill(&mut p, "prod", date(9, 9), today(), |_| 410);
    Scenario {
        partitions: Some(p),
        parquet: Parquet::Complete { age: 2 },
        headroom_measurement: complete(2),
        headroom: Some(Headroom {
            data: (1, 536_870_912_000, 128_849_018_880),
            wal: Some((2, 68_719_476_736, 64_424_509_440)),
            spill: (1, 536_870_912_000, 128_849_018_880),
            repin: false,
        }),
        wal: (7, 2048, complete(2)),
        evidence: Evidence {
            removals_age: 14,
            last_sweep: Some((SweepOutcome::Completed, 754)),
            ..Evidence::NONE
        },
        floor: GIB,
        env_max_age: &[],
    }
}

fn wire_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../trawl-web-ui/e2e/harness/wire")
}

fn regenerating() -> bool {
    std::env::var_os(REGEN).is_some_and(|value| value == "1")
}

fn pretty(value: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap() + "\n"
}

/// Unchanged lines kept around each change in [`unified_diff`].
const DIFF_CONTEXT: usize = 3;

/// A line diff of `expected` against `actual` over their longest common
/// subsequence: removed lines marked `-`, added lines `+`, and up to
/// [`DIFF_CONTEXT`] unchanged lines around each change, with `@@` between
/// runs of change that are further apart.
fn unified_diff(expected: &str, actual: &str) -> String {
    let (a, b): (Vec<_>, Vec<_>) = (expected.lines().collect(), actual.lines().collect());
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut lines) = (0, 0, Vec::new());
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            lines.push((' ', a[i]));
            (i, j) = (i + 1, j + 1);
        } else if i < a.len() && (j == b.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            lines.push(('-', a[i]));
            i += 1;
        } else {
            lines.push(('+', b[j]));
            j += 1;
        }
    }
    let changed: Vec<usize> = (0..lines.len()).filter(|&k| lines[k].0 != ' ').collect();
    let near = |k: usize| changed.iter().any(|&c| c.abs_diff(k) <= DIFF_CONTEXT);
    let mut out = String::new();
    let mut skipped = false;
    for (k, (mark, line)) in lines.iter().enumerate() {
        if near(k) {
            if skipped {
                out += "@@\n";
                skipped = false;
            }
            writeln!(out, "{mark} {line}").unwrap();
        } else {
            skipped = !out.is_empty();
        }
    }
    out
}

/// Compare the `produced` slice with the `committed` one as JSON values.
/// On a mismatch, `write` the produced fixture when regenerating, and
/// otherwise panic with a line diff of the two pretty forms. A fixture
/// that already matches is never rewritten, so regenerating does not
/// reformat an unchanged file.
fn check_or_regenerate(
    path: &Path,
    committed: &serde_json::Value,
    produced: &serde_json::Value,
    write: impl FnOnce(),
) {
    if committed == produced {
        return;
    }
    if regenerating() {
        write();
        return;
    }
    panic!(
        "{} is not what the producer emits (- committed, + produced); \
         if the change is deliberate, rerun with {REGEN}=1\n{}",
        path.display(),
        unified_diff(&pretty(committed), &pretty(produced))
    );
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn capacity_fixtures_are_producer_output() {
    let scenarios = [
        ("complete", complete_scenario()),
        ("failed-retained", failed_retained_scenario()),
        ("repin-suppressed", repin_suppressed_scenario()),
        ("floor-zero", floor_zero_scenario()),
        ("pressure", pressure_scenario()),
        ("awaiting", awaiting_scenario()),
        ("scan-failed", scan_failed_scenario()),
    ];
    for (name, scenario) in scenarios {
        let path = wire_dir().join(format!("health-capacity-{name}.json"));
        let produced = produce(&scenario);
        check_or_regenerate(
            &path,
            &read_json(&path),
            &serde_json::to_value(&produced).unwrap(),
            || std::fs::write(&path, pretty(&produced)).unwrap(),
        );
    }
}

/// The base snapshot's capacity slice is producer output too. Only the
/// slice's fields are compared and rewritten; the rest of the snapshot is
/// the harness's own.
#[test]
fn base_dashboard_capacity_is_producer_output() {
    let path = wire_dir().join("health-dashboard.json");
    let snapshot = read_json(&path);
    let produced = serde_json::to_value(produce(&base_dashboard_scenario())).unwrap();
    let produced = produced.as_object().unwrap();
    let committed = produced
        .keys()
        .map(|field| (field.clone(), snapshot[field].clone()))
        .collect::<serde_json::Map<_, _>>();
    check_or_regenerate(
        &path,
        &serde_json::Value::Object(committed),
        &serde_json::Value::Object(produced.clone()),
        || {
            // Through the typed snapshot, so the file keeps its field order.
            let mut value = snapshot.clone();
            for (field, produced) in produced {
                value[field] = produced.clone();
            }
            let typed: trawl_api::DashboardSnapshot = serde_json::from_value(value).unwrap();
            std::fs::write(&path, pretty(&typed)).unwrap();
        },
    );
}

/// The diff names the lines that differ and keeps the ones that match.
#[test]
fn the_fixture_diff_marks_changed_lines() {
    assert_eq!(
        unified_diff("a\nb\nc\n", "a\nx\nc\n"),
        "  a\n- b\n+ x\n  c\n"
    );
    // Unchanged runs longer than the context collapse to `@@`.
    let expected = (0..20).map(|n| n.to_string() + "\n").collect::<String>();
    let actual = (0..20)
        .map(|n| match n {
            2 => "two".to_owned(),
            17 => "seventeen".to_owned(),
            n => n.to_string(),
        } + "\n")
        .collect::<String>();
    assert_eq!(
        unified_diff(&expected, &actual),
        "  0\n  1\n- 2\n+ two\n  3\n  4\n  5\n@@\n  14\n  15\n  16\n- 17\n+ seventeen\n  18\n  19\n"
    );
}
