// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl doctor` target and key selection, observed on the real binary
//! (ADR-0047).
//!
//! Every run strips the inherited `TRAWL_*` environment and gets a temp
//! `HOME`, so the default config path is one the test wrote or none at all.
//! Tests cannot set the environment in-process (`unsafe_code = forbid`), so
//! every case is a subprocess.
//!
//! Where a case must not reach a server, a plain TCP listener stands where a
//! wrong resolution would send the doctor, and records every connection and
//! every `Authorization` header it sees.

use std::io::Read as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

const SAVED_TOKEN: &str = "flt_savedservertokenvalue";

/// What a [`Recorder`] saw.
#[derive(Debug, Default)]
struct Seen {
    connections: usize,
    authorization_headers: usize,
}

/// A plain TCP listener on `127.0.0.1` that records, answers nothing, and
/// closes.
struct Recorder {
    port: u16,
    seen: Arc<Mutex<Seen>>,
}

impl Recorder {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let record = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                record.lock().unwrap().connections += 1;
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .ok();
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                if head.contains("\nauthorization:") {
                    record.lock().unwrap().authorization_headers += 1;
                }
            }
        });
        Self { port, seen }
    }

    fn url(&self, scheme: &str) -> String {
        format!("{scheme}://127.0.0.1:{}", self.port)
    }

    fn assert_untouched(&self, case: &str) {
        let seen = self.seen.lock().unwrap();
        assert_eq!(seen.connections, 0, "{case}: nothing may connect");
        assert_eq!(seen.authorization_headers, 0, "{case}: no key may be sent");
    }
}

/// A temp `HOME` (and `XDG_STATE_HOME`) for one test.
struct Sandbox {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state_home: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state_home = tmp.path().join("state");
        std::fs::create_dir(&home).unwrap();
        Self {
            _tmp: tmp,
            home,
            state_home,
        }
    }

    /// Write the default config file, `~/.config/trawl/config.toml`.
    fn default_config(&self, toml: &str) -> PathBuf {
        let dir = self.home.join(".config/trawl");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, toml).unwrap();
        path
    }

    fn file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.home.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn trawl(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_trawl"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("TRAWL_") {
                cmd.env_remove(key);
            }
        }
        cmd.env("HOME", &self.home)
            .env("XDG_STATE_HOME", &self.state_home)
            .args(args)
            .envs(env.iter().copied());
        cmd.output().expect("spawn trawl")
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A usage error: exit 2, nothing on stdout, and none of `secrets` echoed.
fn assert_usage_error(output: &Output, fragment: &str, secrets: &[&str], case: &str) {
    let all = text(output);
    assert_eq!(output.status.code(), Some(2), "{case}: exit 2, got {all}");
    assert!(
        output.stdout.is_empty(),
        "{case}: stdout stays empty: {all}"
    );
    assert!(
        all.contains(fragment),
        "{case}: must name {fragment:?}: {all}"
    );
    for secret in secrets {
        assert!(!all.contains(secret), "{case}: {secret:?} leaked: {all}");
    }
}

/// The JSON report of a run that got past selection.
fn report(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("stdout is not a JSON report ({e}): {}", text(output)))
}

fn connection_config(report: &serde_json::Value) -> &serde_json::Value {
    let check = &report["checks"][0];
    assert_eq!(check["id"], "connection.config");
    check
}

/// Each ambient variable refuses the run by name, never by value, even when
/// it is empty or a flag would shadow it.
#[test]
fn doctor_refuses_ambient_env() {
    const PLANTED: &str = "flt_plantedambientvalue";
    let home = Sandbox::new();
    let recorder = Recorder::start();
    let url = recorder.url("https");
    for name in [
        "TRAWL_URL",
        "TRAWL_PROFILE",
        "TRAWL_TOKEN",
        "TRAWL_INSECURE",
    ] {
        // TRAWL_INSECURE is a bool clap parses before the doctor runs: an
        // empty or malformed value is clap's own usage error (still exit 2,
        // and a bool is no secret), so its table holds values clap accepts.
        let values: &[&str] = if name == "TRAWL_INSECURE" {
            &["true", "false"]
        } else {
            &[PLANTED, ""]
        };
        for value in values {
            let case = format!("{name}={value:?}");
            let output = home.trawl(&["doctor", "--url", &url], &[(name, value)]);
            assert_usage_error(&output, &format!("{name} is set"), &[PLANTED], &case);
            if !value.is_empty() {
                assert!(
                    !text(&output).contains(&format!("{name}={value}")),
                    "{case}: the value is not shown"
                );
            }
        }
    }
    let output = home.trawl(&["doctor", "--url", &url], &[("TRAWL_INSECURE", "")]);
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(output.stdout.is_empty());
    recorder.assert_untouched("ambient environment");
}

