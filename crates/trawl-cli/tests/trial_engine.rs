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
//! deletes that state again only when its own inspect finds another
//! trial's claim.

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
    /// For `up`, it has every image, as `sha256:aaaa`, and runs no
    /// services: every `compose` call fails. The engine's claim is the
    /// file `claim.id`, holding the id it carries: `ps` lists it,
    /// `container inspect` prints its labels, and `container create`
    /// refuses the name while it exists. Otherwise the create makes it,
    /// unless `create.err` exists: then the create fails with that stderr,
    /// after making the claim when `create.lands` exists too, as an
    /// engine whose authorization plugin denies the response does.
    /// `race.id` becomes the claim when the create runs, as another trial
    /// winning the race after `up` listed the engine. `inspect.err` makes
    /// `container inspect` of the claim fail with that stderr.
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
  compose*) echo "$*" >> "$d/calls"; echo "the stub engine runs no services" >&2; exit 1 ;;
  info*) echo {id} ;;
  ps*)
    if [ -e "$d/claim.id" ]; then
      printf '{{"id":"c1a1m","name":"trawl-trial-claim","state":"created","project":"","trial":"%s","oneoff":""}}\n' "$(cat "$d/claim.id")"
    fi ;;
  "volume ls"*|"network ls"*) ;;
  "image inspect"*) printf '{{"id":"sha256:aaaa","digests":[]}}' ;;
  "container create"*)
    echo "$*" >> "$d/calls"
    if [ -e "$d/race.id" ]; then mv "$d/race.id" "$d/claim.id"; fi
    if [ -e "$d/claim.id" ]; then
      echo 'Error response from daemon: Conflict. The container name "/trawl-trial-claim" is already in use by container "c1a1m".' >&2
      exit 1
    fi
    if [ -e "$d/create.err" ] && [ ! -e "$d/create.lands" ]; then cat "$d/create.err" >&2; exit 1; fi
    echo "$*" | sed 's/.*sh[.]trawl[.]trial[.]id=\([0-9a-f]*\).*/\1/' > "$d/claim.id"
    if [ -e "$d/create.err" ]; then cat "$d/create.err" >&2; exit 1; fi
    echo c1a1m ;;
  "container inspect"*.Image*) echo '"sha256:aaaa"' ;;
  "container inspect"*)
    if [ -e "$d/inspect.err" ]; then cat "$d/inspect.err" >&2; exit 1; fi
    if [ -e "$d/claim.id" ]; then printf '{{"sh.trawl.trial.id":"%s"}}' "$(cat "$d/claim.id")"
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

/// Another trial's id.
const THEIRS: &str = "fedcba9876543210fedcba9876543210";

/// How the Docker CLI reports a request the engine carried out and an
/// authorization plugin then denied the response to.
const RESPONSE_DENIED: &str =
    "Error response from daemon: authorization denied by plugin authz: response denied";

/// The first step `up` takes once it holds the claim. The stub runs no
/// services, so `up` fails there.
const PAST_THE_CLAIM: &str = "writing the PostgreSQL superuser password";

impl Engine {
    fn put(&self, name: &str, body: &str) {
        std::fs::write(self.stub.join(name), body).unwrap();
    }

    fn remove(&self, name: &str) {
        std::fs::remove_file(self.stub.join(name)).unwrap();
    }

    /// The trial id the engine's claim carries.
    fn claim(&self) -> Option<String> {
        std::fs::read_to_string(self.stub.join("claim.id"))
            .ok()
            .map(|id| id.trim().to_owned())
    }

    fn dir(&self) -> PathBuf {
        self.state_home.join("trawl/trial")
    }

    /// The trial id the state records, when there is a state.
    fn state(&self) -> Option<String> {
        let bytes = std::fs::read(self.dir().join("state.json")).ok()?;
        let state: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(state["engine_id"], "engine-a");
        Some(state["trial_id"].as_str().unwrap().to_owned())
    }

    /// How many claim creates `up` ran.
    fn creates(&self) -> usize {
        self.calls()
            .lines()
            .filter(|call| {
                call.starts_with(
                    "container create --name trawl-trial-claim --label sh.trawl.trial.id=",
                )
            })
            .count()
    }

    /// `up`, which the stub always fails; returns its stderr. The first
    /// `up` picks free ports, and a rerun resumes on the recorded ones.
    fn up(&self) -> String {
        let out = if self.state().is_some() {
            self.trawl(&["up", "--no-sample-data"])
        } else {
            let (api, web) = (free_port(), free_port());
            self.trawl(&[
                "up",
                "--api-port",
                &api,
                "--web-port",
                &web,
                "--no-sample-data",
            ])
        };
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(!out.status.success(), "{stderr}");
        stderr
    }
}

