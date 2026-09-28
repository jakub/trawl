// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Two real `trawl trial` invocations run one at a time.
//!
//! Each case runs two of `trawl trial down --yes` and `trawl trial up`
//! against an empty temp `XDG_STATE_HOME`, with `docker` on `PATH`
//! replaced by a stub that passes preflight and lists an empty engine,
//! and `DOCKER_HOST` naming a socket the test binds, because preflight
//! trusts only a real socket. The stub's container listing blocks until
//! the test releases it, so the first invocation holds the lifecycle lock
//! while it lists. The second must report that it is waiting, and must
//! not reach its own listing until the first has finished. The stub
//! answers nothing past the listing, so `up` fails at its image check
//! after the listing and never creates a resource.

use std::io::{BufRead as _, BufReader};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const WAITING: &str = "trawl trial: waiting for another `trawl trial` command to finish";

/// Generous: a step only has to start a process and take a free lock.
const STEP: Duration = Duration::from_secs(60);

/// A `docker` that answers preflight as Docker 29 with Compose 5.5.1, and
/// lists no resources. `ps` logs its start and end with the calling
/// process's pid, and blocks until `release` exists.
fn stub_docker(dir: &Path) {
    let script = format!(
        r#"#!/bin/sh
d="{dir}"
case "$1" in
  version) printf '{{"Client":{{}},"Server":{{"Version":"29.7.2"}}}}' ;;
  compose) echo 5.5.1 ;;
  info) echo stub-engine ;;
  ps)
    echo "start $PPID" >> "$d/log"
    while [ ! -e "$d/release" ]; do sleep 0.05; done
    echo "end $PPID" >> "$d/log" ;;
  volume|network) ;;
  *) echo "unexpected docker $*" >&2; exit 99 ;;
esac
"#,
        dir = dir.display()
    );
    let path = dir.join("docker");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn trial(verb: &[&str], state: &Path, stub: &Path, host: &str) -> (Child, mpsc::Receiver<String>) {
    let path = std::env::join_paths(
        std::iter::once(stub.to_owned()).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_trawl"));
    for (key, _) in std::env::vars_os() {
        let key_text = key.to_string_lossy();
        if key_text.starts_with("TRAWL_") || key_text.starts_with("DOCKER_") {
            command.env_remove(&key);
        }
    }
    let mut child = command
        .arg("trial")
        .args(verb)
        .env("XDG_STATE_HOME", state)
        .env("HOME", state)
        .env("DOCKER_HOST", host)
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn trawl");
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (child, rx)
}

fn log(stub: &Path) -> Vec<String> {
    std::fs::read_to_string(stub.join("log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + STEP;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Wait for `child` and return whether it succeeded, with its stderr.
fn finish(mut child: Child, lines: &mpsc::Receiver<String>) -> (bool, Vec<String>) {
    let status = child.wait().unwrap();
    let stderr: Vec<String> = lines.iter().collect();
    (status.success(), stderr)
}

const DOWN: &[&str] = &["down", "--yes"];
const UP: &[&str] = &["up"];

/// Start `first`, hold it in its listing, start `second`, and check that
/// `second` waits for the lock and lists only after `first` has finished.
/// Returns the two results, first then second.
fn race(first: &[&str], second: &[&str]) -> [(bool, Vec<String>); 2] {
    let tmp = tempfile::tempdir().unwrap();
    let stub = tmp.path().join("stub");
    std::fs::create_dir(&stub).unwrap();
    stub_docker(&stub);
    let state = tmp.path().join("state");
    let socket = stub.join("docker.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let host = format!("unix://{}", socket.display());

    let (first, first_lines) = trial(first, &state, &stub, &host);
    wait_for("the first command's listing", || log(&stub).len() == 1);
    let first_pid = first.id();
    assert_eq!(log(&stub), [format!("start {first_pid}")]);

    let (second, second_lines) = trial(second, &state, &stub, &host);
    let second_pid = second.id();
    let deadline = Instant::now() + STEP;
    loop {
        let line = second_lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the second command reports that it waits");
        if line == WAITING {
            break;
        }
    }
    // Waiting, and still waiting: it has not reached its own listing.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(log(&stub), [format!("start {first_pid}")]);

    std::fs::write(stub.join("release"), "").unwrap();
    let first = finish(first, &first_lines);
    let second = finish(second, &second_lines);
    assert_eq!(
        log(&stub),
        [
            format!("start {first_pid}"),
            format!("end {first_pid}"),
            format!("start {second_pid}"),
            format!("end {second_pid}"),
        ],
        "the second listing starts only after the first command finished: {first:?} {second:?}"
    );
    let lock = state.join("trawl/trial.lock");
    assert!(lock.is_file(), "the lock file stays");
    assert!(
        !state.join("trawl/trial").exists(),
        "a trial directory was created: {first:?} {second:?}"
    );
    [first, second]
}

/// `up` passed its listing and stopped at the stub's first unanswered
/// call, the image check, having created nothing.
fn assert_up_stopped_after_listing((ok, stderr): &(bool, Vec<String>)) {
    assert!(!ok, "up cannot succeed against the stub: {stderr:?}");
    assert!(
        stderr
            .iter()
            .any(|l| l.contains("unexpected docker image inspect")),
        "{stderr:?}"
    );
}

fn assert_down_found_nothing((ok, stderr): &(bool, Vec<String>)) {
    assert!(ok, "trawl trial down failed: {stderr:?}");
    assert!(
        stderr.iter().any(|l| l.contains("nothing to delete")),
        "{stderr:?}"
    );
}

#[test]
fn concurrent_trial_commands_run_one_at_a_time() {
    let [first, second] = race(DOWN, DOWN);
    assert_down_found_nothing(&first);
    assert_down_found_nothing(&second);
}

#[test]
fn down_waits_for_up() {
    let [up, down] = race(UP, DOWN);
    assert_up_stopped_after_listing(&up);
    assert_down_found_nothing(&down);
}

#[test]
fn up_waits_for_down() {
    let [down, up] = race(DOWN, UP);
    assert_down_found_nothing(&down);
    assert_up_stopped_after_listing(&up);
}
