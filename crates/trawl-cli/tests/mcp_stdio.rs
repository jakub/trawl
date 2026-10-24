// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl mcp` on the real binary (ADR-0058): it refuses every ambient or
//! command-line target that is not the named profile, and its stdout
//! carries protocol messages and nothing else.
//!
//! Every run starts from an empty environment (`env_clear`) with a temp
//! `HOME`, so the default config path is one the test wrote. Tests cannot
//! set the environment in-process (`unsafe_code = forbid`), so every case
//! is a subprocess. A session is driven over the child's pipes with
//! hand-written JSON-RPC lines; no trawld runs, so the profile points at a
//! loopback port and a tool that needs the server answers with a tool
//! error.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// A key value that must never appear in any output.
const SENTINEL: &str = "flt_s3ntinelsecretvalue";

/// The key the profile holds.
const PROFILE_TOKEN: &str = "flt_profiletokenvalue";

/// The variables `trawl mcp` refuses.
const REFUSED: [&str; 4] = [
    "TRAWL_URL",
    "TRAWL_TOKEN",
    "TRAWL_PROFILE",
    "TRAWL_INSECURE",
];

/// How long one session may take before the test gives up on it.
const SESSION_DEADLINE: Duration = Duration::from_secs(60);

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
    fn default_config(&self, toml: &str) {
        let dir = self.home.join(".config/trawl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), toml).unwrap();
    }

    fn file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.home.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_trawl"));
        cmd.env_clear()
            .env("HOME", &self.home)
            .env("XDG_STATE_HOME", &self.state_home)
            .args(args)
            .envs(env.iter().copied());
        cmd
    }

    /// Run to completion with stdin closed.
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.command(args, env)
            .stdin(Stdio::null())
            .output()
            .expect("spawn trawl")
    }
}

/// A port with nothing listening on it.
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

/// A config with `[profiles.home]` at `url`, with or without a token, and
/// a `[server]` token that must never be used.
fn profile_config(url: &str, token: bool) -> String {
    let token = if token {
        format!("token = \"{PROFILE_TOKEN}\"\n")
    } else {
        String::new()
    };
    format!(
        "[server]\nurl = \"{url}\"\ntoken = \"{SENTINEL}\"\n\n[profiles.home]\nurl = \"{url}\"\n{token}"
    )
}

