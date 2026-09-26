// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl trial` verbs against a stub engine: which engine they act on,
//! and what a failed engine claim leaves behind.
//!
//! `docker` on `PATH` is a stub for an empty engine whose id the test
//! chooses, and `DOCKER_HOST` names a socket the test binds, because
//! preflight trusts only a real socket.
//!
//! `down`, `stop`, and `status` act only on the engine the trial was
//! created on. The trial state records engine `engine-a`. On the wrong
//! engine, `down --yes` would find nothing of the trial's, delete the
//! state directory, and report success while the trial's containers and
//! volumes stay on `engine-a`. `down` and `stop` must refuse, name both
//! engines, and leave the state and the engine alone; `status` must not
//! list the wrong engine's containers as the trial's.
//!
//! A first `up` writes its state before it creates the engine claim, and
//! deletes that state again only when the claim was refused for certain.

use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const TRIAL_ID: &str = "0123456789abcdef0123456789abcdef";

/// A stub engine: the `docker` program and the socket `DOCKER_HOST`
/// names.
struct Engine {
    _tmp: tempfile::TempDir,
    stub: PathBuf,
    host: String,
    state_home: PathBuf,
    _listener: std::os::unix::net::UnixListener,
}

impl Engine {
    /// A `docker` that answers preflight as Docker 29 with Compose 5.5.1
    /// on engine `id`, lists no resources, and logs every other call.
    ///
    /// For `up`, it has every image, as `sha256:aaaa`. `container create`
    /// fails with the stderr in `create.err`, and `container inspect`
    /// prints `claim.labels`, or reports no such container when that file
    /// does not exist.
    fn new(id: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let stub = tmp.path().join("stub");
        std::fs::create_dir(&stub).unwrap();
        let script = format!(
            r#"#!/bin/sh
d="{dir}"
case "$*" in
  version*) printf '{{"Client":{{}},"Server":{{"Version":"29.7.2"}}}}' ;;
  "compose version --short") echo 5.5.1 ;;
  info*) echo {id} ;;
  ps*|"volume ls"*|"network ls"*) ;;
  "image inspect"*) printf '{{"id":"sha256:aaaa","digests":[]}}' ;;
  "container create"*) echo "$*" >> "$d/calls"; cat "$d/create.err" >&2; exit 1 ;;
  "container inspect"*)
    if [ -e "$d/claim.labels" ]; then cat "$d/claim.labels"
    else echo "Error: No such container: trawl-trial-claim" >&2; exit 1; fi ;;
  *) echo "$*" >> "$d/calls" ;;