/// Selection errors on the command line: all exit 2, before anything is
/// read or contacted.
#[test]
fn doctor_selection_usage_errors() {
    let home = Sandbox::new();
    let recorder = Recorder::start();
    let url = recorder.url("https");
    let key = home.file("key", "flt_keyfilevalue\n");
    let key = key.to_str().unwrap();
    let config = home.default_config(&format!("[profiles.prod]\nurl = \"{url}\"\n"));
    let config = config.to_str().unwrap();

    let cases: &[(&str, Vec<&str>, &str)] = &[
        ("no target", vec!["doctor"], "give --url or --profile"),
        (
            "both targets",
            vec!["doctor", "--url", &url, "-p", "prod"],
            "--url and --profile",
        ),
        (
            "--token with --url",
            vec!["doctor", "--url", &url, "--token", "flt_flagvalue"],
            "--token cannot be used",
        ),
        (
            "--token with --profile",
            vec!["doctor", "-p", "prod", "--token", "flt_flagvalue"],
            "--token cannot be used",
        ),
        (
            "-c with --url",
            vec!["doctor", "--url", &url, "-c", config],
            "-c cannot be used with --url",
        ),
        (
            "two key sources",
            vec![
                "doctor",
                "--url",
                &url,
                "--token-env",
                "SOME_KEY",
                "--token-file",
                key,
            ],
            "--token-env and --token-file",
        ),
        (
            "malformed --url",
            vec!["doctor", "--url", "localhost:5514"],
            "--url must be an http or https URL",
        ),
        (
            "plain http --web-url off loopback",
            vec!["doctor", "--url", &url, "--web-url", "http://trawl.example"],
            "--web-url must use https",
        ),
    ];
    for (case, args, fragment) in cases {
        let output = home.trawl(args, &[]);
        assert_usage_error(
            &output,
            fragment,
            &["flt_flagvalue", "flt_keyfilevalue"],
            case,
        );
    }
    recorder.assert_untouched("selection usage errors");
}

/// A URL carrying userinfo would reach the server as a Basic
/// `Authorization` header. `--url` and `--web-url` refuse it as a usage
/// error; a profile's url fails `connection.config`. The userinfo never
/// appears in any output.
#[test]
fn doctor_refuses_url_credentials() {
    let home = Sandbox::new();
    let recorder = Recorder::start();
    let port = recorder.port;
    let forms = [
        (
            "username only",
            format!("https://tr4wluser@127.0.0.1:{port}"),
        ),
        (
            "user and password",
            format!("https://tr4wluser:pa55word@127.0.0.1:{port}"),
        ),
    ];
    let secrets = ["tr4wluser", "pa55word"];
    for (form, with_creds) in &forms {
        let output = home.trawl(&["doctor", "--url", with_creds], &[]);
        assert_usage_error(&output, "--url carries credentials", &secrets, form);

        let output = home.trawl(
            &[
                "doctor",
                "--url",
                &recorder.url("https"),
                "--web-url",
                with_creds,
            ],
            &[],
        );
        assert_usage_error(&output, "--web-url carries credentials", &secrets, form);

        home.default_config(&format!("[profiles.creds]\nurl = \"{with_creds}\"\n"));
        let output = home.trawl(&["doctor", "-p", "creds", "--format", "json"], &[]);
        assert_eq!(output.status.code(), Some(1), "{form}: {}", text(&output));
        let report = report(&output);
        let check = connection_config(&report);
        assert_eq!(check["outcome"], "failed", "{form}");
        assert_eq!(check["reason"], "URL carries credentials", "{form}");
        assert!(
            check["source"]
                .as_str()
                .unwrap()
                .contains("[profiles.creds]"),
            "{form}: names the profile"
        );
        assert!(report["target"]["origin"].is_null());
        for secret in secrets {
            assert!(!text(&output).contains(secret), "{form}: {secret} leaked");
        }
    }
    recorder.assert_untouched("credentials in a URL");
}

