// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The results root (ADR-0057) over real temporary directories: how a
//! capture's file is staged and published, what reads refuse, and what the
//! hold refuses at open. No database.

#![cfg(unix)]

use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use trawl_server::config::Config;
use trawl_server::results::root::{ResultsRoot, Verified, is_stage_name};
use trawl_server::state::{open_query_scratch, open_results_root};

const ID: &str = "0123456789abcdef0123456789abcdef";
const OTHER: &str = "fedcba9876543210fedcba9876543210";

/// The permission bits at `path`, not following a final symlink.
fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

/// Every name in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// A results root held in a fresh temporary directory, kept apart from
/// nothing.
fn root_in(tmp: &tempfile::TempDir) -> ResultsRoot {
    ResultsRoot::open_apart_from(&tmp.path().join("data.results"), &[]).unwrap()
}

/// Stage `bytes` and publish them as `id`.
fn publish(root: &ResultsRoot, id: &str, bytes: &[u8]) -> trawl_server::results::root::Published {
    let staged = root.stage().unwrap();
    std::fs::write(staged.path(), bytes).unwrap();
    root.publish(staged, id).unwrap()
}

/// A configuration over `data`, with `[server]` settings `server`, parsed
/// without validation so the hold's own checks are what refuses.
fn config(data: &Path, server: &str) -> Config {
    Config::parse_toml(&format!(
        "[data]\npath = {:?}\n[server]\n{server}\n",
        data.to_str().unwrap()
    ))
    .unwrap()
}

#[test]
fn a_published_file_is_0600_under_a_0700_root_and_its_stage_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let root = root_in(&tmp);
    assert_eq!(mode_of(root.path()), 0o700);

    let staged = root.stage().unwrap();
    let stage_name = staged
        .path()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(is_stage_name(&stage_name), "{stage_name}");
    assert_eq!(mode_of(staged.path()), 0o600);
    let bytes = b"PAR1 not really parquet PAR1";
    std::fs::write(staged.path(), bytes).unwrap();
    let published = root.publish(staged, ID).unwrap();

    assert_eq!(published.file_bytes, bytes.len() as u64);
    assert_eq!(published.digest, *blake3::hash(bytes).as_bytes());
    let file = root.path().join(format!("{ID}.parquet"));
    assert_eq!(names(root.path()), [format!("{ID}.parquet")]);
    assert_eq!(mode_of(&file), 0o600);
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
    assert_eq!(root.path_of(ID).unwrap(), file);
    assert_eq!(root.list_ids().unwrap(), [ID]);
    assert_eq!(root.verify(ID, &published.digest).unwrap(), Verified::Ok);
    assert!(root.open_file(ID).unwrap().is_some());
    assert!(root.modified(ID).unwrap().is_some());
}

#[test]
fn a_dropped_stage_is_unlinked() {
    let tmp = tempfile::tempdir().unwrap();
    let root = root_in(&tmp);
    let staged = root.stage().unwrap();
    std::fs::write(staged.path(), b"half a file").unwrap();
    let path = staged.path().to_owned();
    assert!(path.exists());
    drop(staged);
    assert!(!path.exists());
    assert!(names(root.path()).is_empty());
}

#[test]
fn publish_never_replaces_an_existing_result() {
    let tmp = tempfile::tempdir().unwrap();
    let root = root_in(&tmp);
    let first = publish(&root, ID, b"the first capture");

    let staged = root.stage().unwrap();
    std::fs::write(staged.path(), b"a second capture").unwrap();
    let error = root.publish(staged, ID).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists, "{error}");

    assert_eq!(
        std::fs::read(root.path().join(format!("{ID}.parquet"))).unwrap(),
        b"the first capture"
    );
    assert_eq!(root.verify(ID, &first.digest).unwrap(), Verified::Ok);
    assert_eq!(
        names(root.path()),
        [format!("{ID}.parquet")],
        "the refused stage is unlinked"
    );
}