fn text(output: &Output) -> String {
    format!(
        "status {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A refusal: exit 2, nothing on stdout, one line on stderr that holds
/// every `needle` and no sentinel.
fn assert_refused(output: &Output, needles: &[&str], case: &str) {
    let all = text(output);
    assert_eq!(output.status.code(), Some(2), "{case}: {all}");
    assert!(
        output.stdout.is_empty(),
        "{case}: stdout must stay empty\n{all}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!all.contains(SENTINEL), "{case}: a value leaked\n{all}");
    assert!(
        !all.contains(PROFILE_TOKEN),
        "{case}: the key leaked\n{all}"
    );
    assert_eq!(stderr.lines().count(), 1, "{case}: one stderr line\n{all}");
    for needle in needles {
        assert!(
            stderr.contains(needle),
            "{case}: expected {needle:?}\n{all}"
        );
    }
}

#[test]
fn ambient_variables_are_refused_by_name() {
    let home = Sandbox::new();
    home.default_config(&profile_config(
        &format!("http://127.0.0.1:{}", closed_port()),
        true,
    ));
    for name in REFUSED {
        for value in [SENTINEL, ""] {
            let case = format!("{name}={value:?}");
            for args in [
                vec!["mcp", "--profile", "home"],
                // A flag that shadows the variable does not hide it.
                vec!["mcp", "--profile", "home", "--insecure"],
                vec!["-p", "home", "mcp"],
            ] {
                let output = home.run(&args, &[(name, value)]);
                assert_refused(
                    &output,
                    &[&format!("{name} is set")],
                    &format!("{case} {args:?}"),
                );
            }
        }
    }
    let all: Vec<(&str, &str)> = REFUSED.iter().map(|&name| (name, SENTINEL)).collect();
    let output = home.run(&["mcp", "--profile", "home"], &all);
    assert_refused(&output, &REFUSED, "all four");
}

#[test]
fn flags_that_move_the_target_are_refused() {
    let home = Sandbox::new();
    let url = format!("http://127.0.0.1:{}", closed_port());
    home.default_config(&profile_config(&url, true));
    for (flag, args) in [
        (
            "--url",
            vec!["mcp", "--profile", "home", "--url", url.as_str()],
        ),
        (
            "--token",
            vec!["mcp", "--profile", "home", "--token", SENTINEL],
        ),
        ("--insecure", vec!["mcp", "--profile", "home", "--insecure"]),
        (
            "--token",
            vec!["--token", SENTINEL, "mcp", "--profile", "home"],
        ),
    ] {
        let output = home.run(&args, &[]);
        assert_refused(&output, &[&format!("{flag} cannot be used")], flag);
    }
    let output = home.run(&["mcp"], &[]);
    assert_refused(&output, &["needs a profile"], "no profile");
}

#[test]
fn the_key_comes_from_exactly_one_source() {
    let home = Sandbox::new();
    let url = format!("http://127.0.0.1:{}", closed_port());
    let key_file = home.file("key", &format!("{SENTINEL}\n"));
    let key_file = key_file.to_str().unwrap();

    home.default_config(&profile_config(&url, true));
    let output = home.run(&["mcp", "--profile", "home", "--token-file", key_file], &[]);
    assert_refused(
        &output,
        &["two key sources", "--token-file"],
        "token and file",
    );

    // `[server].token` is set and is never a fallback.
    home.default_config(&profile_config(&url, false));
    let output = home.run(&["mcp", "--profile", "home"], &[]);
    assert_refused(
        &output,
        &["no key", "[profiles.home]", "--token-file"],
        "no key",
    );

    let missing = home.home.join("no-such-key");
    let output = home.run(
        &[
            "mcp",
            "--profile",
            "home",
            "--token-file",
            missing.to_str().unwrap(),
        ],
        &[],
    );
    assert_refused(
        &output,
        &["--token-file", "does not exist"],
        "missing key file",
    );

    let output = home.run(&["mcp", "--profile", "elsewhere"], &[]);
    assert_refused(&output, &["[profiles.elsewhere]"], "unknown profile");
}

/// One session's output.
struct Session {
    status: Option<i32>,
    /// Every stdout line, each with its trailing newline removed.
    lines: Vec<String>,
    /// Bytes after the last newline: a partial message.
    trailing: Vec<u8>,
    stderr: String,
}

/// Send one JSON-RPC message as a line.
fn send(stdin: &mut ChildStdin, message: &Value) {
    let mut line = serde_json::to_vec(message).unwrap();
    line.push(b'\n');
    stdin.write_all(&line).expect("write to trawl mcp");
    stdin.flush().unwrap();
}

/// Read stdout lines on a thread, so the test can wait with a deadline.
fn stdout_lines(child: &mut Child) -> mpsc::Receiver<Result<String, Vec<u8>>> {
    let (tx, rx) = mpsc::channel();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    std::thread::spawn(move || {
        loop {
            let mut line = Vec::new();
            match stdout.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    let sent = match line.strip_suffix(b"\n") {
                        Some(body) => tx.send(Ok(String::from_utf8_lossy(body).into_owned())),
                        None => tx.send(Err(line)),
                    };
                    if sent.is_err() {
                        return;
                    }
                }
            }
        }
    });
    rx
}

/// Run one session: `initialize` (2025-11-25), `notifications/initialized`,
/// `tools/list`, a `tools/call` of `orient`, then close stdin and read
/// stdout to EOF.
fn session(home: &Sandbox, args: &[&str], env: &[(&str, &str)]) -> Session {
    let mut child = home
        .command(args, env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn trawl mcp");
    let mut stdin = child.stdin.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stderr = std::thread::spawn(move || {
        let mut text = String::new();
        stderr.read_to_string(&mut text).ok();
        text
    });
    let rx = stdout_lines(&mut child);
    let deadline = Instant::now() + SESSION_DEADLINE;
    let mut lines = Vec::new();
    let mut trailing = Vec::new();
    // Read until the response with `id` arrives.
    let wait_for = |id: u64, lines: &mut Vec<String>, trailing: &mut Vec<u8>| loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Ok(line)) => {
                let answered = serde_json::from_str::<Value>(&line)
                    .is_ok_and(|message| message["id"] == json!(id));
                lines.push(line);
                if answered {
                    return;
                }
            }
            Ok(Err(partial)) => {
                trailing.extend(partial);
                return;
            }
            Err(e) => panic!("no response to id {id}: {e}; lines so far: {lines:?}"),
        }
    };

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "mcp_stdio", "version": "0"},
            },
        }),
    );
    wait_for(1, &mut lines, &mut trailing);
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    wait_for(2, &mut lines, &mut trailing);
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "orient", "arguments": {}},
        }),
    );
    wait_for(3, &mut lines, &mut trailing);
    drop(stdin);

    // stdout to EOF.
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Ok(line)) => lines.push(line),
            Ok(Err(partial)) => trailing.extend(partial),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                child.kill().ok();
                panic!("stdout did not reach EOF after stdin closed: {lines:?}");
            }
        }
    }
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status.code();
        }
        if Instant::now() > deadline {
            child.kill().ok();
            panic!("trawl mcp did not exit after stdin closed");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Session {
        status,
        lines,
        trailing,
        stderr: stderr.join().unwrap(),
    }
}