/// Under a profile, every flag that would change its URL, key, or trust is
/// refused by name.
#[test]
fn doctor_profile_refuses_overrides() {
    let home = Sandbox::new();
    let recorder = Recorder::start();
    let url = recorder.url("https");
    home.default_config(&format!(
        "[profiles.prod]\nurl = \"{url}\"\ntoken = \"{SAVED_TOKEN}\"\n"
    ));
    let key = home.file("key", "flt_keyfilevalue\n");
    let key = key.to_str().unwrap();
    for profile in ["prod", "trial"] {
        let cases: [(&str, Vec<&str>); 5] = [
            ("--url", vec!["--url", &url]),
            ("--token", vec!["--token", "flt_flagvalue"]),
            ("--insecure", vec!["--insecure"]),
            ("--token-env", vec!["--token-env", "SOME_KEY"]),
            ("--token-file", vec!["--token-file", key]),
        ];
        for (flag, extra) in cases {
            let mut args = vec!["doctor", "-p", profile];
            args.extend(extra);
            let output = home.trawl(&args, &[]);
            let case = format!("-p {profile} {flag}");
            assert_usage_error(
                &output,
                flag,
                &["flt_flagvalue", "flt_keyfilevalue", SAVED_TOKEN],
                &case,
            );
        }
    }
    recorder.assert_untouched("profile overrides");
}

/// `--url` never reads config.toml: a `[server].token` saved there is not
/// selected, so the key is "none selected" and nothing carries it.
///
/// No check contacts the server yet, so the recorder asserts the absence of
/// any `Authorization` header vacuously today; it becomes the end-to-end
/// proof once the API checks send requests.
#[test]
fn doctor_url_ignores_saved_token() {
    let home = Sandbox::new();
    let recorder = Recorder::start();
    let url = recorder.url("https");
    home.default_config(&format!(
        "[server]\nurl = \"{url}\"\ntoken = \"{SAVED_TOKEN}\"\ninsecure = true\n"
    ));

    let output = home.trawl(&["doctor", "--url", &url, "--format", "json"], &[]);
    assert!(!text(&output).contains(SAVED_TOKEN), "{}", text(&output));
    let report = report(&output);
    let check = connection_config(&report);
    assert_eq!(check["outcome"], "complete", "{}", text(&output));
    assert_eq!(check["source"], "--url flag");
    assert_eq!(
        check["detail"], "trust: system roots; key: none selected",
        "neither the saved token nor the saved insecure is used"
    );
    assert_eq!(report["target"]["origin"], url);
    assert_eq!(report["vantage"], "client");

    let seen = recorder.seen.lock().unwrap();
    assert_eq!(seen.authorization_headers, 0, "the saved token was sent");
}

/// Case name, extra flags, extra environment, expected reason suffix.
type KeyCase<'a> = (&'a str, Vec<&'a str>, Vec<(&'a str, &'a str)>, &'a str);

/// `--token-env` and `--token-file` select the key; a missing or empty
/// source fails `connection.config` naming the flag, not the source's name
/// or path.
#[test]
fn doctor_url_key_sources() {
    let home = Sandbox::new();
    let recorder = Recorder::start();
    let url = recorder.url("https");
    let good = home.file("good.key", "flt_keyfilevalue\n");
    let empty = home.file("empty.key", "\n");
    let absent = home.home.join("absent.key");

    let run = |extra: &[&str], env: &[(&str, &str)]| {
        let mut args = vec!["doctor", "--url", url.as_str(), "--format", "json"];
        args.extend_from_slice(extra);
        home.trawl(&args, env)
    };

    let output = run(&["--token-file", good.to_str().unwrap()], &[]);
    let check = connection_config(&report(&output)).clone();
    assert_eq!(check["outcome"], "complete");
    assert!(
        check["detail"]
            .as_str()
            .unwrap()
            .ends_with("key: key from --token-file")
    );
    assert!(!text(&output).contains("flt_keyfilevalue"));

    let output = run(
        &["--token-env", "DOCTOR_KEY"],
        &[("DOCTOR_KEY", "flt_envvalue")],
    );
    let check = connection_config(&report(&output)).clone();
    assert_eq!(check["outcome"], "complete");
    assert!(
        check["detail"]
            .as_str()
            .unwrap()
            .ends_with("key: key from --token-env")
    );
    assert!(!text(&output).contains("flt_envvalue"));

    let failures: [KeyCase<'_>; 4] = [
        (
            "unset variable",
            vec!["--token-env", "DOCTOR_KEY_ABSENT"],
            vec![],
            "is not set",
        ),
        (
            "empty variable",
            vec!["--token-env", "DOCTOR_KEY"],
            vec![("DOCTOR_KEY", "  ")],
            "is empty",
        ),
        (
            "missing file",
            vec!["--token-file", absent.to_str().unwrap()],
            vec![],
            "does not exist",
        ),
        (
            "empty file",
            vec!["--token-file", empty.to_str().unwrap()],
            vec![],
            "is empty",
        ),
    ];
    for (case, extra, env, fragment) in failures {
        let output = run(&extra, &env);
        assert_eq!(output.status.code(), Some(1), "{case}: {}", text(&output));
        let report = report(&output);
        let check = connection_config(&report);
        assert_eq!(check["outcome"], "failed", "{case}");
        assert!(
            check["reason"].as_str().unwrap().ends_with(fragment),
            "{case}: {check}"
        );
        assert!(
            check["source"]
                .as_str()
                .unwrap()
                .starts_with("key from --token-")
        );
        let all = text(&output);
        assert!(
            !all.contains("DOCTOR_KEY"),
            "{case}: the name is not echoed"
        );
        assert!(
            !all.contains(&*home.home.to_string_lossy()),
            "{case}: no path"
        );
        assert_eq!(report["verdict"], "fail");
    }
    recorder.assert_untouched("key sources");
}