esac
"#,
            dir = stub.display()
        );
        let program = stub.join("docker");
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let socket = stub.join("docker.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let host = format!("unix://{}", socket.display());
        let state_home = tmp.path().join("state");
        Self {
            _tmp: tmp,
            stub,
            host,
            state_home,
            _listener: listener,
        }
    }

    /// Write a complete schema-1 trial created on `engine`, with a
    /// Compose file so that `stop` has something to run. Returns the
    /// trial directory.
    fn trial_on(&self, engine: &str) -> PathBuf {
        let dir = self.state_home.join("trawl/trial");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .unwrap();
        let state = serde_json::json!({
            "schema": 1,
            "trial_id": TRIAL_ID,
            "project": "trawl-trial",
            "cli_version": "0.9.0",
            "created_at": "2026-09-25T12:00:00Z",
            "engine_id": engine,
            "ports": { "api": 15514, "web": 18090 },
            "images": {
                "trawl": { "reference": "ghcr.io/jakub/trawl:0.9.0", "id": "sha256:aaaa", "repo_digest": null },
                "postgres": { "reference": "postgres:18", "id": "sha256:cccc", "repo_digest": null },
                "trawl_overridden": false,
            },
            "phases": { "database": true, "fleet_migrated": true, "tls": true, "services_verified": true },
            "tls": null,
            "keys": { "operator": null, "ingest": null },
            "samples": { "state": "skipped" },
        });
        for (name, body) in [
            ("state.json", state.to_string()),
            ("compose.json", "{}".to_owned()),
        ] {
            use std::io::Write as _;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(dir.join(name))
                .unwrap()
                .write_all(body.as_bytes())
                .unwrap();
        }
        dir
    }

    fn trawl(&self, verb: &[&str]) -> Output {
        let path = std::env::join_paths(std::iter::once(self.stub.clone()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_trawl"));
        for (key, _) in std::env::vars_os() {
            let key_text = key.to_string_lossy();
            if key_text.starts_with("TRAWL_") || key_text.starts_with("DOCKER_") {
                command.env_remove(&key);
            }
        }
        command
            .arg("trial")
            .args(verb)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("HOME", &self.state_home)
            .env("DOCKER_HOST", &self.host)
            .env("PATH", path)
            .stdin(Stdio::null())
            .output()
            .expect("run trawl")
    }

    /// Every call the stub did not answer as a read.
    fn calls(&self) -> String {
        std::fs::read_to_string(self.stub.join("calls")).unwrap_or_default()
    }
}

/// Run `verb` against a trial created on `engine-a` while `engine-b`
/// answers, and check it refused without touching either side.
fn refuses_another_engine(verb: &[&str]) {
    let engine = Engine::new("engine-b");
    let dir = engine.trial_on("engine-a");

    let out = engine.trawl(verb);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{verb:?} succeeded: {stderr}");
    assert!(
        stderr.contains("engine-a") && stderr.contains("engine-b"),
        "{verb:?} must name both engines: {stderr}"
    );
    assert!(
        stderr.contains("DOCKER_HOST") && stderr.contains("docker context use"),
        "{verb:?} must say how to select the engine: {stderr}"
    );
    assert!(
        dir.join("state.json").is_file(),
        "{verb:?} deleted the state: {stderr}"
    );
    assert_eq!(engine.calls(), "", "{verb:?} acted on the other engine");
}

#[test]
fn down_refuses_another_engine() {
    refuses_another_engine(&["down", "--yes"]);
}

#[test]
fn stop_refuses_another_engine() {
    refuses_another_engine(&["stop"]);
}

/// The control: on its own engine, `down` goes ahead. That engine is
/// empty here, so it deletes the state directory and nothing else.
#[test]
fn down_proceeds_on_its_own_engine() {
    let engine = Engine::new("engine-a");
    let dir = engine.trial_on("engine-a");
    let out = engine.trawl(&["down", "--yes"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!dir.exists(), "{stderr}");
    assert_eq!(engine.calls(), "");
}

/// `status` lists containers only from the trial's own engine.
#[test]
fn status_lists_containers_only_on_its_own_engine() {
    let containers = |engine_id: &str| {
        let engine = Engine::new(engine_id);
        engine.trial_on("engine-a");
        let out = engine.trawl(&["status"]);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let (_, section) = stdout
            .split_once("\nContainers\n")
            .unwrap_or_else(|| panic!("no Containers section: {stdout}"));
        section.to_owned()
    };
    assert_eq!(containers("engine-a"), "  none\n");
    let elsewhere = containers("engine-b");
    assert!(
        elsewhere.starts_with("  unknown: this command reached Docker engine engine-b"),
        "{elsewhere}"
    );
}

/// A free loopback port, for `up`'s bind test.
fn free_port() -> String {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.local_addr().unwrap().port().to_string()
}

/// A first `up` whose claim create fails with `create_err`, while the
/// claim inspect finds `claim` (its labels JSON) or nothing. Returns the
/// trial directory and `up`'s stderr.
fn first_up_with_claim(create_err: &str, claim: Option<&str>) -> (Engine, PathBuf, String) {
    let engine = Engine::new("engine-a");
    std::fs::write(engine.stub.join("create.err"), create_err).unwrap();
    if let Some(labels) = claim {
        std::fs::write(engine.stub.join("claim.labels"), labels).unwrap();
    }
    let (api, web) = (free_port(), free_port());
    let out = engine.trawl(&[
        "up",
        "--api-port",
        &api,
        "--web-port",
        &web,
        "--no-sample-data",
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success(), "{stderr}");
    let calls = engine.calls();
    assert!(
        calls.starts_with("container create --name trawl-trial-claim --label sh.trawl.trial.id="),
        "up stops at the claim: {calls:?}"
    );
    let dir = engine.state_home.join("trawl/trial");
    (engine, dir, stderr)
}

/// The create may have reached the engine and made the claim before the
/// connection dropped. The state stays, so a rerun finds that claim
/// carrying its own id instead of refusing it as an orphan.
#[test]
fn an_unknown_claim_outcome_keeps_the_state() {
    let (_engine, dir, stderr) = first_up_with_claim(
        "error during connect: Post \"http://%2Fvar%2Frun%2Fdocker.sock/v1.52/containers/create?name=trawl-trial-claim\": EOF",
        None,
    );
    assert!(stderr.contains("error during connect"), "{stderr}");
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["engine_id"], "engine-a");
    assert_eq!(state["trial_id"].as_str().unwrap().len(), 32);
}

/// Another trial holds the engine: nothing of ours was created, and the
/// state this `up` wrote goes.
#[test]
fn a_foreign_claim_removes_the_new_state() {
    let (_engine, dir, stderr) = first_up_with_claim(
        "Error response from daemon: Conflict. The container name \"/trawl-trial-claim\" is \
         already in use by container \"c0ffee\". You have to remove (or rename) that container \
         to be able to reuse that name.",
        Some(r#"{"sh.trawl.trial.id":"fedcba9876543210fedcba9876543210"}"#),
    );
    assert!(
        stderr.contains("trawl-trial-claim (trial id fedcba9876543210fedcba9876543210)"),
        "{stderr}"
    );
    assert!(!dir.exists(), "{stderr}");
}

/// The engine answered the create with an error, so it created nothing,
/// and the state this `up` wrote goes.
#[test]
fn an_engine_refusal_removes_the_new_state() {
    let (_engine, dir, stderr) = first_up_with_claim(
        "Error response from daemon: No such image: sha256:aaaa",
        None,
    );
    assert!(stderr.contains("No such image"), "{stderr}");
    assert!(!dir.exists(), "{stderr}");
}