/// Every stdout line is one JSON-RPC 2.0 message and nothing follows the
/// last newline; the three requests were answered, and the process exited
/// 0 after EOF.
fn assert_pure(session: &Session, case: &str) {
    let context = format!(
        "{case}\nstatus {:?}\nstdout: {:#?}\ntrailing: {:?}\nstderr: {}",
        session.status,
        session.lines,
        String::from_utf8_lossy(&session.trailing),
        session.stderr
    );
    assert!(session.trailing.is_empty(), "partial message: {context}");
    let messages: Vec<Value> = session
        .lines
        .iter()
        .map(|line| {
            let message: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("not JSON ({e}): {line:?}\n{context}"));
            assert_eq!(message["jsonrpc"], "2.0", "not JSON-RPC: {line}\n{context}");
            assert!(
                message.get("id").is_some() || message.get("method").is_some(),
                "neither a response nor a request: {line}\n{context}"
            );
            message
        })
        .collect();
    let response = |id: u64| {
        messages
            .iter()
            .find(|m| m["id"] == json!(id))
            .unwrap_or_else(|| panic!("no response to {id}: {context}"))
    };
    assert_eq!(
        response(1)["result"]["serverInfo"]["name"],
        "trawl",
        "{context}"
    );
    let tools: Vec<&str> = response(2)["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tools: {context}"))
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["orient", "search", "read", "fields"], "{context}");
    let orient = &response(3)["result"];
    assert_eq!(orient["isError"], true, "{context}");
    assert!(
        orient["content"][0]["text"].is_string(),
        "orient refusal is text: {context}"
    );
    assert_eq!(session.status, Some(0), "clean exit after EOF: {context}");
    assert!(!context.contains(SENTINEL), "a value leaked: {context}");
    assert!(
        !context.contains(PROFILE_TOKEN),
        "the key leaked: {context}"
    );
}

#[test]
fn stdout_carries_protocol_messages_only() {
    let home = Sandbox::new();
    home.default_config(&profile_config(
        &format!("http://127.0.0.1:{}", closed_port()),
        true,
    ));
    let quiet = session(&home, &["mcp", "--profile", "home"], &[]);
    assert_pure(&quiet, "default log level");
    assert!(quiet.stderr.is_empty(), "nothing to log: {}", quiet.stderr);

    // Debug logging goes to stderr, never stdout.
    let debug = session(&home, &["-p", "home", "mcp"], &[("RUST_LOG", "debug")]);
    assert_pure(&debug, "RUST_LOG=debug");
    assert!(
        debug.stderr.contains("DEBUG"),
        "debug lines reach stderr: {}",
        debug.stderr
    );
}