#[test]
fn a_symlink_at_a_result_name_is_never_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = root_in(&tmp);
    // The link's target holds exactly the bytes the digest expects, so a
    // read that followed it would verify.
    let bytes = b"bytes outside the root";
    let target = tmp.path().join("outside.parquet");
    std::fs::write(&target, bytes).unwrap();
    let link = root.path().join(format!("{ID}.parquet"));
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let digest = *blake3::hash(bytes).as_bytes();

    let error = root.open_file(ID).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc_eloop()), "{error}");
    let verified = root.verify(ID, &digest);
    assert!(verified.is_err(), "verify followed the link: {verified:?}");
    assert!(
        root.list_ids().unwrap().is_empty(),
        "a link is not a result"
    );
    assert!(root.unlink(ID).unwrap(), "unlink removes the link itself");
    assert_eq!(std::fs::read(&target).unwrap(), bytes);
}

/// `ELOOP`, which an `O_NOFOLLOW` open of a symlink fails with on Linux and
/// macOS.
fn libc_eloop() -> i32 {
    rustix::io::Errno::LOOP.raw_os_error()
}

#[test]
fn verify_detects_a_flipped_byte_and_a_missing_file() {
    let tmp = tempfile::tempdir().unwrap();
    let root = root_in(&tmp);
    let published = publish(&root, ID, b"0123456789 captured rows");
    let path = root.path_of(ID).unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[3] ^= 0x01;
    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .unwrap()
        .write_all(&bytes)
        .unwrap();
    assert_eq!(
        root.verify(ID, &published.digest).unwrap(),
        Verified::Mismatch
    );
    assert_eq!(
        root.verify(OTHER, &published.digest).unwrap(),
        Verified::Missing
    );
    assert!(root.open_file(OTHER).unwrap().is_none());
    assert!(root.modified(OTHER).unwrap().is_none());
}

#[test]
fn unlink_and_stage_removal_report_what_they_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = root_in(&tmp);
    publish(&root, ID, b"kept");
    // Stages a killed process left: committed never ran.
    let left = [root.stage().unwrap(), root.stage().unwrap()];
    for staged in &left {
        std::fs::write(staged.path(), b"orphaned").unwrap();
    }
    let paths: Vec<PathBuf> = left.iter().map(|s| s.path().to_owned()).collect();
    std::mem::forget(left);
    assert!(paths.iter().all(|p| p.exists()));

    assert_eq!(root.remove_stages().unwrap(), 2);
    assert!(paths.iter().all(|p| !p.exists()));
    assert_eq!(root.remove_stages().unwrap(), 0);
    assert!(root.unlink(ID).unwrap());
    assert!(!root.unlink(ID).unwrap());
    assert!(names(root.path()).is_empty());
}

#[test]
fn ids_are_checked_before_any_name_is_built() {
    let tmp = tempfile::tempdir().unwrap();
    let root = root_in(&tmp);
    let outside = tmp.path().join("victim.parquet");
    std::fs::write(&outside, b"not a result").unwrap();
    for bad in [
        "../victim",
        "0123456789ABCDEF0123456789ABCDEF",
        "",
        "0123456789abcdef0123456789abcde",
    ] {
        let invalid = |e: std::io::Error| e.kind() == std::io::ErrorKind::InvalidInput;
        assert!(root.open_file(bad).is_err_and(invalid), "{bad:?}");
        assert!(root.unlink(bad).is_err_and(invalid), "{bad:?}");
        assert!(root.path_of(bad).is_err_and(invalid), "{bad:?}");
        assert!(root.verify(bad, &[0; 32]).is_err_and(invalid), "{bad:?}");
        let staged = root.stage().unwrap();
        assert!(root.publish(staged, bad).is_err_and(invalid), "{bad:?}");
    }
    assert!(outside.exists());
    assert!(names(root.path()).is_empty(), "every refused stage went");
}

