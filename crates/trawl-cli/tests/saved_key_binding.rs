// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Where a saved or exported key may go, observed on the real binary
//! (ADR-0065).
//!
//! Every run starts from an empty environment (keeping only the dynamic
//! loader's search path, see `loader_path`), with a temp `HOME` and
//! `XDG_STATE_HOME`, plus only the variables the case sets. Tests cannot
//! set the environment in-process (`unsafe_code = forbid`), so every case
//! is a subprocess, killed if it outlives its deadline.
//!
//! A plain-HTTP [`Recorder`] on `127.0.0.1` stands at every URL a run could
//! reach. It counts each connection when it accepts it, before reading a
//! byte, and records each request's target and its `Authorization` value,
//! so a case asserts which key arrived, not just whether one did. Every
//! case uses its own canary key per source, so a key that arrives names
//! the source it came from.

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The key saved in `[server]`.
const T: &str = "flt_savedservertoken";
/// The key exported in `TRAWL_TOKEN`.
const K: &str = "flt_exportedenvtoken";
/// The key typed with `--token`.
const K2: &str = "flt_typedflagtoken";
/// The key in the file `--token-file` names.
const FK: &str = "flt_keyfiletoken";
/// A key saved in a profile's own table.
const PK: &str = "flt_profileowntoken";

/// The words every binding refusal carries.
const BINDING: &str = "different API base";

/// One request a [`Recorder`] read.
#[derive(Debug, Clone)]
struct Request {
    /// The request target as sent: a path, or an absolute URL through a
    /// proxy.
    target: String,
    /// The `Authorization` header's value, if any.
    authorization: Option<String>,
}

/// What a [`Recorder`] saw.
#[derive(Debug, Default)]
struct Seen {
    connections: usize,
    requests: Vec<Request>,
    /// Barrier connections fully handled; see [`Recorder::settle`].
    barriers: usize,
}

/// The whole request a barrier connection sends.
const BARRIER: &[u8] = b"TRAWL-RECORDER-BARRIER\r\n\r\n";
/// The whole request that ends the accept loop.
const STOP: &[u8] = b"TRAWL-RECORDER-STOP\r\n\r\n";

/// How a [`Recorder`] answers a request.
#[derive(Debug, Clone)]
enum Answer {
    /// `401`, so a run that reaches the recorder ends at once.
    Unauthorized,
    /// `302` to this URL, which a client that follows redirects would
    /// take its key to.
    Redirect(String),
}

impl Answer {
    fn response(&self) -> String {
        match self {
            Self::Unauthorized => {
                "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    .to_owned()
            }
            Self::Redirect(to) => format!(
                "HTTP/1.1 302 Found\r\nlocation: {to}/api/v1/query\r\ncontent-length: 0\r\n\
                 connection: close\r\n\r\n"
            ),
        }
    }
}

/// A plain TCP listener on `127.0.0.1` that records and answers.
///
/// One thread accepts and handles connections one at a time, in the order
/// the kernel queued them, so once it has handled a connection it has
/// counted every connection queued before that one.
struct Recorder {
    port: u16,
    seen: Arc<Mutex<Seen>>,
    barriers_sent: Mutex<usize>,
    accept: Option<JoinHandle<()>>,
}

impl Recorder {
    fn start() -> Self {
        Self::answering(Answer::Unauthorized)
    }