#[test]
fn a_key_file_serves_a_profile_without_a_token() {
    let home = Sandbox::new();
    home.default_config(&profile_config(
        &format!("http://127.0.0.1:{}", closed_port()),
        false,
    ));
    let key_file = home.file("key", &format!("{PROFILE_TOKEN}\n"));
    let run = session(
        &home,
        &[
            "mcp",
            "--profile",
            "home",
            "--token-file",
            key_file.to_str().unwrap(),
        ],
        &[],
    );
    assert_pure(&run, "--token-file");
}

#[test]
fn startup_contacts_no_server() {
    let home = Sandbox::new();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    home.default_config(&profile_config(&format!("http://127.0.0.1:{port}"), true));
    let mut child = home
        .command(&["mcp", "--profile", "home"], &[])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn trawl mcp");
    let mut stdin = child.stdin.take().unwrap();
    let rx = stdout_lines(&mut child);
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "mcp_stdio", "version": "0"},
            },
        }),
    );
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let mut answered = 0;
    while answered < 2 {
        let line = rx
            .recv_timeout(SESSION_DEADLINE)
            .expect("a response")
            .expect("a whole line");
        if serde_json::from_str::<Value>(&line).is_ok_and(|m| m.get("id").is_some()) {
            answered += 1;
        }
    }
    drop(stdin);
    let status = child.wait().expect("wait");
    assert_eq!(status.code(), Some(0));
    // Any connection the run made is queued on the listener by now.
    listener.set_nonblocking(true).unwrap();
    match listener.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("trawl mcp contacted the server at startup: {other:?}"),
    }
}

/// A stdout the client stopped reading after the session started is a
/// transport failure: exit 1, one `trawl:` line on stderr naming the
/// direction and the error's kind, and no bytes of the stream. Rust
/// ignores SIGPIPE, so the write fails with EPIPE instead of killing the
/// process.
#[test]
fn a_broken_stdout_after_initialize_exits_one() {
    let home = Sandbox::new();
    home.default_config(&profile_config(
        &format!("http://127.0.0.1:{}", closed_port()),
        true,
    ));
    let mut child = home
        .command(&["mcp", "--profile", "home"], &[])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn trawl mcp");
    let mut stdin = child.stdin.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stderr = std::thread::spawn(move || {
        let mut text = String::new();
        stderr.read_to_string(&mut text).ok();
        text
    });
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "mcp_stdio", "version": "0"},
            },
        }),
    );
    let mut line = Vec::new();
    stdout
        .read_until(b'\n', &mut line)
        .expect("the initialize response");
    let response: Value = serde_json::from_slice(&line).expect("a JSON-RPC line");
    assert_eq!(response["id"], 1, "{response}");
    // Close the only read end of the child's stdout.
    drop(stdout);

    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    drop(stdin);

    let deadline = Instant::now() + SESSION_DEADLINE;
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status.code();
        }
        if Instant::now() > deadline {
            child.kill().ok();
            panic!("trawl mcp did not exit after its stdout broke");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stderr = stderr.join().unwrap();
    assert_eq!(status, Some(1), "stderr: {stderr}");
    // rmcp logs the failed write itself, at ERROR, through the stderr
    // tracing writer; the command's own report is one line, and the last.
    let reports: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("trawl: "))
        .collect();
    assert_eq!(
        reports,
        ["trawl: the MCP transport failed: writing stdout failed: broken pipe"],
        "{stderr}"
    );
    assert_eq!(stderr.lines().last(), Some(reports[0]), "{stderr}");
    for leaked in ["tools", "jsonrpc", "serverInfo", PROFILE_TOKEN, SENTINEL] {
        assert!(!stderr.contains(leaked), "{leaked} leaked: {stderr}");
    }
}

/// `trawl mcp --help` names the variables and never prints their values.
#[test]
fn help_never_prints_environment_values() {
    let home = Sandbox::new();
    let env: Vec<(&str, &str)> = REFUSED.iter().map(|&name| (name, SENTINEL)).collect();
    for args in [vec!["mcp", "--help"], vec!["help", "mcp"]] {
        let output = home.run(&args, &env);
        let all = text(&output);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {all}");
        assert!(!all.contains(SENTINEL), "{args:?} printed a value: {all}");
        assert!(all.contains("--token-file"), "{args:?}: {all}");
    }
}