#[test]
fn list_ids_ignores_what_trawld_does_not_make() {
    let tmp = tempfile::tempdir().unwrap();
    let root = root_in(&tmp);
    publish(&root, OTHER, b"b");
    publish(&root, ID, b"a");
    let staged = root.stage().unwrap();
    for foreign in [
        "notes.txt",
        "0123456789ABCDEF0123456789ABCDEF.parquet",
        "0123456789abcdef0123456789abcdef.parquet.tmp",
        "tmp_0123456789abcdef0123456789abcdef.parquet",
    ] {
        std::fs::write(root.path().join(foreign), b"x").unwrap();
    }
    std::fs::create_dir(root.path().join("11111111111111111111111111111111.parquet")).unwrap();
    std::fs::create_dir(root.path().join(".stage-0123456789abcdef.parquet")).unwrap();

    assert_eq!(root.list_ids().unwrap(), [ID, OTHER]);
    // Only the real stage is removed; the directory with a stage's name is
    // not one trawld made.
    drop(staged);
    assert_eq!(root.remove_stages().unwrap(), 0);
    assert!(root.path().join(".stage-0123456789abcdef.parquet").is_dir());
    assert!(root.path().join("notes.txt").exists());
}

#[test]
fn a_second_hold_of_the_same_root_refuses() {
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&tmp.path().join("data"), "");
    let first = open_results_root(&config).unwrap();
    assert_eq!(first.path(), tmp.path().join("data.results"));
    let error = open_results_root(&config).unwrap_err().to_string();
    assert!(error.contains("locked by another trawld"), "{error}");
    assert!(error.contains("[server] results_dir"), "{error}");
    drop(first);
    open_results_root(&config).expect("the lock is free once the hold drops");
}

#[test]
fn the_default_root_is_made_0700_and_a_loose_one_is_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&tmp.path().join("data"), "");
    let results = tmp.path().join("data.results");
    drop(open_results_root(&config).unwrap());
    assert_eq!(mode_of(&results), 0o700);

    std::fs::set_permissions(&results, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(results.join(format!("{ID}.parquet")), b"kept across starts").unwrap();
    let root = open_results_root(&config).unwrap();
    assert_eq!(mode_of(&results), 0o700);
    assert_eq!(root.list_ids().unwrap(), [ID], "the hold clears nothing");
}

#[test]
fn a_results_root_on_the_scratch_directory_refuses() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let scratch = tmp.path().join("scratch");
    // The same directory by its own spelling, and through a symlinked
    // parent that no spelling comparison can see through.
    let alias = tmp.path().join("alias");
    std::os::unix::fs::symlink(tmp.path(), &alias).unwrap();
    for results in [scratch.clone(), alias.join("scratch")] {
        let config = config(
            &data,
            &format!(
                "query_scratch_dir = {:?}\nresults_dir = {:?}",
                scratch.to_str().unwrap(),
                results.to_str().unwrap()
            ),
        );
        let _scratch = open_query_scratch(&config).unwrap();
        let error = open_results_root(&config).unwrap_err().to_string();
        assert!(
            error.contains("is the query scratch directory ([server] query_scratch_dir)"),
            "{error}"
        );
        assert!(
            error.contains("point [server] results_dir at a directory of its own"),
            "{error}"
        );
    }
}

#[test]
fn a_results_root_inside_the_data_root_refuses() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let alias = tmp.path().join("alias");
    std::os::unix::fs::symlink(tmp.path(), &alias).unwrap();
    for results in [data.join("results"), alias.join("data/results")] {
        let config = config(
            &data,
            &format!("results_dir = {:?}", results.to_str().unwrap()),
        );
        let error = open_results_root(&config).unwrap_err().to_string();
        assert!(
            error.contains("is inside the data root ([data] path)"),
            "{error}"
        );
        assert!(error.contains("[server] results_dir"), "{error}");
        assert!(!data.join("results").exists(), "nothing was made inside");
    }
}

#[cfg(target_os = "linux")]
#[allow(
    dead_code,
    reason = "this binary uses a subset of the namespace helper"
)]
#[path = "common/userns.rs"]
mod userns;

/// Held roots and storage roots that alias through bind mounts (#314,
/// ADR-0057 amendment of 2026-10-25), proved inside an unprivileged user
/// and mount namespace.
///
/// The hold is a library call that must run where the mounts are, so each
/// test re-executes this test binary inside the namespace, filtered to
/// itself, with the case in the environment. The inner run makes the public
/// call and checks the refusal word for word; it prints a sentinel only
/// after its checks pass, which guards against a filter that matched
/// nothing and exited 0. The outer run makes the tree, lists it, runs the
/// inner one, and lists it again: a refusal leaves the tree as it was. The
/// mounts are private to the namespace, so the outer listing reads the
/// directories themselves, never the aliases.
#[cfg(target_os = "linux")]
mod bind_alias {
    use std::os::unix::fs::MetadataExt as _;
    use std::path::{Path, PathBuf};

