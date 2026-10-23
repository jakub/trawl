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