/// A tracing line as `trawl mcp` writes it without ANSI colour: an RFC 3339
/// timestamp, then the level.
fn is_tracing_line(line: &str) -> bool {
    let mut words = line.split_whitespace();
    let stamped = words.next().is_some_and(|stamp| {
        stamp.starts_with(|c: char| c.is_ascii_digit()) && stamp.contains('T')
    });
    stamped
        && words
            .next()
            .is_some_and(|level| ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"].contains(&level))
}

/// Whether `c` ends a line for a reader that splits lines the Unicode way,
/// as Python's `str.splitlines` does.
fn is_unicode_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{1c}'..='\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

/// A JSON-RPC id is client-controlled, and rmcp logs it raw at the default
/// `warn` level when it refuses a `tools/call`. stderr still carries one
/// line per log event and no terminal control sequence: no forged line, no
/// ESC, and no Unicode line or paragraph separator, so a reader that
/// splits lines the Unicode way reads the same lines as one that splits on
/// `\n`.
#[test]
fn a_hostile_request_id_cannot_forge_stderr_lines() {
    const HOSTILE_ID: &str =
        "id\nforged-line\u{1b}[2J\u{2028}forged-separator\u{2029}forged-paragraph";
    let home = Sandbox::new();
    home.default_config(&profile_config(
        &format!("http://127.0.0.1:{}", closed_port()),
        true,
    ));
    // RUST_LOG stays unset: the default level is the one under test.
    let mut child = home
        .command(&["mcp", "--profile", "home"], &[])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn trawl mcp");
    let mut stdin = child.stdin.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stderr = std::thread::spawn(move || {
        let mut text = String::new();
        stderr.read_to_string(&mut text).ok();
        text
    });
    let rx = stdout_lines(&mut child);
    let deadline = Instant::now() + SESSION_DEADLINE;
    let response_to = |id: &Value| loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = rx
            .recv_timeout(left)
            .unwrap_or_else(|e| panic!("no response to {id}: {e}"))
            .expect("a whole line");
        let message: Value = serde_json::from_str(&line).expect("a JSON-RPC line");
        if message["id"] == *id {
            return message;
        }
    };

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "mcp_stdio", "version": "0"},
            },
        }),
    );
    response_to(&json!(1));
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": HOSTILE_ID,
            "method": "tools/call",
            "params": {"name": "unknown", "arguments": {}},
        }),
    );
    let refused = response_to(&json!(HOSTILE_ID));
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    drop(stdin);

    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status.code();
        }
        if Instant::now() > deadline {
            child.kill().ok();
            panic!("trawl mcp did not exit after stdin closed");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stderr = stderr.join().unwrap();
    assert_eq!(status, Some(0), "stderr: {stderr:?}");
    // The whole id reached the log, so the case is not vacuous.
    assert!(stderr.contains("forged-line"), "stderr: {stderr:?}");
    assert!(stderr.contains("forged-paragraph"), "stderr: {stderr:?}");
    assert!(!stderr.contains('\u{1b}'), "ESC on stderr: {stderr:?}");
    for separator in ['\u{2028}', '\u{2029}'] {
        assert!(
            !stderr.contains(separator),
            "{separator:?} on stderr: {stderr:?}"
        );
    }
    let lf_lines: Vec<&str> = stderr.split('\n').filter(|l| !l.is_empty()).collect();
    let unicode_lines: Vec<&str> = stderr
        .split(is_unicode_line_break)
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(unicode_lines, lf_lines, "stderr: {stderr:?}");
    for line in stderr.split('\n') {
        assert!(!line.starts_with("forged"), "a forged line: {stderr:?}");
        assert!(
            line.is_empty() || line.starts_with("trawl: ") || is_tracing_line(line),
            "not a log line: {line:?}\nstderr: {stderr:?}"
        );
    }
}
