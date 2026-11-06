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
//!
//! A `down` that stops part way leaves its teardown marker. `up` then
//! refuses before it changes anything, and the next `down` finishes the
//! teardown. A resume refuses when the trial's volumes are gone.

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
    ///
    /// `oneoff.state` makes `ps` list a one-off container `0ne0ff` of
    /// trial [`TRIAL_ID`] in that state, as a `compose run` an interrupted
    /// command left. `container rm` of it removes it.
    ///
    /// `container rm` of the claim removes `claim.id`, unless `claim.rm.err`
    /// exists: then it fails with that stderr, after removing the claim
    /// when `claim.rm.lands` exists too.
    ///
    /// The file `volumes` holds the `volume ls` listing, one volume per
    /// line, and `volume.ls.err` makes the listing fail with that stderr.
    /// `volume rm` removes each named volume from the listing except the
    /// one `volume.rm.fail` names, and then fails if it skipped one, as
    /// Docker removes what it can. When `watch` names a file, `volume rm`
    /// appends that file's contents to `seen`.
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
    fi
    if [ -e "$d/oneoff.state" ]; then
      printf '{{"id":"0ne0ff","name":"trawl-trial-fleet-admin-run-1","state":"%s","project":"trawl-trial","trial":"{trial}","oneoff":"True"}}\n' "$(cat "$d/oneoff.state")"
    fi ;;
  "volume ls"*)
    if [ -e "$d/volume.ls.err" ]; then cat "$d/volume.ls.err" >&2; exit 1; fi
    if [ -e "$d/volumes" ]; then cat "$d/volumes"; fi ;;
  "network ls"*) ;;
  "volume rm"*)
    echo "$*" >> "$d/calls"
    if [ -e "$d/watch" ]; then cat "$(cat "$d/watch")" >> "$d/seen"; fi
    skipped=""
    for v in $(echo "$*" | sed 's/.* -- //'); do
      if [ -e "$d/volume.rm.fail" ] && [ "$v" = "$(cat "$d/volume.rm.fail")" ]; then skipped="$v"; continue; fi
      grep -v "\"name\":\"$v\"" "$d/volumes" > "$d/volumes.new"; mv "$d/volumes.new" "$d/volumes"
    done
    if [ -n "$skipped" ]; then echo "Error response from daemon: remove $skipped: volume is in use" >&2; exit 1; fi ;;
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
  "container rm"*)
    echo "$*" >> "$d/calls"
    case "$*" in *0ne0ff*) rm -f "$d/oneoff.state" ;; esac
    case "$*" in *c1a1m*)
      if [ -e "$d/claim.rm.err" ] && [ ! -e "$d/claim.rm.lands" ]; then cat "$d/claim.rm.err" >&2; exit 1; fi
      rm -f "$d/claim.id"
      if [ -e "$d/claim.rm.err" ]; then cat "$d/claim.rm.err" >&2; exit 1; fi ;;
    esac ;;
  *) echo "$*" >> "$d/calls" ;;