    use trawl_server::config::Config;
    use trawl_server::state::{open_query_scratch, open_results_root};

    use crate::userns::Userns;

    /// Names the case in the inner run. Unset, the test is the outer run.
    const CASE: &str = "TRAWL_TEST_BIND_ALIAS_CASE";
    /// The fixture's base directory, which the outer run made and owns.
    const BASE: &str = "TRAWL_TEST_BIND_ALIAS_BASE";
    /// What the inner run prints once its checks pass, before the case name.
    const SENTINEL: &str = "INNER_RAN";

    /// The refusal a case expects: the clause and roots, all paths relative
    /// to the base directory.
    struct Refusal {
        /// `is inside` or `is above`.
        overlap: &'static str,
        /// The storage root as the refusal names it.
        what: &'static str,
        /// The storage root's configured path.
        root: &'static str,
        /// The mount point the clause names.
        mount: &'static str,
    }

    /// The call the inner run makes.
    enum Call {
        /// `open_results_root` refuses.
        Results(Refusal),
        /// `open_query_scratch` refuses.
        Scratch(Refusal),
        /// `open_query_scratch`, then `open_results_root`, both open.
        Both,
    }

    /// One fixture. Every path is relative to the base directory.
    struct Case {
        name: &'static str,
        /// Directories made before the run, mount points included.
        dirs: &'static [&'static str],
        /// Files written before the run, so a listing has content to keep.
        files: &'static [&'static str],
        /// `(source, mount point)` pairs bound read-write in the namespace.
        binds: &'static [(&'static str, &'static str)],
        /// `[data] path`.
        data: &'static str,
        /// `[ingest] wal_dir`, or the default under the data root.
        wal: Option<&'static str>,
        /// `[server] query_scratch_dir`, always explicit.
        scratch: &'static str,
        /// `[server] results_dir`.
        results: &'static str,
        call: Call,
        /// The directory whose listing must read the same afterward.
        unchanged: &'static str,
    }

    const DATA_ROOT: &str = "the data root ([data] path)";
    const WAL_DIR: &str = "the WAL directory ([ingest] wal_dir)";
    const SCRATCH_DIR: &str = "the query scratch directory ([server] query_scratch_dir)";
    const REPIN_SHADOW_ROOT: &str = "the repin shadow root (<[data] path>.repin-next)";
    const REPIN_ASIDE_ROOT: &str = "the repin aside root (<[data] path>.repin-aside)";

    /// The configuration `case` describes, over `base`.
    fn config_of(case: &Case, base: &Path) -> Config {
        let path = |rel: &str| base.join(rel).to_str().unwrap().to_owned();
        let ingest = case.wal.map_or_else(String::new, |wal| {
            format!("[ingest]\nwal_dir = {:?}\n", path(wal))
        });
        let toml = format!(
            "[data]\npath = {:?}\n{ingest}[server]\nquery_scratch_dir = {:?}\nresults_dir = {:?}\n",
            path(case.data),
            path(case.scratch),
            path(case.results)
        );
        Config::parse_toml(&toml).unwrap()
    }

    /// The pinned overlap refusal with its mount clause, for the held
    /// directory `held` named `noun` and moved by `setting`.
    fn refusal_text(
        refusal: &Refusal,
        base: &Path,
        noun: &str,
        held: &str,
        setting: &str,
    ) -> String {
        format!(
            "the {noun} {} {} {} {} through the mount at {}, as the filesystem resolves \
             them; point {setting} at a directory of its own",
            base.join(held).display(),
            refusal.overlap,
            refusal.what,
            base.join(refusal.root).display(),
            base.join(refusal.mount).display(),
        )
    }

    /// Inside the namespace: make the case's call and check what it says.
    fn inner(case: &Case, base: &Path) {
        let config = config_of(case, base);
        match &case.call {
            Call::Results(refusal) => {
                let Err(error) = open_results_root(&config) else {
                    panic!("{}: the results root opened", case.name);
                };
                assert_eq!(
                    error.to_string(),
                    refusal_text(
                        refusal,
                        base,
                        "results root",
                        case.results,
                        "[server] results_dir"
                    ),
                    "{}",
                    case.name
                );
            }
            Call::Scratch(refusal) => {
                let Err(error) = open_query_scratch(&config) else {
                    panic!("{}: the query scratch directory opened", case.name);
                };
                assert_eq!(
                    error.to_string(),
                    refusal_text(
                        refusal,
                        base,
                        "query scratch directory",
                        case.scratch,
                        "[server] query_scratch_dir"
                    ),
                    "{}",
                    case.name
                );
            }
            Call::Both => {
                let scratch = open_query_scratch(&config)
                    .unwrap_or_else(|e| panic!("{}: query scratch: {e}", case.name));
                let results = open_results_root(&config)
                    .unwrap_or_else(|e| panic!("{}: results root: {e}", case.name));
                assert_eq!(results.path(), base.join(case.results), "{}", case.name);
                drop((scratch, results));
            }
        }
    }

    /// Every entry at and below `dir`, as `(path below dir, kind, mode)`,
    /// sorted, not following symlinks.
    fn tree(dir: &Path) -> Vec<(PathBuf, char, u32)> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, char, u32)>) {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            entries.sort();
            for path in entries {
                let meta = std::fs::symlink_metadata(&path).unwrap();
                out.push(entry(root, &path, &meta));
                if meta.is_dir() {
                    walk(root, &path, out);
                }
            }
        }
        fn entry(root: &Path, path: &Path, meta: &std::fs::Metadata) -> (PathBuf, char, u32) {
            let kind = if meta.is_dir() {
                'd'
            } else if meta.is_symlink() {
                'l'
            } else if meta.is_file() {
                'f'
            } else {
                '?'
            };
            let rel = path.strip_prefix(root).unwrap().to_owned();
            (rel, kind, meta.mode() & 0o7777)
        }
        let meta = std::fs::symlink_metadata(dir).unwrap();
        let mut out = vec![entry(dir, dir, &meta)];
        walk(dir, dir, &mut out);
        out
    }

    /// Outside the namespace: make `case`'s tree, run it inside as `test`,
    /// and check the run reached its sentinel and changed nothing it must
    /// keep.
    fn outer(userns: &Userns, test: &str, case: &Case) {
        let tmp = tempfile::tempdir().unwrap();
        // The mount table names canonical paths, and so does the clause.
        let base = tmp.path().canonicalize().unwrap();
        for dir in case.dirs {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        for file in case.files {
            std::fs::write(base.join(file), b"stored").unwrap();
        }
        let kept = base.join(case.unchanged);
        let before = tree(&kept);

        let binds: Vec<(PathBuf, PathBuf)> = case
            .binds
            .iter()
            .map(|(source, mount)| (base.join(source), base.join(mount)))
            .collect();
        let binds: Vec<(&Path, &Path)> = binds
            .iter()
            .map(|(source, mount)| (source.as_path(), mount.as_path()))
            .collect();
        let output = userns
            .command_binding(None, &binds)
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("bind_alias::{test}"),
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CASE, case.name)
            .env(BASE, &base)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let context = format!(
            "{test} {}: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            case.name, output.status
        );
        assert!(output.status.success(), "{context}");
        let sentinel = format!("{SENTINEL} {}", case.name);
        assert!(
            stdout.lines().any(|line| line.ends_with(&sentinel)),
            "the inner run never reached its sentinel: {context}"
        );
        assert!(!stderr.contains("panicked"), "{context}");
        assert_eq!(
            tree(&kept),
            before,
            "{} changed {}",
            case.name,
            kept.display()
        );
    }

    /// Run `cases` for the test `test`: inside the namespace when this is
    /// the inner run, else each case through the namespace in turn.
    fn scenario(test: &str, cases: &[Case]) {
        if let Some(name) = std::env::var_os(CASE) {
            let case = cases
                .iter()
                .find(|case| *case.name == name)
                .unwrap_or_else(|| panic!("{test}: no case {}", name.display()));
            let base = PathBuf::from(std::env::var_os(BASE).expect("the outer run sets the base"));
            inner(case, &base);
            println!("{SENTINEL} {}", case.name);
            return;
        }
        let Some(userns) = Userns::for_test(test) else {
            return;
        };
        for case in cases {
            outer(&userns, test, case);
        }
    }

    #[test]
    fn a_results_root_bind_mounted_from_inside_the_data_root_refuses() {
        scenario(
            "a_results_root_bind_mounted_from_inside_the_data_root_refuses",
            &[Case {
                name: "results-alias-of-a-data-hour",
                dirs: &["data/prod/2024-01-16/10", "results-alias"],
                files: &["data/prod/2024-01-16/10/events.parquet"],
                binds: &[("data/prod/2024-01-16/10", "results-alias")],
                data: "data",
                wal: None,
                scratch: "scratch",
                results: "results-alias",
                call: Call::Results(Refusal {
                    overlap: "is inside",
                    what: DATA_ROOT,
                    root: "data",
                    mount: "results-alias",
                }),
                unchanged: "",
            }],
        );
    }

    #[test]
    fn a_bind_alias_refuses_against_every_protected_root() {
        scenario(
            "a_bind_alias_refuses_against_every_protected_root",
            &[
                Case {
                    name: "results-vs-wal",
                    dirs: &["data", "wal/sub", "results-alias"],
                    files: &["wal/sub/00000001.wal"],
                    binds: &[("wal/sub", "results-alias")],
                    data: "data",
                    wal: Some("wal"),
                    scratch: "scratch",
                    results: "results-alias",
                    call: Call::Results(Refusal {
                        overlap: "is inside",
                        what: WAL_DIR,
                        root: "wal",
                        mount: "results-alias",
                    }),
                    unchanged: "",
                },
                Case {
                    name: "results-vs-scratch",
                    dirs: &["data", "scratch/sub", "results-alias"],
                    files: &["scratch/sub/spill.tmp"],
                    binds: &[("scratch/sub", "results-alias")],
                    data: "data",
                    wal: Some("wal"),
                    scratch: "scratch",
                    results: "results-alias",
                    call: Call::Results(Refusal {
                        overlap: "is inside",
                        what: SCRATCH_DIR,
                        root: "scratch",
                        mount: "results-alias",
                    }),
                    unchanged: "",
                },
                Case {
                    name: "scratch-vs-data",
                    dirs: &["data/prod/2024-01-16/10", "scratch-alias"],
                    files: &["data/prod/2024-01-16/10/events.parquet"],
                    binds: &[("data/prod/2024-01-16/10", "scratch-alias")],
                    data: "data",
                    wal: Some("wal"),
                    scratch: "scratch-alias",
                    results: "results",
                    call: Call::Scratch(Refusal {
                        overlap: "is inside",
                        what: DATA_ROOT,
                        root: "data",
                        mount: "scratch-alias",
                    }),
                    unchanged: "",
                },
                Case {
                    name: "scratch-vs-wal",
                    dirs: &["data", "wal/sub", "scratch-alias"],
                    files: &["wal/sub/00000001.wal"],
                    binds: &[("wal/sub", "scratch-alias")],
                    data: "data",
                    wal: Some("wal"),
                    scratch: "scratch-alias",
                    results: "results",
                    call: Call::Scratch(Refusal {
                        overlap: "is inside",
                        what: WAL_DIR,
                        root: "wal",
                        mount: "scratch-alias",
                    }),
                    unchanged: "",
                },
            ],
        );
    }

    #[test]
    fn a_bind_alias_refuses_against_both_repin_staging_roots() {
        scenario(
            "a_bind_alias_refuses_against_both_repin_staging_roots",
            &[
                Case {
                    name: "results-vs-repin-shadow",
                    dirs: &["data", "data.repin-next/sub", "results-alias"],
                    files: &["data.repin-next/sub/staged.parquet"],
                    binds: &[("data.repin-next/sub", "results-alias")],
                    data: "data",
                    wal: Some("wal"),
                    scratch: "scratch",
                    results: "results-alias",
                    call: Call::Results(Refusal {
                        overlap: "is inside",
                        what: REPIN_SHADOW_ROOT,
                        root: "data.repin-next",
                        mount: "results-alias",
                    }),
                    unchanged: "",
                },
                Case {
                    name: "results-vs-repin-aside",
                    dirs: &["data", "data.repin-aside/sub", "results-alias"],
                    files: &["data.repin-aside/sub/staged.parquet"],
                    binds: &[("data.repin-aside/sub", "results-alias")],
                    data: "data",
                    wal: Some("wal"),
                    scratch: "scratch",
                    results: "results-alias",
                    call: Call::Results(Refusal {
                        overlap: "is inside",
                        what: REPIN_ASIDE_ROOT,
                        root: "data.repin-aside",
                        mount: "results-alias",
                    }),
                    unchanged: "",
                },
                Case {
                    name: "scratch-vs-repin-shadow",
                    dirs: &["data", "data.repin-next/sub", "scratch-alias"],
                    files: &["data.repin-next/sub/staged.parquet"],
                    binds: &[("data.repin-next/sub", "scratch-alias")],
                    data: "data",
                    wal: Some("wal"),
                    scratch: "scratch-alias",
                    results: "results",
                    call: Call::Scratch(Refusal {
                        overlap: "is inside",
                        what: REPIN_SHADOW_ROOT,
                        root: "data.repin-next",
                        mount: "scratch-alias",
                    }),
                    unchanged: "",
                },
                Case {
                    name: "scratch-vs-repin-aside",
                    dirs: &["data", "data.repin-aside/sub", "scratch-alias"],
                    files: &["data.repin-aside/sub/staged.parquet"],
                    binds: &[("data.repin-aside/sub", "scratch-alias")],
                    data: "data",
                    wal: Some("wal"),
                    scratch: "scratch-alias",
                    results: "results",
                    call: Call::Scratch(Refusal {
                        overlap: "is inside",
                        what: REPIN_ASIDE_ROOT,
                        root: "data.repin-aside",
                        mount: "scratch-alias",
                    }),
                    unchanged: "",
                },
            ],
        );
    }

    #[test]
    fn a_data_root_configured_through_a_bind_mount_is_seen_below_the_results_root() {
        scenario(
            "a_data_root_configured_through_a_bind_mount_is_seen_below_the_results_root",
            &[Case {
                name: "results-above-a-bound-data-root",
                dirs: &["real/data/prod/2024-01-16/10", "data-alias"],
                files: &["real/data/prod/2024-01-16/10/events.parquet"],
                binds: &[("real/data/prod", "data-alias")],
                data: "data-alias",
                wal: None,
                scratch: "scratch",
                results: "real/data",
                call: Call::Results(Refusal {
                    overlap: "is above",
                    what: DATA_ROOT,
                    root: "data-alias",
                    mount: "data-alias",
                }),
                unchanged: "",
            }],
        );
    }

    #[test]
    fn a_missing_results_root_below_a_bind_alias_refuses_before_anything_is_made() {
        scenario(
            "a_missing_results_root_below_a_bind_alias_refuses_before_anything_is_made",
            &[Case {
                name: "missing-results-below-an-alias",
                dirs: &["data/prod/2024-01-16/10", "results-alias"],
                files: &["data/prod/2024-01-16/10/events.parquet"],
                binds: &[("data/prod/2024-01-16/10", "results-alias")],
                data: "data",
                wal: None,
                scratch: "scratch",
                results: "results-alias/new",
                call: Call::Results(Refusal {
                    overlap: "is inside",
                    what: DATA_ROOT,
                    root: "data",
                    mount: "results-alias",
                }),
                unchanged: "",
            }],
        );
    }

    #[test]
    fn bind_mounts_of_sibling_directories_open_as_the_data_and_results_roots() {
        scenario(
            "bind_mounts_of_sibling_directories_open_as_the_data_and_results_roots",
            &[Case {
                name: "sibling-binds",
                dirs: &[
                    "real/a/prod/2024-01-16/10",
                    "real/b",
                    "data-alias",
                    "results-alias",
                ],
                files: &["real/a/prod/2024-01-16/10/events.parquet"],
                binds: &[("real/a", "data-alias"), ("real/b", "results-alias")],
                data: "data-alias",
                wal: None,
                scratch: "scratch",
                results: "results-alias",
                call: Call::Both,
                unchanged: "real/a",
            }],
        );
    }
}