    fn answering(answer: Answer) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let record = Arc::clone(&seen);
        let accept = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                record.lock().unwrap().connections += 1;
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                let (head, mut body) = read_head(&mut stream);
                if head == BARRIER {
                    record.lock().unwrap().barriers += 1;
                    continue;
                }
                if head == STOP {
                    return;
                }
                let text = String::from_utf8_lossy(&head).into_owned();
                let target = text
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or_default()
                    .to_owned();
                let header = |name: &str| {
                    text.lines().skip(1).find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.trim()
                            .eq_ignore_ascii_case(name)
                            .then(|| value.trim().to_owned())
                    })
                };
                let authorization = header("authorization");
                record.lock().unwrap().requests.push(Request {
                    target,
                    authorization,
                });
                // Read the body the client sent before answering, so the
                // close is clean.
                let length: usize = header("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
                    .min(1 << 20);
                let mut buf = [0u8; 4096];
                while body < length {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => body += n,
                    }
                }
                stream.write_all(answer.response().as_bytes()).ok();
            }
        });
        Self {
            port,
            seen,
            barriers_sent: Mutex::new(0),
            accept: Some(accept),
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Send a barrier and wait until it is handled: every connection made
    /// before it has then been counted. Call it after the runs exit.
    fn settle(&self) -> MutexGuard<'_, Seen> {
        let mut sent = self.barriers_sent.lock().unwrap();
        *sent += 1;
        let mut barrier = TcpStream::connect(("127.0.0.1", self.port)).expect("barrier connect");
        barrier.write_all(BARRIER).expect("barrier write");
        drop(barrier);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let seen = self.seen.lock().unwrap();
            if seen.barriers >= *sent {
                return seen;
            }
            assert!(Instant::now() < deadline, "the barrier was not handled");
            drop(seen);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Nothing but barriers reached the recorder.
    fn assert_untouched(&self, case: &str) {
        let seen = self.settle();
        assert_eq!(
            seen.connections, seen.barriers,
            "{case}: nothing but a barrier may connect; saw {:?}",
            seen.requests
        );
        assert!(seen.requests.is_empty(), "{case}: {:?}", seen.requests);
    }

    /// Every request so far, after the runs exit.
    fn requests(&self) -> Vec<Request> {
        self.settle().requests.clone()
    }

    /// Requests arrived, and every one carried `Bearer {key}`.
    fn assert_only_key(&self, key: &str, case: &str) -> Vec<Request> {
        let requests = self.requests();
        assert!(!requests.is_empty(), "{case}: no request arrived");
        for request in &requests {
            assert_eq!(
                request.authorization.as_deref(),
                Some(format!("Bearer {key}").as_str()),
                "{case}: {request:?}"
            );
        }
        requests
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if let Ok(mut stop) = TcpStream::connect(("127.0.0.1", self.port)) {
            stop.write_all(STOP).ok();
        }
        if let Some(accept) = self.accept.take() {
            accept.join().ok();
        }
    }
}

/// Read up to the end of the request head. Returns the head, and how many
/// body bytes came with it.
fn read_head(stream: &mut TcpStream) -> (Vec<u8>, usize) {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let body = bytes.len() - (end + 4);
            bytes.truncate(end + 4);
            return (bytes, body);
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return (bytes, 0),
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
        }
    }
}

/// A temp `HOME` and `XDG_STATE_HOME` for one test.
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
    fn config(&self, toml: &str) {
        let dir = self.home.join(".config/trawl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), toml).unwrap();
    }

    fn file(&self, name: &str, contents: &[u8]) -> PathBuf {
        let path = self.home.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_trawl"));
        cmd.env_clear()
            .envs(loader_path())
            .env("HOME", &self.home)
            .env("XDG_STATE_HOME", &self.state_home)
            .envs(env.iter().copied())
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    /// Run `trawl` with stdin closed.
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let child = self
            .command(args, env)
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn trawl");
        finish(child, args)
    }

    /// Run `trawl` with a stdin that stays open until it exits, so a run
    /// that reads stdin first hangs until its deadline.
    fn run_with_open_stdin(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut child = self
            .command(args, env)
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn trawl");
        let stdin = child.stdin.take();
        let output = finish(child, args);
        drop(stdin);
        output
    }
}