/// A missing config file, a missing profile, or a profile with no url fails
/// `connection.config` naming what is missing. `[server]` points at the
/// recorder, so a fallback to it would show as a connection.
#[test]
fn doctor_missing_profile_fails_without_contact() {
    let home = Sandbox::new();
    let recorder = Recorder::start();
    let url = recorder.url("https");

    // No config file at all.
    let output = home.trawl(&["doctor", "-p", "prod", "--format", "json"], &[]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    let report_json = report(&output);
    let check = connection_config(&report_json);
    assert_eq!(check["outcome"], "failed");
    assert_eq!(check["reason"], "the config file does not exist");
    assert_eq!(check["source"], "config file ~/.config/trawl/config.toml");
    assert!(report_json["target"]["origin"].is_null());
    assert_eq!(
        report_json["target"]["source"],
        "CLI profile `prod` in ~/.config/trawl/config.toml"
    );

    // A named config file that does not exist.
    let absent = home.home.join("absent.toml");
    let output = home.trawl(
        &[
            "doctor",
            "-p",
            "prod",
            "-c",
            absent.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[],
    );
    let check = connection_config(&report(&output)).clone();
    assert_eq!(check["reason"], "the config file does not exist");
    assert_eq!(check["source"], format!("config file {}", absent.display()));

    home.default_config(&format!(
        "[server]\nurl = \"{url}\"\ntoken = \"{SAVED_TOKEN}\"\n\n\
         [profiles.lab]\nurl = \"{url}\"\n\n[profiles.nourl]\ninsecure = false\n"
    ));
    let cases = [
        ("prod", "the config file has no [profiles.prod]"),
        ("nourl", "the profile sets no url"),
    ];
    for (profile, reason) in cases {
        let output = home.trawl(&["doctor", "-p", profile, "--format", "json"], &[]);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{profile}: {}",
            text(&output)
        );
        let report = report(&output);
        let check = connection_config(&report);
        assert_eq!(check["outcome"], "failed", "{profile}");
        assert_eq!(check["reason"], reason, "{profile}");
        assert!(report["target"]["origin"].is_null());
        assert!(!text(&output).contains(SAVED_TOKEN));
    }
    let output = home.trawl(&["doctor", "-p", "prod", "--format", "table"], &[]);
    let table = text(&output);
    assert!(table.contains("pick one of: lab, nourl"), "{table}");

    // A profile without a token of its own selects no key: [server].token
    // is never inherited.
    let output = home.trawl(&["doctor", "-p", "lab", "--format", "json"], &[]);
    let check = connection_config(&report(&output)).clone();
    assert_eq!(check["outcome"], "complete", "{}", text(&output));
    assert!(
        check["detail"]
            .as_str()
            .unwrap()
            .ends_with("key: none selected")
    );

    recorder.assert_untouched("missing profile");
}

/// `-p trial` resolves through the trial's reserved profile rules: the
/// trial's recorded port, its operator key, and its pinned certificate.
#[test]
fn doctor_trial_profile() {
    const TRIAL_TOKEN: &str = "flt_trialoperatortoken";
    let home = Sandbox::new();
    let recorder = Recorder::start();
    let trial_dir = write_trial(&home.state_home, recorder.port, TRIAL_TOKEN);
    home.default_config(&format!(
        "[server]\nurl = \"https://elsewhere.example:5514\"\ntoken = \"{SAVED_TOKEN}\"\ninsecure = true\n"
    ));

    let output = home.trawl(&["doctor", "-p", "trial", "--format", "json"], &[]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let report_json = report(&output);
    let check = connection_config(&report_json);
    assert_eq!(check["outcome"], "complete");
    assert_eq!(
        check["detail"],
        "trust: pinned CA from the trial's certificate; key: the trial's operator key"
    );
    assert_eq!(report_json["target"]["source"], "trial profile (-p trial)");
    assert_eq!(
        report_json["target"]["origin"],
        format!("https://127.0.0.1:{}", recorder.port)
    );
    let all = text(&output);
    assert!(
        !all.contains(TRIAL_TOKEN) && !all.contains(SAVED_TOKEN),
        "{all}"
    );

    // A user profile named `trial` is refused through the trial's own rule.
    let own = home.file("own.toml", "[profiles.trial]\nurl = \"https://x:1\"\n");
    let output = home.trawl(
        &[
            "doctor",
            "-p",
            "trial",
            "-c",
            own.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[],
    );
    let check = connection_config(&report(&output)).clone();
    assert_eq!(check["outcome"], "failed");
    assert!(
        check["reason"]
            .as_str()
            .unwrap()
            .contains("[profiles.trial]")
    );

    // No trial: connection.config fails and names the fix.
    std::fs::remove_dir_all(&trial_dir).unwrap();
    let output = home.trawl(&["doctor", "-p", "trial", "--format", "json"], &[]);
    assert_eq!(output.status.code(), Some(1));
    let check = connection_config(&report(&output)).clone();
    assert_eq!(check["reason"], "there is no trial on this machine");
    assert_eq!(check["next_action"], "start one with `trawl trial up`");

    recorder.assert_untouched("trial profile");
}

/// A finished trial directory under `state_home`, in the shape
/// `tests/trial_profile.rs` builds. Returns the trial directory.
fn write_trial(state_home: &Path, api_port: u16, token: &str) -> PathBuf {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    let dir = state_home.join("trawl/trial");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .unwrap();
    let state = serde_json::json!({
        "schema": 1,
        "trial_id": "0123456789abcdef0123456789abcdef",
        "project": "trawl-trial",
        "cli_version": "0.9.0",
        "created_at": "2026-09-25T12:00:00Z",
        "engine_id": "ENGINE:ID",
        "ports": {"api": api_port, "web": 18090},
        "images": {
            "trawl": {"reference": "ghcr.io/jakub/trawl:0.9.0", "id": "sha256:aa", "repo_digest": null},
            "postgres": {"reference": "postgres:18", "id": "sha256:bb", "repo_digest": null},
            "trawl_overridden": false
        },
        "phases": {"database": true, "fleet_migrated": true, "tls": true, "services_verified": true},
        "tls": {"sha256_fingerprint": "AB:CD"},
        "keys": {
            "operator": {"name": "trial-operator", "prefix": "pfx00001"},
            "ingest": {"name": "trial-ingest", "prefix": "pfx00002"}
        },
        "samples": {"state": "skipped"}
    });
    let write = |name: &str, bytes: &[u8], mode: u32| {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    write("state.json", state.to_string().as_bytes(), 0o600);
    write("operator.token", format!("{token}\n").as_bytes(), 0o600);
    let pem = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()])
        .expect("self-signed")
        .cert
        .pem();
    write("ca.pem", pem.as_bytes(), 0o644);
    dir
}

/// Other commands keep exiting as before: success is 0, any error is 1.
#[test]
fn other_commands_keep_their_exit_status() {
    let home = Sandbox::new();
    let ok = home.trawl(&["validate", "* | head 1"], &[]);
    assert_eq!(ok.status.code(), Some(0), "{}", text(&ok));
    let bad = home.trawl(&["validate", "* | nosuchcommand"], &[]);
    assert_eq!(bad.status.code(), Some(1), "{}", text(&bad));
    let refused = home.trawl(&["-p", "trial", "driver", "status"], &[]);
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused));
}
