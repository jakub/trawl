// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawld --doctor` tests of the data root and recovery marker checks.
//!
//! Each test plants one state under a private data root, points the doctor
//! at a migrated Fleet database and a migrated app-state database, so the
//! database rows are not what fails, and reads the JSON report. A state
//! boot refuses is `failed` with a stable reason and the run exits 1; the
//! output never names the data root, a marker's content, or either catalog
//! identifier.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use trawl_api::doctor::{Check, Outcome, Report, reason};
use trawl_server::ingest::publication_marker::{OutputIdentity, ValidatedMarker};

use crate::common;
use crate::support::{DoctorConfig, assert_no_values, report, run_doctor, write_doctor_config};

/// A catalog id no database here holds, for a data root restored from
/// somewhere else.
const FOREIGN_CATALOG: &str = "5b1e0c55-2f0e-4c1a-9d3b-00000000f0e1";

/// A migrated Fleet database and a migrated app-state database.
struct Databases {
    fleet: String,
    app: String,
    /// `catalog_state.catalog_id` of the app-state database.
    catalog_id: String,
}

/// Mint and migrate both databases, and release everything that holds a
/// connection before the doctor runs.
async fn migrated_databases() -> Databases {
    let fleet = common::create_fleet_database().await;
    let pool = common::fleet_pool(&fleet).await;
    fleet_auth::MIGRATOR
        .run(&pool)
        .await
        .expect("migrate the Fleet database");
    pool.close().await;

    let app = common::create_app_database().await;
    let pool = common::app_pool(&app).await;
    let storage = trawl_server::store::StorageState::from_pool(pool.clone())
        .await
        .expect("migrate the app-state database");
    let catalog_id = storage
        .catalog
        .catalog_id()
        .await
        .expect("read the catalog id");
    drop(storage);
    pool.close().await;
    Databases {
        fleet,
        app,
        catalog_id,
    }
}

/// The doctor's environment: `HOME` and both real database URLs.
fn env(home: &Path, dbs: &Databases) -> Vec<(&'static str, OsString)> {
    vec![
        ("HOME", home.as_os_str().to_owned()),
        ("FLEET_DATABASE_URL", dbs.fleet.clone().into()),
        ("TRAWL_DATABASE_URL", dbs.app.clone().into()),
    ]
}

/// What one doctor run printed, parsed.
struct Run {
    code: i32,
    report: Report,
}

impl Run {
    fn row(&self, id: &str) -> &Check {
        self.report
            .checks()
            .iter()
            .find(|check| check.id == id)
            .unwrap_or_else(|| panic!("no {id} row"))
    }

    /// `id`'s outcome and reason.
    fn outcome(&self, id: &str) -> (Outcome, Option<&str>) {
        let row = self.row(id);
        (row.outcome, row.reason.as_deref())
    }
}

/// Write `config` into `dir`, run the doctor on it with JSON output, and
/// check both streams for every planted value: the database URLs, both
/// catalog ids, the data root's own path, and a configured WAL directory's.
async fn doctor(dir: &Path, config: &DoctorConfig, dbs: &Databases) -> Run {
    let path = write_doctor_config(dir, config);
    let args: Vec<OsString> = vec![
        "--doctor".into(),
        "--config".into(),
        path.into(),
        "--format".into(),
        "json".into(),
    ];
    let env = env(dir, dbs);
    let (code, stdout, stderr) = tokio::task::spawn_blocking(move || run_doctor(&args, &env))
        .await
        .expect("the doctor run");
    let data = config.data_path.to_string_lossy().into_owned();
    let wal = config
        .wal_dir
        .as_ref()
        .map(|wal| wal.to_string_lossy().into_owned());
    let mut planted = vec![
        dbs.fleet.as_str(),
        &dbs.app,
        &dbs.catalog_id,
        FOREIGN_CATALOG,
        &data,
    ];
    planted.extend(wal.as_deref());
    assert_no_values(&stdout, &stderr, &planted);
    Run {
        code,
        report: report(&stdout),
    }
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create parents");
    std::fs::write(path, bytes).expect("plant a file");
}