/// Wait for `child` to exit, killing it at the deadline.
fn finish(mut child: Child, args: &[&str]) -> Output {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if child.try_wait().expect("wait").is_some() {
            return child.wait_with_output().expect("output");
        }
        if Instant::now() > deadline {
            child.kill().ok();
            child.wait().ok();
            panic!("trawl {args:?} did not exit before its deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `[server]` holding `url` and `token = T`.
fn server(url: &str) -> String {
    format!("[server]\nurl = \"{url}\"\ntoken = \"{T}\"\n")
}

/// A case: its name, its arguments, its environment, and the source the
/// refusal must name as the target.
type Case<'a> = (&'a str, Vec<&'a str>, Vec<(&'a str, &'a str)>, &'a str);

/// The run failed with a binding refusal that names `credential` and
/// `target` and both remedies, and none of `secrets`.
fn assert_binding_refusal(
    output: &Output,
    credential: &str,
    target: &str,
    secrets: &[&str],
    case: &str,
) {
    let err = stderr(output);
    assert_eq!(output.status.code(), Some(1), "{case}: {err}");
    for needed in [BINDING, credential, target, "--token ", "--token-file"] {
        assert!(err.contains(needed), "{case}: no {needed:?} in {err}");
    }
    assert_secret_free(&err, secrets, case);
}

fn assert_secret_free(err: &str, secrets: &[&str], case: &str) {
    // The message names the leak by position, never by value.
    for (index, secret) in secrets
        .iter()
        .chain(&[T, K, K2, FK, PK, "127.0.0.1"])
        .enumerate()
    {
        assert!(
            !err.contains(secret),
            "{case}: forbidden string #{index} appears in stderr"
        );
    }
}

/// The recorder sees a keyed request, and which key it carried: the
/// control that gives every "untouched" below its meaning.
#[test]
fn recorder_sees_a_keyed_request() {
    let home = Sandbox::new();
    let a = Recorder::start();
    let output = home.run(&["--url", &a.url(), "--token", K2, "query", "*"], &[]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let requests = a.assert_only_key(K2, "control");
    assert!(
        requests[0].target.starts_with("/api/v1/query"),
        "{requests:?}"
    );
    // The barrier counts this run's connection, then its own.
    let seen = a.settle();
    assert_eq!(seen.connections, seen.barriers + requests.len());
}

/// AC 1: `[server] url = A, token = T`, no key flag. A command pointed at
/// B by `--url`, `TRAWL_URL`, or a profile's `url` fails before it sends
/// anything, the TUI included, and so does a redirect from A to B.
#[test]
fn saved_key_never_reaches_another_base() {
    let home = Sandbox::new();
    let a = Recorder::start();
    let b = Recorder::start();
    let (a_url, b_url) = (a.url(), b.url());
    home.config(&format!(
        "{}\n[profiles.lab]\nurl = \"{b_url}\"\n\n[profiles.dev]\ninsecure = true\n",
        server(&a_url)
    ));
    let secrets = [
        a_url.as_str(),
        b_url.as_str(),
        &a.port.to_string(),
        &b.port.to_string(),
    ];

    let cases: [Case<'_>; 7] = [
        (
            "--url B",
            vec!["--url", &b_url, "query", "*"],
            vec![],
            "--url",
        ),
        (
            "--url after the subcommand",
            vec!["query", "*", "--url", &b_url],
            vec![],
            "--url",
        ),
        (
            "TRAWL_URL=B",
            vec!["query", "*"],
            vec![("TRAWL_URL", &b_url)],
            "TRAWL_URL",
        ),
        (
            "schema under --url B",
            vec!["--url", &b_url, "schema", "fields"],
            vec![],
            "--url",
        ),
        (
            "a trust-only profile under --url B",
            vec!["-p", "dev", "--url", &b_url, "query", "*"],
            vec![],
            "--url",
        ),
        (
            "[profiles.lab] url = B",
            vec!["-p", "lab", "query", "*"],
            vec![],
            "url in [profiles.lab]",
        ),
        (
            "the TUI under --url B",
            vec!["--url", &b_url],
            vec![],
            "--url",
        ),
    ];
    for (case, args, env, target) in cases {
        let output = home.run(&args, &env);
        assert_binding_refusal(&output, "token in [server]", target, &secrets, case);
    }
    assert!(
        !home.home.join(".config/trawl/tui.log").exists(),
        "a refused TUI opens no log"
    );
    a.assert_untouched("A");
    b.assert_untouched("B");

    // The client never follows a redirect, so A's key stays at A.
    let redirecting = Recorder::answering(Answer::Redirect(b_url.clone()));
    home.config(&server(&redirecting.url()));
    let output = home.run(&["query", "*"], &[]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    redirecting.assert_only_key(T, "A redirects");
    b.assert_untouched("B after a redirect");
}

/// AC 2: an exported key binds to `TRAWL_URL` or the saved base, a typed
/// key goes where the command points, and a default port is no port.
#[test]
fn env_and_flag_provenance() {
    let home = Sandbox::new();
    let a = Recorder::start();
    let b = Recorder::start();
    let (a_url, b_url) = (a.url(), b.url());
    home.config(&server(&a_url));
    let secrets = [a_url.as_str(), b_url.as_str()];

    // TRAWL_TOKEN is bound to the saved base, so a typed --url B refuses it.
    let output = home.run(&["--url", &b_url, "query", "*"], &[("TRAWL_TOKEN", K)]);
    assert_binding_refusal(
        &output,
        "TRAWL_TOKEN",
        "--url",
        &secrets,
        "TRAWL_TOKEN, --url B",
    );

    // No fallthrough: TRAWL_TOKEN bound to B is refused at A, though the
    // [server] key under it belongs to A.
    let output = home.run(
        &["--url", &a_url, "query", "*"],
        &[("TRAWL_URL", &b_url), ("TRAWL_TOKEN", K)],
    );
    assert_binding_refusal(
        &output,
        "TRAWL_TOKEN",
        "--url",
        &secrets,
        "TRAWL_TOKEN bound to B, --url A",
    );
    a.assert_untouched("A after the refusals");
    b.assert_untouched("B after the refusals");

    // TRAWL_URL and TRAWL_TOKEN together send K to B.
    home.run(
        &["query", "*"],
        &[("TRAWL_URL", &b_url), ("TRAWL_TOKEN", K)],
    );
    b.assert_only_key(K, "TRAWL_URL=B TRAWL_TOKEN=K");

    // A typed --token wins over TRAWL_TOKEN and goes to --url.
    let b2 = Recorder::start();
    home.run(
        &["--url", &b2.url(), "--token", K2, "query", "*"],
        &[("TRAWL_TOKEN", K)],
    );
    b2.assert_only_key(K2, "--token K2 --url B, TRAWL_TOKEN=K");

    // --token-file goes to --url, and TRAWL_TOKEN beside it is no conflict.
    let b3 = Recorder::start();
    let file = home.file("key", format!("{FK}\n").as_bytes());
    home.run(
        &[
            "--token-file",
            file.to_str().unwrap(),
            "--url",
            &b3.url(),
            "query",
            "*",
        ],
        &[("TRAWL_TOKEN", K)],
    );
    b3.assert_only_key(FK, "--token-file F --url B");

    // TRAWL_URL that spells A differently reuses A's saved key.
    home.run(&["query", "*"], &[("TRAWL_URL", &format!("{a_url}/"))]);
    a.assert_only_key(T, "TRAWL_URL=A/");

    // TRAWL_TOKEN alone binds to the saved base and goes there.
    let c = Recorder::start();
    home.config(&format!("[server]\nurl = \"{}\"\n", c.url()));
    home.run(&["query", "*"], &[("TRAWL_TOKEN", K)]);
    c.assert_only_key(K, "TRAWL_TOKEN with no saved key");

    // `--url http://A:80/` is `http://a`: the request reaches the proxy
    // for a.test, with T. Port 80 cannot be bound in a test, so reqwest's
    // HTTP_PROXY stands in for it, and sees the absolute request target.
    let proxy = Recorder::start();
    home.config(&server("http://a.test"));
    let output = home.run(
        &["--url", "HTTP://A.TEST:80/", "query", "*"],
        &[("HTTP_PROXY", &proxy.url())],
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let requests = proxy.assert_only_key(T, "--url http://A.TEST:80/");
    assert!(
        requests[0].target.starts_with("http://a.test/api/v1/query"),
        "{requests:?}"
    );
}

/// AC 5, through the binary: a saved URL with credentials is refused
/// before any key is read, naming its source and never the URL.
#[test]
fn a_url_with_credentials_is_refused_without_echo() {
    let home = Sandbox::new();
    let b = Recorder::start();
    let creds = format!("http://alice:s3cret@127.0.0.1:{}", b.port);
    home.config(&server(&creds));
    let output = home.run(&["query", "*"], &[]);
    let err = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{err}");
    assert!(err.contains("url in [server]"), "{err}");
    assert!(err.contains("credentials"), "{err}");
    assert_secret_free(&err, &["alice", "s3cret", &b.port.to_string()], "userinfo");

    // A typed --url with a query fails before its typed key is sent.
    let query = format!("{}/x?s3cret=1", b.url());
    home.config(&server("https://localhost:5514"));
    let output = home.run(&["--url", &query, "--token", K2, "query", "*"], &[]);
    let err = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{err}");
    assert!(err.contains("--url has a query or fragment"), "{err}");
    assert_secret_free(&err, &["s3cret", &b.port.to_string()], "query");
    b.assert_untouched("B");
}

/// AC 6: `--token` with `--token-file` is a usage error, and a selected
/// key source that is empty or unusable fails without falling back to
/// `TRAWL_TOKEN` or the saved key, both of which would have been sent.
#[test]
fn key_sources_never_fall_through() {
    let home = Sandbox::new();
    let a = Recorder::start();
    let a_url = a.url();
    home.config(&server(&a_url));
    let file = home.file("key", format!("{FK}\n").as_bytes());
    let path = file.to_str().unwrap();

    let output = home.run(&["--token", K2, "--token-file", path, "query", "*"], &[]);
    let err = stderr(&output);
    assert_eq!(output.status.code(), Some(2), "a usage error: {err}");
    assert!(
        err.contains("--token") && err.contains("--token-file"),
        "{err}"
    );
    assert_secret_free(&err, &[path], "both key flags");

    // The conflict is a usage error for every command, including those
    // that never resolve a connection.
    let data = home.home.join("none/*.parquet");
    let data = data.to_str().unwrap();
    for args in [
        vec![
            "--token",
            K2,
            "--token-file",
            path,
            "query",
            "--data",
            data,
            "*",
        ],
        vec![
            "--token",
            K2,
            "--token-file",
            path,
            "schema",
            "fields",
            "--data",
            data,
        ],
        vec!["--token", K2, "--token-file", path, "trial", "status"],
        vec!["trial", "key", "--token", K2, "--token-file", path],
    ] {
        let output = home.run(&args, &[]);
        let err = stderr(&output);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {err}");
        assert_secret_free(&err, &[path], "both key flags, no connection");
    }

    let empty = home.file("empty", b"");
    let blank = home.file("blank", b" \n\t\n");
    let oversized = home.file("oversized", &[b'k'; 5000]);
    let invalid = home.file("invalid", &[b'k', 0xff, 0xfe]);
    let fifo = home.home.join("fifo");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
    let absent = home.home.join("absent");
    let mut bad: Vec<(&str, &Path)> = vec![
        ("empty", &empty),
        ("whitespace", &blank),
        ("oversized", &oversized),
        ("not UTF-8", &invalid),
        ("a FIFO", &fifo),
        ("a directory", &home.home),
        ("missing", &absent),
    ];
    let locked = home.file("locked", format!("{FK}\n").as_bytes());
    if !nix::unistd::geteuid().is_root() {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        bad.push(("unreadable", &locked));
    }
    for (case, path) in bad {
        let path = path.to_str().unwrap();
        let output = home.run(&["--token-file", path, "query", "*"], &[("TRAWL_TOKEN", K)]);
        let err = stderr(&output);
        assert_eq!(output.status.code(), Some(1), "{case}: {err}");
        assert!(err.contains("--token-file"), "{case}: {err}");
        assert_secret_free(&err, &[path, &a_url], case);
    }

    // An empty exported or typed key is selected, and fails.
    for (case, args, env) in [
        (
            "TRAWL_TOKEN=",
            vec!["query", "*"],
            vec![("TRAWL_TOKEN", "")],
        ),
        ("--token ''", vec!["--token", "", "query", "*"], vec![]),
        ("--token ' '", vec!["--token", " ", "query", "*"], vec![]),
    ] {
        let output = home.run(&args, &env);
        let err = stderr(&output);
        assert_eq!(output.status.code(), Some(1), "{case}: {err}");
        assert!(err.contains("empty"), "{case}: {err}");
        assert_secret_free(&err, &[&a_url], case);
    }
    a.assert_untouched("A: neither K nor T may be sent");
}

/// AC 8: `trawl validate` with a binding refusal is an error, not a
/// silent local result.
#[test]
fn validate_refuses_a_binding_mismatch() {
    let home = Sandbox::new();
    let a = Recorder::start();
    let b = Recorder::start();
    home.config(&server(&a.url()));
    let output = home.run(&["--url", &b.url(), "validate", "* | head 1"], &[]);
    assert_binding_refusal(
        &output,
        "token in [server]",
        "--url",
        &[&a.url(), &b.url()],
        "validate",
    );
    a.assert_untouched("A");
    b.assert_untouched("B");
}

/// AC 8: with no key anywhere, `trawl validate` validates locally and
/// contacts nothing.
#[test]
fn validate_without_any_credential_runs_locally() {
    let home = Sandbox::new();
    let b = Recorder::start();
    let output = home.run(&["--url", &b.url(), "validate", "* | head 1"], &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    b.assert_untouched("B");
}

/// `trawl preview-ingest` resolves before it reads its sample, so a
/// refusal never waits on stdin, and it keeps its exit 2.
#[test]
fn preview_ingest_refuses_before_reading_its_sample() {
    let home = Sandbox::new();
    let a = Recorder::start();
    let b = Recorder::start();
    home.config(&server(&a.url()));
    let output = home.run_with_open_stdin(&["--url", &b.url(), "preview-ingest"], &[]);
    let err = stderr(&output);
    assert_eq!(output.status.code(), Some(2), "{err}");
    assert!(err.contains(BINDING), "{err}");
    a.assert_untouched("A");
    b.assert_untouched("B");
}

/// Embedded mode reads no connection, so neither a binding nor a broken
/// saved URL stops it.
#[test]
fn embedded_mode_never_resolves() {
    let home = Sandbox::new();
    let b = Recorder::start();
    home.config(&server("http://alice:s3cret@127.0.0.1:1"));
    let data = home.home.join("none/*.parquet");
    let data = data.to_str().unwrap();
    for args in [
        vec!["--url", &b.url(), "query", "--data", data, "*"],
        vec!["--url", &b.url(), "schema", "fields", "--data", data],
    ] {
        let output = home.run(&args, &[]);
        let err = stderr(&output);
        for connection_error in [BINDING, "url in [server]", "credentials"] {
            assert!(!err.contains(connection_error), "{args:?}: {err}");
        }
    }
    b.assert_untouched("B");
}

/// The dynamic loader's search path, kept when the rest of the environment
/// is cleared. It is not configuration trawl reads: the test runner sets it
/// so a binary built in this target directory finds the libraries it links
/// (libduckdb in CI's archive), and a child that lost it would die before
/// it resolves anything. Every other variable stays cleared.
fn loader_path() -> impl Iterator<Item = (&'static str, std::ffi::OsString)> {
    [
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
    ]
    .into_iter()
    .filter_map(|name| std::env::var_os(name).map(|value| (name, value)))
}
