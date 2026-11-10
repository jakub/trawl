// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The real `trawl` binary as a client of a fixture server: a temp `HOME`
//! holding the configs it reads, and the TUI on a PTY driven through its
//! driver socket.
//!
//! Every client starts from an empty environment (only the dynamic
//! loader's search path survives) with the temp `HOME` and the `TZ` the
//! case names. Tests cannot set the environment in-process
//! (`unsafe_code = forbid`), so a zone is always a child's `TZ`. The TUI
//! runs under a PTY, because crossterm needs a real terminal. Its screen is
//! read with the driver's `capture`, which draws the grid the way the
//! terminal does.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use super::Server;
use super::mcp::loader_path;

/// How long one client run, one driver reply or one poll may take.
pub const DEADLINE: Duration = Duration::from_secs(60);

/// What the clients may do: batch queries, the live tail, Parquet export
/// and the schema reads the TUI starts with.
pub const READER_PERMISSIONS: [&str; 4] = ["query", "stream", "export", "schema_read"];

// ---------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------

/// A temp `HOME` the clients run under, holding the configs they read.
pub struct Home {
    dir: tempfile::TempDir,
    configs: AtomicUsize,
}

impl Home {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
            configs: AtomicUsize::new(0),
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Write a config whose `[server]` table is `server` and whose
    /// `ui.timezone` is `zone`.
    pub fn config(&self, server: &str, zone: &str) -> PathBuf {
        let n = self.configs.fetch_add(1, Ordering::Relaxed);
        let path = self.path().join(format!("config-{n}.toml"));
        std::fs::write(
            &path,
            format!("[server]\n{server}\n[ui]\ntimezone = {zone:?}\n"),
        )
        .expect("write a client config");
        path
    }

    /// A `trawl` command under this `HOME` and `TZ=tz`, nothing else
    /// inherited.
    pub fn command(&self, tz: &str) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_trawl"));
        cmd.env_clear()
            .envs(loader_path())
            .env("HOME", self.path())
            .env("XDG_STATE_HOME", self.path().join(".state"))
            .env("TZ", tz)
            .kill_on_drop(true);
        cmd
    }

    /// `trawl --config config query args...` under `TZ=tz`, to completion.
    pub async fn query(&self, tz: &str, config: &Path, args: &[&str]) -> Output {
        let run = self
            .command(tz)
            .arg("--config")
            .arg(config)
            .arg("query")
            .args(args)
            .stdin(Stdio::null())
            .output();
        tokio::time::timeout(DEADLINE, run)
            .await
            .unwrap_or_else(|_| panic!("trawl query {args:?} did not finish in {DEADLINE:?}"))
            .expect("spawn trawl query")
    }

    /// [`Self::query`] that must succeed; returns its stdout.
    pub async fn query_ok(&self, tz: &str, config: &Path, args: &[&str]) -> String {
        let output = self.query(tz, config, args).await;
        assert!(
            output.status.success(),
            "trawl query {args:?} under TZ={tz} failed\n{}",
            describe(&output)
        );
        String::from_utf8(output.stdout).expect("utf-8 stdout")
    }
}

/// `[server]` for a client that reaches `server` directly as `token`,
/// pinning its certificate.
pub fn pinned(server: &Server, token: &str) -> String {
    format!(
        "url = {:?}\ntoken = {token:?}\nca_cert = {:?}\n",
        server.url,
        server.cert_path.to_str().expect("utf-8 cert path")
    )
}