/// (a) The engine made the claim and the response to the create was
/// denied. Our inspect finds the claim with our id, so `up` goes on,
/// and so does a rerun.
#[test]
fn a_denied_create_response_goes_on_when_the_claim_is_ours() {
    let engine = Engine::new("engine-a");
    engine.put("create.err", RESPONSE_DENIED);
    engine.put("create.lands", "");
    let stderr = engine.up();
    assert!(stderr.contains(PAST_THE_CLAIM), "{stderr}");
    let id = engine.state().expect("the state stays");
    assert_eq!(engine.claim().as_deref(), Some(id.as_str()));

    let stderr = engine.up();
    assert!(stderr.contains(PAST_THE_CLAIM), "{stderr}");
    assert_eq!(engine.state().as_deref(), Some(id.as_str()));
    assert_eq!(engine.claim().as_deref(), Some(id.as_str()));
}

/// (b) The engine made the claim, the response to the create was denied,
/// and the inspect failed too. Nothing proves the claim is not ours, so
/// the state stays, and a rerun finds the claim with its own id instead
/// of refusing it as an orphan.
#[test]
fn a_denied_create_response_and_a_failed_inspect_keep_the_state() {
    let engine = Engine::new("engine-a");
    engine.put("create.err", RESPONSE_DENIED);
    engine.put("create.lands", "");
    engine.put(
        "inspect.err",
        "Error response from daemon: authorization denied by plugin authz: request denied",
    );
    let stderr = engine.up();
    assert!(stderr.contains("response denied"), "{stderr}");
    let id = engine.state().expect("the state stays");
    assert_eq!(engine.claim().as_deref(), Some(id.as_str()));

    engine.remove("inspect.err");
    let stderr = engine.up();
    assert!(stderr.contains(PAST_THE_CLAIM), "{stderr}");
    assert_eq!(engine.state().as_deref(), Some(id.as_str()));
}

/// (c) Another trial took the claim after `up` listed the engine. Our
/// inspect shows its id, so nothing of ours was created: `up` refuses,
/// and the state it wrote goes.
#[test]
fn a_foreign_claim_removes_the_new_state() {
    let engine = Engine::new("engine-a");
    engine.put("race.id", THEIRS);
    let stderr = engine.up();
    assert!(
        stderr.contains(&format!("trawl-trial-claim (trial id {THEIRS})")),
        "{stderr}"
    );
    assert!(!engine.dir().exists(), "{stderr}");
    assert_eq!(engine.claim().as_deref(), Some(THEIRS));
}

/// (d) The engine refused the create and holds no claim. The state stays,
/// and a rerun creates the claim with the recorded id.
#[test]
fn a_refused_create_keeps_the_state_and_a_rerun_creates_the_claim() {
    let engine = Engine::new("engine-a");
    engine.put(
        "create.err",
        "Error response from daemon: No such image: sha256:aaaa",
    );
    let stderr = engine.up();
    assert!(stderr.contains("No such image"), "{stderr}");
    let id = engine.state().expect("the state stays");
    assert_eq!(engine.claim(), None);

    engine.remove("create.err");
    let stderr = engine.up();
    assert!(stderr.contains(PAST_THE_CLAIM), "{stderr}");
    assert_eq!(engine.claim().as_deref(), Some(id.as_str()));
    assert_eq!(engine.creates(), 2);
}

/// The create may have reached the engine before the connection dropped;
/// the state stays for the same reason.
#[test]
fn a_dropped_create_keeps_the_state() {
    let engine = Engine::new("engine-a");
    engine.put(
        "create.err",
        "error during connect: Post \"http://%2Fvar%2Frun%2Fdocker.sock/v1.52/containers/create?name=trawl-trial-claim\": EOF",
    );
    let stderr = engine.up();
    assert!(stderr.contains("error during connect"), "{stderr}");
    assert!(engine.state().is_some(), "{stderr}");
}

/// (e) `down` deletes a kept state that owns nothing on the engine.
#[test]
fn down_removes_a_kept_state_that_owns_nothing() {
    let engine = Engine::new("engine-a");
    engine.put(
        "create.err",
        "Error response from daemon: No such image: sha256:aaaa",
    );
    engine.up();
    assert!(engine.state().is_some());

    let out = engine.trawl(&["down", "--yes"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!engine.dir().exists(), "{stderr}");
    assert_eq!(engine.creates(), 1);
    assert_eq!(engine.calls().lines().count(), 1, "down removed nothing");
}