/// A repin marker document in `phase`.
fn repin_marker(phase: &str) -> String {
    format!(
        r#"{{"job_id":9,"field":"status","from_type":"BIGINT","to_type":"VARCHAR","phase":"{phase}"}}"#
    )
}

/// One state boot refuses, the check that must fail on it, and the reason.
struct Refusal {
    name: &'static str,
    ingest: bool,
    plant: fn(&Path),
    check: &'static str,
    reason: &'static str,
}

#[allow(clippy::too_many_lines)] // one table of states, clearer unsplit
fn refusals() -> Vec<Refusal> {
    vec![
        Refusal {
            name: "unsupported epoch",
            ingest: true,
            plant: |data| {
                write(&data.join("EPOCH"), b"2\n");
                write(&data.join("prod/2026-01-01/10/svc.parquet"), b"corpus");
            },
            check: "server.data.epoch",
            reason: "the data root carries an unsupported storage epoch",
        },
        Refusal {
            name: "unsupported epoch on a query-only node",
            ingest: false,
            plant: |data| write(&data.join("EPOCH"), b"4\n"),
            check: "server.data.epoch",
            reason: "the data root carries an unsupported storage epoch",
        },
        Refusal {
            name: "owned root without an epoch",
            ingest: true,
            plant: |data| write(&data.join("prod/2026-01-01/10/svc.parquet"), b"corpus"),
            check: "server.data.epoch",
            reason: "the data root is nonempty but has no EPOCH marker",
        },
        Refusal {
            name: "owned root without an epoch on a query-only node",
            ingest: false,
            plant: |data| write(&data.join("CATALOG"), b"anything\n"),
            check: "server.data.epoch",
            reason: "the data root is nonempty but has no EPOCH marker",
        },
        Refusal {
            name: "flat WAL batches with ingest enabled",
            ingest: true,
            plant: |data| {
                write(&data.join("EPOCH"), b"3\n");
                write(&data.join("wal/svc_1700000000000_ab12.ndjson"), b"{}\n");
            },
            check: "server.data.epoch",
            reason: "the WAL directory holds flat batches outside environment directories",
        },
        Refusal {
            name: "catalog identity mismatch on a query-only node",
            ingest: false,
            plant: |data| {
                write(&data.join("EPOCH"), b"3\n");
                write(
                    &data.join("CATALOG"),
                    format!("{FOREIGN_CATALOG}\n").as_bytes(),
                );
                write(&data.join("prod/2026-01-01/10/svc.parquet"), b"corpus");
            },
            check: "server.data.identity",
            reason: reason::CATALOG_IDENTITY_MISMATCH,
        },
        Refusal {
            name: "catalog identity mismatch on an ingest node",
            ingest: true,
            plant: |data| {
                write(&data.join("EPOCH"), b"3\n");
                write(
                    &data.join("CATALOG"),
                    format!("{FOREIGN_CATALOG}\n").as_bytes(),
                );
                write(&data.join("prod/2026-01-01/10/svc.parquet"), b"corpus");
            },
            check: "server.data.identity",
            reason: reason::CATALOG_IDENTITY_MISMATCH,
        },
        Refusal {
            name: "repin cutover on a query-only node",
            ingest: false,
            plant: |data| {
                write(&data.join("EPOCH"), b"3\n");
                write(&data.join("REPIN"), repin_marker("cutover").as_bytes());
            },
            check: "server.recovery.repin",
            reason: "a repin job died mid-cutover and this node runs with ingest disabled",
        },
        Refusal {
            name: "repin cleanup on a query-only node",
            ingest: false,
            plant: |data| {
                write(&data.join("EPOCH"), b"3\n");
                write(&data.join("REPIN"), repin_marker("cleanup").as_bytes());
            },
            check: "server.recovery.repin",
            reason: "a repin job died mid-cutover and this node runs with ingest disabled",
        },
        Refusal {
            name: "malformed repin marker",
            ingest: true,
            plant: |data| {
                write(&data.join("EPOCH"), b"3\n");
                write(&data.join("REPIN"), br#"{"job_id":"#);
            },
            check: "server.recovery.repin",
            reason: "the repin marker is malformed",
        },
    ]
}

/// The storage rows of `doctor_boot_refusals_fail`: every data-root state
/// boot refuses is `failed` with its stable reason and the run exits 1.
///
/// Catalog identity is stricter than boot (#269 H1): a marker naming
/// another catalog fails on a query-only node, which boot refuses, and on
/// an ingest node, which boot would adopt through the lossy conformance
/// pass. Neither catalog id appears in the output, which
/// [`doctor`] checks on every run.
///
/// Every case runs before the test judges, so one failure names every
/// state that disagrees.
#[tokio::test]
async fn doctor_boot_refusals_fail() {
    let dbs = migrated_databases().await;
    let mut wrong = Vec::new();
    for case in refusals() {
        let dir = tempfile::tempdir().unwrap();
        let config = DoctorConfig {
            ingest: case.ingest,
            ..DoctorConfig::in_dir(dir.path())
        };
        (case.plant)(&config.data_path);
        let run = doctor(dir.path(), &config, &dbs).await;
        let seen = run.outcome(case.check);
        if seen != (Outcome::Failed, Some(case.reason)) || run.code != 1 {
            wrong.push(format!(
                "{}: {} is {seen:?}, exit {}",
                case.name, case.check, run.code
            ));
        }
        if case.reason == reason::CATALOG_IDENTITY_MISMATCH {
            let next = run.row(case.check).next_action.clone().unwrap_or_default();
            for way_out in [
                "point the app-state database at the catalog that owns this archive",
                "restore the database dump and the data archive from the same backup",
                "point [data] path at a new data root",
                "start trawld with [ingest] enabled = true to adopt the archive on purpose",
            ] {
                if !next.contains(way_out) {
                    wrong.push(format!("{}: next action lacks {way_out:?}", case.name));
                }
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "states boot refuses that the doctor did not fail:\n  {}",
        wrong.join("\n  ")
    );
}

/// An ingest node's data root holding `EPOCH` 3 and its WAL inside it.
fn current_ingest_root(dir: &Path) -> DoctorConfig {
    let config = DoctorConfig::in_dir(dir);
    write(&config.data_path.join("EPOCH"), b"3\n");
    std::fs::create_dir_all(config.data_path.join("wal/prod")).unwrap();
    config
}

/// A well-formed publication marker for `prod`/`nginx`, from the encoder
/// compaction writes with.
fn publication_marker(wal: &Path) -> (PathBuf, String) {
    let marker = ValidatedMarker::new(
        "prod",
        "nginx",
        chrono::NaiveDate::from_ymd_opt(2026, 9, 23).unwrap(),
        7,
        vec!["nginx_1700000000000_ab12.ndjson".to_owned()],
        OutputIdentity {
            size: 26,
            hash: blake3::hash(b"PAR1 new output bytes PAR1"),
        },
    )
    .expect("a valid marker");
    (marker.marker_path(wal), marker.encode())
}

/// A rollup marker as compaction writes it: the hourly inputs, one path a
/// line.
fn rollup_marker(data: &Path) -> (PathBuf, String) {
    let day = data.join("prod/2026-09-23");
    let body = format!(
        "{}\n{}",
        day.join("07/nginx.parquet").display(),
        day.join("08/nginx.parquet").display()
    );
    (day.join(".rollup-nginx"), body)
}

/// Whether the running user reads a file of mode 000 anyway: root, or a
/// holder of `CAP_DAC_OVERRIDE`.
#[cfg(unix)]
fn reads_mode_000(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    let probe = dir.join("probe");
    std::fs::write(&probe, b"x").unwrap();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o000)).unwrap();
    let privileged = std::fs::File::open(&probe).is_ok();
    std::fs::remove_file(&probe).unwrap();
    privileged
}

/// `doctor_recovery_markers`: a malformed publication or rollup marker is
/// `failed`, an unreadable one is `not_sampled`/`unreadable`, and
/// well-formed pending ones are `complete`/`pending_at_next_boot`.
#[tokio::test]
async fn doctor_recovery_markers() {
    const CHECK: &str = "server.recovery.publication";
    let dbs = migrated_databases().await;

    // Well formed and pending: boot's recovery finishes both.
    {
        let dir = tempfile::tempdir().unwrap();
        let config = current_ingest_root(dir.path());
        let (path, body) = publication_marker(&config.data_path.join("wal"));
        write(&path, body.as_bytes());
        let (path, body) = rollup_marker(&config.data_path);
        write(&path, body.as_bytes());
        let run = doctor(dir.path(), &config, &dbs).await;
        assert_eq!(
            run.outcome(CHECK),
            (Outcome::Complete, Some(reason::PENDING_AT_NEXT_BOOT))
        );
        assert_ne!(run.code, 1, "{:?}", run.report.checks());
        let detail = run.row(CHECK).detail.clone().unwrap();
        assert!(
            detail.contains("1 pending and 0 malformed publication marker(s)")
                && detail.contains("1 pending and 0 malformed rollup marker(s)"),
            "{detail}"
        );
    }

    // Malformed: a publication marker that does not parse, a rollup marker
    // that is not text, and one that is not a regular file.
    for (name, plant, why) in [
        (
            "unparseable publication marker",
            (|data: &Path| write(&data.join("wal/prod/.publish-nginx.json"), b"{ not json"))
                as fn(&Path),
            "a publication marker is malformed",
        ),
        (
            "publication marker naming WAL of another service",
            |data: &Path| {
                let (_, body) = publication_marker(&data.join("wal"));
                write(&data.join("wal/prod/.publish-api.json"), body.as_bytes());
            },
            "a publication marker is malformed",
        ),
        (
            "rollup marker that is not text",
            |data: &Path| write(&data.join("prod/2026-09-23/.rollup-nginx"), &[0xff, 0xfe]),
            "a rollup marker is malformed",
        ),
        (
            "rollup marker that is a directory",
            |data: &Path| {
                std::fs::create_dir_all(data.join("prod/2026-09-23/.rollup-nginx")).unwrap();
            },
            "a rollup marker is malformed",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = current_ingest_root(dir.path());
        plant(&config.data_path);
        let run = doctor(dir.path(), &config, &dbs).await;
        assert_eq!(run.outcome(CHECK), (Outcome::Failed, Some(why)), "{name}");
        assert_eq!(run.code, 1, "{name}");
    }

    // Unreadable: a well-formed marker of mode 000 is not sampled, kept
    // apart from malformed. A user who reads it anyway sees it pending.
    #[cfg(unix)]
    for rollup in [false, true] {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let config = current_ingest_root(dir.path());
        let (path, body) = if rollup {
            rollup_marker(&config.data_path)
        } else {
            publication_marker(&config.data_path.join("wal"))
        };
        write(&path, body.as_bytes());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let privileged = reads_mode_000(dir.path());
        let run = doctor(dir.path(), &config, &dbs).await;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let expected = if privileged {
            (Outcome::Complete, Some(reason::PENDING_AT_NEXT_BOOT))
        } else {
            (Outcome::NotSampled, Some(reason::UNREADABLE))
        };
        assert_eq!(run.outcome(CHECK), expected, "rollup {rollup}");
        assert_ne!(run.code, 1, "rollup {rollup}");
        assert_ne!(run.code, 0, "rollup {rollup}");
    }
}

/// A query-only node's data root holding `EPOCH` 3, with a WAL directory
/// inside it that the node never reads.
fn current_query_only_root(dir: &Path) -> DoctorConfig {
    DoctorConfig {
        ingest: false,
        ..current_ingest_root(dir)
    }
}

/// `server.recovery.publication` on a query-only node: its publication
/// gate registers every rollup marker the boot scan finds and refuses
/// corpus reads while one remains, so the rollup census runs there too. A
/// malformed rollup marker is `failed`, an unreadable one is
/// `not_sampled`/`unreadable`, and a pending one is
/// `complete`/`pending_at_next_boot`. Publication markers stay an ingest
/// node's: a malformed one in the WAL a query-only node never reads changes
/// nothing.
#[tokio::test]
async fn doctor_query_only_rollup_markers() {
    const CHECK: &str = "server.recovery.publication";
    let dbs = migrated_databases().await;
    let malformed_publication =
        |data: &Path| write(&data.join("wal/prod/.publish-nginx.json"), b"{ not json");

    // Pending, beside a malformed publication marker it does not count.
    {
        let dir = tempfile::tempdir().unwrap();
        let config = current_query_only_root(dir.path());
        let (path, body) = rollup_marker(&config.data_path);
        write(&path, body.as_bytes());
        malformed_publication(&config.data_path);
        let run = doctor(dir.path(), &config, &dbs).await;
        assert_eq!(
            run.outcome(CHECK),
            (Outcome::Complete, Some(reason::PENDING_AT_NEXT_BOOT))
        );
        assert_ne!(run.code, 1, "{:?}", run.report.checks());
        let detail = run.row(CHECK).detail.clone().unwrap();
        assert!(
            detail.contains("1 pending and 0 malformed rollup marker(s)")
                && !detail.contains("publication marker(s)"),
            "{detail}"
        );
    }

    // No rollup marker: complete, whatever the WAL holds.
    {
        let dir = tempfile::tempdir().unwrap();
        let config = current_query_only_root(dir.path());
        malformed_publication(&config.data_path);
        let run = doctor(dir.path(), &config, &dbs).await;
        assert_eq!(run.outcome(CHECK), (Outcome::Complete, None));
    }

    // Malformed: not text, and not a regular file.
    for (name, plant) in [
        (
            "rollup marker that is not text",
            (|data: &Path| write(&data.join("prod/2026-09-23/.rollup-nginx"), &[0xff, 0xfe]))
                as fn(&Path),
        ),
        ("rollup marker that is a directory", |data: &Path| {
            std::fs::create_dir_all(data.join("prod/2026-09-23/.rollup-nginx")).unwrap();
        }),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = current_query_only_root(dir.path());
        plant(&config.data_path);
        let run = doctor(dir.path(), &config, &dbs).await;
        assert_eq!(
            run.outcome(CHECK),
            (Outcome::Failed, Some("a rollup marker is malformed")),
            "{name}"
        );
        assert_eq!(run.code, 1, "{name}");
    }

    // Unreadable: a well-formed marker of mode 000. A user who reads it
    // anyway sees it pending.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let config = current_query_only_root(dir.path());
        let (path, body) = rollup_marker(&config.data_path);
        write(&path, body.as_bytes());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let privileged = reads_mode_000(dir.path());
        let run = doctor(dir.path(), &config, &dbs).await;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let expected = if privileged {
            (Outcome::Complete, Some(reason::PENDING_AT_NEXT_BOOT))
        } else {
            (Outcome::NotSampled, Some(reason::UNREADABLE))
        };
        assert_eq!(run.outcome(CHECK), expected);
        assert_ne!(run.code, 1);
        assert_ne!(run.code, 0);
    }
}

/// `server.data.identity` over an archive the walk could not enumerate in
/// full. A foreign `CATALOG` marker over a root whose readable part holds
/// no parquet, beside an empty directory of mode 000, is
/// `not_sampled`/`unreadable` on both node types: the doctor has seen no
/// parquet the marker must account for. Boot reads the same walk as
/// standing data (`conform::archive_is_empty`), which this leaves alone.
/// Parquet the walk does see still fails the mismatch. A user who reads the
/// sealed directory anyway sees an empty archive.
#[cfg(unix)]
#[tokio::test]
async fn doctor_identity_over_an_unwalked_archive() {
    use std::os::unix::fs::PermissionsExt as _;
    const CHECK: &str = "server.data.identity";
    let dbs = migrated_databases().await;
    for ingest in [false, true] {
        for parquet in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let config = DoctorConfig {
                ingest,
                ..current_ingest_root(dir.path())
            };
            let data = &config.data_path;
            write(
                &data.join("CATALOG"),
                format!("{FOREIGN_CATALOG}\n").as_bytes(),
            );
            if parquet {
                write(&data.join("prod/2026-01-02/10/svc.parquet"), b"corpus");
            }
            let sealed = data.join("prod/2026-01-01");
            std::fs::create_dir_all(&sealed).unwrap();
            std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o000)).unwrap();
            let privileged = reads_mode_000(dir.path());
            let run = doctor(dir.path(), &config, &dbs).await;
            std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o700)).unwrap();
            let label = format!("ingest {ingest}, parquet {parquet}");
            let expected = match (parquet, privileged) {
                (true, _) => (Outcome::Failed, Some(reason::CATALOG_IDENTITY_MISMATCH)),
                (false, true) => (Outcome::Complete, None),
                (false, false) => (Outcome::NotSampled, Some(reason::UNREADABLE)),
            };
            assert_eq!(run.outcome(CHECK), expected, "{label}");
            if parquet {
                assert_eq!(run.code, 1, "{label}");
            } else if !privileged {
                assert_ne!(run.code, 1, "{label}: {:?}", run.report.checks());
                assert_ne!(run.code, 0, "{label}");
            }
        }
    }
}