pub fn describe(output: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// ---------------------------------------------------------------------------
// The TUI under a PTY
// ---------------------------------------------------------------------------

/// A bare `trawl` on a PTY. The master is drained by a thread into
/// `screen`, or the TUI would block on a full terminal.
pub struct Tui {
    child: std::process::Child,
    screen: Arc<Mutex<Vec<u8>>>,
    drain: Option<std::thread::JoinHandle<()>>,
    driver: Option<Driver>,
}

/// One connection to the TUI's driver socket.
struct Driver {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl Tui {
    /// Start `trawl --config config [--driver=socket]` under `TZ=tz`.
    pub fn spawn(home: &Home, tz: &str, config: &Path, socket: Option<&Path>) -> Self {
        let size = nix::pty::Winsize {
            ws_row: 40,
            ws_col: 160,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = nix::pty::openpty(Some(&size), None).expect("open a PTY");
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_trawl"));
        cmd.env_clear()
            .envs(loader_path())
            .env("HOME", home.path())
            .env("XDG_STATE_HOME", home.path().join(".state"))
            .env("TZ", tz)
            .env("TERM", "xterm-256color")
            .arg("--config")
            .arg(config);
        if let Some(socket) = socket {
            cmd.arg(format!("--driver={}", socket.display()));
        }
        cmd.stdin(Stdio::from(pty.slave.try_clone().expect("dup the slave")))
            .stdout(Stdio::from(pty.slave.try_clone().expect("dup the slave")))
            .stderr(Stdio::from(pty.slave));
        let child = cmd.spawn().expect("spawn the TUI");
        // The command holds the slave ends; the master reads EOF only once
        // the child's are the last ones.
        drop(cmd);
        let screen = Arc::new(Mutex::new(Vec::new()));
        let drain = std::thread::spawn({
            let screen = screen.clone();
            let mut master = std::fs::File::from(pty.master);
            move || {
                let mut buf = [0u8; 8192];
                while let Ok(n @ 1..) = master.read(&mut buf) {
                    screen.lock().unwrap().extend_from_slice(&buf[..n]);
                }
            }
        });
        Self {
            child,
            screen,
            drain: Some(drain),
            driver: None,
        }
    }

    /// Everything the TUI wrote to its terminal so far.
    pub fn screen(&self) -> String {
        String::from_utf8_lossy(&self.screen.lock().unwrap()).into_owned()
    }

    /// Start under a fresh driver socket and connect to it once the TUI
    /// listens.
    pub async fn start(home: &Home, tz: &str, config: &Path) -> Self {
        let socket = home
            .path()
            .join(format!("driver-{}.sock", std::process::id()));
        let mut tui = Self::spawn(home, tz, config, Some(&socket));
        let started = Instant::now();
        let stream = loop {
            if let Ok(stream) = tokio::net::UnixStream::connect(&socket).await {
                break stream;
            }
            if let Some(status) = tui.child.try_wait().expect("poll the TUI") {
                panic!(
                    "the TUI exited ({status}) before its driver listened\n{}",
                    tui.screen()
                );
            }
            assert!(
                started.elapsed() < DEADLINE,
                "the TUI's driver did not listen within {DEADLINE:?}\n{}",
                tui.screen()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let (reader, writer) = stream.into_split();
        tui.driver = Some(Driver {
            lines: BufReader::new(reader).lines(),
            writer,
        });
        tui
    }

    /// Send one driver request and return its successful reply.
    pub async fn send(&mut self, request: Value) -> Value {
        let driver = self.driver.as_mut().expect("a driver connection");
        let mut line = request.to_string();
        line.push('\n');
        driver
            .writer
            .write_all(line.as_bytes())
            .await
            .expect("write to the driver");
        let reply = tokio::time::timeout(DEADLINE, driver.lines.next_line())
            .await
            .unwrap_or_else(|_| panic!("no driver reply to {request} within {DEADLINE:?}"))
            .expect("read the driver")
            .expect("the driver closed");
        let reply: Value = serde_json::from_str(&reply).expect("a JSON reply");
        assert_eq!(reply["ok"], true, "driver {request} failed: {reply}");
        reply
    }

    /// Replace the editor with `query` and run it as a batch.
    pub async fn batch(&mut self, query: &str) {
        self.send(json!({ "cmd": "set_query", "query": query }))
            .await;
        self.send(json!({ "cmd": "execute", "timeout_ms": DEADLINE.as_millis() }))
            .await;
    }

    /// The grid as the terminal draws it.
    pub async fn capture(&mut self) -> String {
        let reply = self
            .send(json!({ "cmd": "capture", "width": 160, "height": 40 }))
            .await;
        reply["content"]
            .as_str()
            .expect("capture content")
            .to_owned()
    }

    /// The screen in the table view, once it draws a row holding `needle`.
    /// A result with `_time` opens in the line chart; `v` cycles the
    /// results pane through the sparkline back to the table.
    pub async fn table_with(&mut self, needle: &str) -> String {
        let status = self.send(json!({ "cmd": "status" })).await;
        if status["focus"] != "results" {
            self.send(json!({ "cmd": "key", "key": "tab" })).await;
        }
        for _ in 0..3 {
            let screen = self.capture().await;
            if line_with(&screen, needle).is_some() {
                return screen;
            }
            self.send(json!({ "cmd": "key", "key": "v" })).await;
        }
        let screen = self.capture().await;
        assert!(
            line_with(&screen, needle).is_some(),
            "no view of the results drew {needle:?}:\n{screen}"
        );
        screen
    }

    /// Wait for the TUI to exit, under [`DEADLINE`]; returns its status and
    /// the whole terminal output.
    pub async fn wait(mut self) -> (std::process::ExitStatus, String) {
        let started = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("poll the TUI") {
                break status;
            }
            assert!(
                started.elapsed() < DEADLINE,
                "the TUI did not exit within {DEADLINE:?}\n{}",
                self.screen()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        self.driver = None;
        if let Some(drain) = self.drain.take() {
            drain.join().expect("the PTY drain");
        }
        (status, self.screen())
    }

    /// Ask the TUI to quit and wait for a clean exit.
    pub async fn quit(mut self) {
        self.send(json!({ "cmd": "quit" })).await;
        let (status, screen) = self.wait().await;
        assert!(status.success(), "the TUI exited {status}\n{screen}");
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The capture's line holding `needle`, if the grid draws one.
pub fn line_with<'a>(screen: &'a str, needle: &str) -> Option<&'a str> {
    screen.lines().find(|line| line.contains(needle))
}