esac
"#,
            dir = stub.display(),
            trial = TRIAL_ID,
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
        self.trial_with(engine, |_| {})
    }

    /// [`Self::trial_on`], with `edit` applied to the state first.
    fn trial_with(&self, engine: &str, edit: impl FnOnce(&mut serde_json::Value)) -> PathBuf {
        let dir = self.state_home.join("trawl/trial");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .unwrap();
        let mut state = serde_json::json!({
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
        edit(&mut state);
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
        self.trawl_with(verb, &[])
    }

    /// [`Self::trawl`] with `env` set on top.
    fn trawl_with(&self, verb: &[&str], env: &[(&str, &str)]) -> Output {
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
            .envs(env.iter().copied())
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

/// An interrupted command left a one-off container that never started.
/// Without a terminal and without `--yes`, `down` deletes nothing, not
/// even that one-off: it lists it with the rest, then refuses. With
/// `--yes` it deletes it with everything else.
#[test]
fn down_deletes_nothing_before_it_is_confirmed() {
    let engine = Engine::new("engine-a");
    let dir = engine.trial_on("engine-a");
    engine.put("oneoff.state", "created");

    let out = engine.trawl(&["down"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("nothing was deleted"), "{stderr}");
    assert_eq!(engine.calls(), "", "down deleted before it was confirmed");
    assert!(dir.join("state.json").is_file(), "{stderr}");
    assert!(
        stdout.contains("  container trawl-trial-fleet-admin-run-1  created\n"),
        "the one-off is not listed for approval: {stdout}"
    );

    let out = engine.trawl(&["down", "--yes"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(engine.calls(), "container rm --force -- 0ne0ff\n");
    assert!(!dir.exists(), "{stderr}");
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

/// `status` warns about a connection variable only when it changes where
/// commands go, and reads `TRAWL_INSECURE` and `TRAWL_PROFILE` as the
/// argument parser does.
#[test]
fn status_warns_only_about_variables_that_move_commands() {
    let engine = Engine::new("engine-a");
    engine.trial_on("engine-a");
    let warnings = |env: &[(&str, &str)]| {
        let out = engine.trawl_with(&["status"], env);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "{env:?}: {stderr}");
        stderr
            .lines()
            .filter(|line| line.starts_with("trawl: warning: "))
            .map(|line| line.split(' ').nth(2).unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let none: Vec<String> = Vec::new();
    // `TRAWL_PROFILE=trial` sends commands without -p to the trial, and
    // `TRAWL_INSECURE=false` leaves `-p trial` working.
    assert_eq!(warnings(&[("TRAWL_PROFILE", "trial")]), none);
    assert_eq!(warnings(&[("TRAWL_INSECURE", "false")]), none);
    for (name, value) in [
        ("TRAWL_PROFILE", "home"),
        ("TRAWL_PROFILE", ""),
        ("TRAWL_INSECURE", "true"),
        ("TRAWL_URL", ""),
        ("TRAWL_TOKEN", ""),
    ] {
        assert_eq!(warnings(&[(name, value)]), [name], "{name}={value:?}");
    }
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

/// Port 0 names no port: its bind test passes on an ephemeral port, and
/// the trial would record 0 and print addresses nothing listens on. The
/// argument parser refuses it, naming the flag, before anything is
/// created.
#[test]
fn up_refuses_port_zero() {
    for (zero, other) in [("--api-port", "--web-port"), ("--web-port", "--api-port")] {
        let engine = Engine::new("engine-a");
        let out = engine.trawl(&["up", zero, "0", other, &free_port(), "--no-sample-data"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{zero}: {stderr}");
        assert!(
            stderr.contains(&format!("'{zero} <PORT>'")),
            "{zero}: {stderr}"
        );
        assert!(!engine.dir().exists(), "{zero}: {stderr}");
        assert_eq!(engine.calls(), "", "{zero}: {stderr}");
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

impl Engine {
    /// A trial directory at 0700 that holds `files` and no `state.json`,
    /// as a state file removed by hand, or a removal that failed part way,
    /// leaves it.
    fn leftover(&self, files: &[(&str, &str)]) -> PathBuf {
        let dir = self.dir();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .unwrap();
        for (name, body) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        dir
    }
}

/// A trial directory without a state still holds files, and the engine
/// holds nothing of a trial's. Without a terminal and without `--yes`,
/// `down` lists what the directory holds, deletes nothing, and exits
/// non-zero. With `--yes` it deletes the directory.
#[test]
fn down_asks_before_it_deletes_a_directory_without_state() {
    let engine = Engine::new("engine-a");
    let dir = engine.leftover(&[("operator.token", "kept\n"), ("notes/todo.txt", "mine\n")]);

    let out = engine.trawl(&["down"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("nothing was deleted"), "{stderr}");
    for (name, body) in [("operator.token", "kept\n"), ("notes/todo.txt", "mine\n")] {
        assert_eq!(
            std::fs::read_to_string(dir.join(name)).ok().as_deref(),
            Some(body),
            "down deleted {name} before it was confirmed: {stderr}"
        );
    }
    for row in ["    notes/\n", "    operator.token\n"] {
        assert!(stdout.contains(row), "{row:?} is not listed: {stdout}");
    }
    assert_eq!(engine.calls(), "");

    let out = engine.trawl(&["down", "--yes"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!dir.exists(), "{stderr}");
    assert_eq!(engine.calls(), "");
}

/// (c) over a trial directory that already held files but no state: the
/// foreign claim removes only the state this `up` wrote, and the files
/// and the directory stay.
#[test]
fn a_foreign_claim_keeps_files_the_new_state_did_not_write() {
    let engine = Engine::new("engine-a");
    let dir = engine.leftover(&[("notes.txt", "mine\n")]);
    engine.put("race.id", THEIRS);
    let stderr = engine.up();
    assert!(
        stderr.contains(&format!("trawl-trial-claim (trial id {THEIRS})")),
        "{stderr}"
    );
    assert!(!dir.join("state.json").exists(), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.txt"))
            .ok()
            .as_deref(),
        Some("mine\n"),
        "up deleted a file it did not write: {stderr}"
    );
}

/// An empty trial directory without a state is a deletion like any
/// other: `down` lists it, and without a terminal and without `--yes` it
/// exits non-zero and leaves it. With `--yes` it removes it, and a second
/// `down`, with no trial directory left, exits 0.
#[test]
fn down_asks_before_it_removes_an_empty_directory() {
    let engine = Engine::new("engine-a");
    let dir = engine.leftover(&[]);

    let out = engine.trawl(&["down"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("nothing was deleted"), "{stderr}");
    assert!(dir.is_dir(), "down removed the directory unasked: {stderr}");
    assert!(
        stdout.contains(&format!("  {}\n", dir.display())),
        "the directory is not listed: {stdout}"
    );

    let out = engine.trawl(&["down", "--yes"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!dir.exists(), "{stderr}");
    assert!(dir.parent().unwrap().is_dir(), "{stderr}");

    let out = engine.trawl(&["down"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(engine.calls(), "");
}

/// What `up` says while a teardown marker exists.
const TEARDOWN_INCOMPLETE: &str = "trial teardown is incomplete. run `trawl trial down` to \
     finish deleting it, then `trawl trial up` to create a new trial.";

/// The trial's volumes, as Docker names them.
const VOLUMES: [&str; 3] = [
    "trawl-trial_postgres",
    "trawl-trial_trawld",
    "trawl-trial_web",
];

impl Engine {
    /// List `names` as volumes of trial `trial`.
    fn volumes(&self, trial: &str, names: &[&str]) {
        let mut listing = String::new();
        for name in names {
            let volume = serde_json::json!({
                "name": name,
                "project": "trawl-trial",
                "trial": trial,
            });
            listing.push_str(&volume.to_string());
            listing.push('\n');
        }
        self.put("volumes", &listing);
    }

    /// The names of the volumes the engine still lists.
    fn volumes_left(&self) -> Vec<String> {
        std::fs::read_to_string(self.stub.join("volumes"))
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let volume: serde_json::Value = serde_json::from_str(line).unwrap();
                volume["name"].as_str().unwrap().to_owned()
            })
            .collect()
    }

    fn marker(&self) -> PathBuf {
        self.state_home.join("trawl/trial.teardown")
    }

    /// Write a teardown marker for trial `trial` created on `engine`, at
    /// 0600, as `down` writes it.
    fn put_marker(&self, trial: &str, engine: &str) {
        use std::io::Write as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(self.state_home.join("trawl"))
            .unwrap();
        let body = serde_json::json!({ "trial_id": trial, "engine_id": engine });
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.marker())
            .unwrap()
            .write_all(format!("{body}\n").as_bytes())
            .unwrap();
    }

    /// A trial on `engine-a` that resumes on this engine: free ports, the
    /// images the stub answers with, the claim, and the phases `phases`.
    fn resumable(&self, phases: &serde_json::Value) -> PathBuf {
        self.put("claim.id", TRIAL_ID);
        self.trial_with("engine-a", |state| {
            state["ports"] = serde_json::json!({
                "api": free_port().parse::<u16>().unwrap(),
                "web": free_port().parse::<u16>().unwrap(),
            });
            state["images"]["postgres"]["id"] = "sha256:aaaa".into();
            state["phases"] = phases.clone();
        })
    }
}

/// A trial on `engine-a` with its claim and all three volumes. `down
/// --yes` fails with the stub files `fault` in place, `left` checks what
/// it left, and `after` changes the host as a kill at that point would.
/// Then `up` refuses with no call that changes anything, `down --yes`
/// finishes the teardown once the fault is gone, and `up` creates a new
/// trial.
fn interrupt_down(
    fault: &[(&str, &str)],
    left: impl FnOnce(&Engine, &std::path::Path),
    after: impl FnOnce(&Engine, &std::path::Path),
) {
    let engine = Engine::new("engine-a");
    let dir = engine.trial_on("engine-a");
    engine.put("claim.id", TRIAL_ID);
    engine.volumes(TRIAL_ID, &VOLUMES);
    for (name, body) in fault {
        engine.put(name, body);
    }

    let out = engine.trawl(&["down", "--yes"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{fault:?}: {stderr}");
    assert!(engine.marker().is_file(), "{fault:?}: no marker: {stderr}");
    assert!(dir.is_dir(), "{fault:?}: {stderr}");
    left(&engine, &dir);
    after(&engine, &dir);

    let before = engine.calls();
    let out = engine.trawl(&["up", "--no-sample-data"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{fault:?}: {stderr}");
    assert!(stderr.contains(TEARDOWN_INCOMPLETE), "{fault:?}: {stderr}");
    assert_eq!(engine.calls(), before, "{fault:?}: up changed the engine");

    for (name, _) in fault {
        engine.remove(name);
    }
    let out = engine.trawl(&["down", "--yes"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{fault:?}: {stderr}");
    assert!(
        !dir.exists() && !engine.marker().exists(),
        "{fault:?}: {stderr}"
    );
    assert_eq!(engine.volumes_left(), Vec::<String>::new(), "{fault:?}");
    assert_eq!(engine.claim(), None, "{fault:?}");

    let stderr = engine.up();
    assert!(stderr.contains(PAST_THE_CLAIM), "{fault:?}: {stderr}");
    let id = engine.state().expect("up wrote a state");
    assert_ne!(id, TRIAL_ID, "{fault:?}: up resumed the deleted trial");
}

/// Each way a `down` stops part way leaves a marker that `up` refuses,
/// and the next `down` finishes.
#[test]
fn trial_teardown_interruptions() {
    // A partial `volume rm`: Docker removed the volumes it could.
    interrupt_down(
        &[("volume.rm.fail", "trawl-trial_trawld")],
        |engine, _| {
            assert_eq!(engine.volumes_left(), ["trawl-trial_trawld"]);
            assert_eq!(engine.claim().as_deref(), Some(TRIAL_ID));
        },
        |_, _| {},
    );
    // Every volume gone, the claim left.
    interrupt_down(
        &[("claim.rm.err", RESPONSE_DENIED)],
        |engine, _| {
            assert_eq!(engine.volumes_left(), Vec::<String>::new());
            assert_eq!(engine.claim().as_deref(), Some(TRIAL_ID));
        },
        |_, _| {},
    );
    // The claim gone, the trial directory left.
    let claim_gone = |engine: &Engine, dir: &std::path::Path| {
        assert_eq!(engine.claim(), None);
        assert!(dir.join("state.json").is_file());
    };
    interrupt_down(
        &[("claim.rm.err", RESPONSE_DENIED), ("claim.rm.lands", "")],
        claim_gone,
        |_, _| {},
    );
    // The same, killed inside the directory removal after `state.json`
    // went: `down` finds the trial by the marker.
    interrupt_down(
        &[("claim.rm.err", RESPONSE_DENIED), ("claim.rm.lands", "")],
        claim_gone,
        |_, dir| std::fs::remove_file(dir.join("state.json")).unwrap(),
    );
}

/// A resume needs all three volumes once the state records a step: each
/// one missing is refused before `up` changes anything. A state that
/// records no step starts without them, and a listing that fails is
/// reported as itself, never as a missing volume.
#[test]
fn trial_resume_storage_matrix() {
    let database = serde_json::json!({
        "database": true, "fleet_migrated": false, "tls": false, "services_verified": false,
    });
    for gone in VOLUMES {
        let engine = Engine::new("engine-a");
        engine.resumable(&database);
        let left: Vec<&str> = VOLUMES.into_iter().filter(|v| *v != gone).collect();
        engine.volumes(TRIAL_ID, &left);
        let stderr = engine.up();
        assert!(
            stderr.contains(&format!("the trial's Docker volumes are gone: {gone}.")),
            "{gone}: {stderr}"
        );
        assert!(stderr.contains("`trawl trial down`"), "{gone}: {stderr}");
        assert_eq!(engine.calls(), "", "{gone}: refused after setup began");
    }

    // The control: with all three, the resume goes past the claim.
    let engine = Engine::new("engine-a");
    engine.resumable(&database);
    engine.volumes(TRIAL_ID, &VOLUMES);
    let stderr = engine.up();
    assert!(!stderr.contains("volumes are gone"), "{stderr}");
    assert!(
        stderr.contains("the stub engine runs no services"),
        "{stderr}"
    );

    // No recorded step: the volumes are made by this `up`.
    let engine = Engine::new("engine-a");
    engine.resumable(&serde_json::json!({
        "database": false, "fleet_migrated": false, "tls": false, "services_verified": false,
    }));
    let stderr = engine.up();
    assert!(stderr.contains(PAST_THE_CLAIM), "{stderr}");

    // The volume listing fails: its own error, not a missing volume.
    let engine = Engine::new("engine-a");
    engine.resumable(&database);
    engine.volumes(TRIAL_ID, &VOLUMES);
    let denied = "Error response from daemon: authorization denied by plugin authz: volume list";
    engine.put("volume.ls.err", denied);
    let stderr = engine.up();
    assert!(stderr.contains(denied), "{stderr}");
    assert!(!stderr.contains("volumes are gone"), "{stderr}");
    assert_eq!(engine.calls(), "");
}

/// A `down` killed after the directory went and before the marker did:
/// the next `down` says it is finishing the teardown, removes the
/// marker, and exits 0 without asking. On another engine it refuses and
/// keeps the marker.
#[test]
fn marker_without_dir_cleared_by_down() {
    let engine = Engine::new("engine-a");
    let dir = engine.trial_on("engine-a");
    engine.put("claim.id", TRIAL_ID);
    engine.volumes(TRIAL_ID, &VOLUMES);
    engine.put("claim.rm.err", RESPONSE_DENIED);
    engine.put("claim.rm.lands", "");
    let out = engine.trawl(&["down", "--yes"]);
    assert!(!out.status.success());
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(engine.marker().is_file());

    let before = engine.calls();
    let out = engine.trawl(&["down"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("finishing an interrupted teardown"),
        "{stderr}"
    );
    assert!(!engine.marker().exists(), "{stderr}");
    assert_eq!(engine.calls(), before, "{stderr}");
    assert!(engine.state_home.join("trawl/trial.lock").is_file());

    let elsewhere = Engine::new("engine-b");
    elsewhere.put_marker(TRIAL_ID, "engine-a");
    let out = elsewhere.trawl(&["down"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("engine-a"), "{stderr}");
    assert!(elsewhere.marker().is_file(), "{stderr}");
}

/// `down` reads only the trial id and engine from a state of any schema,
/// and the state's id wins over a marker naming another trial: the
/// marker is rewritten with the state's id before the first deletion.
#[test]
fn trial_down_any_schema() {
    let engine = Engine::new("engine-a");
    let dir = engine.trial_with("engine-a", |state| {
        state["schema"] = 99.into();
        state["written_by_a_later_trawl"] = serde_json::json!({ "anything": [1, 2] });
    });
    engine.put_marker(THEIRS, "engine-b");
    engine.put("claim.id", TRIAL_ID);
    engine.volumes(TRIAL_ID, &VOLUMES);
    engine.put("watch", &engine.marker().display().to_string());

    let out = engine.trawl(&["down", "--yes"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let calls = engine.calls();
    assert!(
        calls.contains(&format!("volume rm -- {}\n", VOLUMES.join(" "))),
        "{calls}"
    );
    assert!(calls.contains("container rm --force -- c1a1m\n"), "{calls}");
    assert!(!calls.contains(THEIRS), "{calls}");
    let seen = std::fs::read_to_string(engine.stub.join("seen")).unwrap();
    let seen: serde_json::Value = serde_json::from_str(&seen).unwrap();
    assert_eq!(
        seen,
        serde_json::json!({ "trial_id": TRIAL_ID, "engine_id": "engine-a" })
    );
    assert!(!dir.exists() && !engine.marker().exists(), "{stderr}");
}

/// `status` reports a teardown marker in place of the trial, and exits 0
/// without reading the state; `stop` ignores the marker.
#[test]
fn status_reports_and_stop_ignores_a_teardown_marker() {
    let engine = Engine::new("engine-a");
    engine.trial_with("engine-a", |state| state["schema"] = 99.into());
    engine.put_marker(TRIAL_ID, "engine-a");

    let out = engine.trawl(&["status"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stdout.starts_with("Trial teardown is incomplete"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Run `trawl trial down` to finish deleting it"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&engine.marker().display().to_string()),
        "{stdout}"
    );
    assert_eq!(engine.calls(), "");

    let engine = Engine::new("engine-a");
    engine.trial_on("engine-a");
    engine.put_marker(TRIAL_ID, "engine-a");
    let out = engine.trawl(&["stop"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the stub runs no services: {stderr}");
    assert!(
        stderr.contains("stopping the trial's containers"),
        "{stderr}"
    );
    assert!(!stderr.contains("teardown"), "{stderr}");
    assert!(engine.calls().contains(" stop"), "{}", engine.calls());
    assert!(engine.marker().is_file());
}