/// `doctor_wal_outside_the_data_root`: an ingest node's WAL directory
/// outside its data root is one boot creates or writes at its start, so
/// `server.data.root` fails when the running user cannot write it, or
/// cannot create it in the directory above it, and never names it. A user
/// who may write a directory its mode forbids sees each state complete.
#[cfg(unix)]
#[tokio::test]
async fn doctor_wal_outside_the_data_root() {
    use std::os::unix::fs::PermissionsExt as _;
    const CHECK: &str = "server.data.root";
    let dbs = migrated_databases().await;
    for (name, wal, sealed, denied) in [
        ("writable", "spool/wal", None, None),
        (
            "unwritable",
            "spool/wal",
            Some("spool/wal"),
            Some("the running user cannot write the WAL directory"),
        ),
        (
            "absent under an unwritable parent",
            "spool/wal",
            Some("spool"),
            Some("the running user cannot create the WAL directory in the directory above it"),
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = DoctorConfig {
            wal_dir: Some(dir.path().join(wal)),
            ..current_ingest_root(dir.path())
        };
        std::fs::create_dir_all(dir.path().join(sealed.unwrap_or(wal))).unwrap();
        let sealed = sealed.map(|sealed| dir.path().join(sealed));
        if let Some(sealed) = &sealed {
            std::fs::set_permissions(sealed, std::fs::Permissions::from_mode(0o500)).unwrap();
        }
        let privileged = reads_mode_000(dir.path());
        let run = doctor(dir.path(), &config, &dbs).await;
        if let Some(sealed) = &sealed {
            std::fs::set_permissions(sealed, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        match denied {
            Some(why) if !privileged => {
                assert_eq!(run.outcome(CHECK), (Outcome::Failed, Some(why)), "{name}");
                assert_eq!(run.code, 1, "{name}");
            }
            _ => {
                assert_eq!(run.outcome(CHECK), (Outcome::Complete, None), "{name}");
                assert_ne!(run.code, 1, "{name}: {:?}", run.report.checks());
            }
        }
    }
}
