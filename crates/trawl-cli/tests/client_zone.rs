// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The display zone is the client machine's, in every client (ADR-0051,
//! Amendment 2026-11-08): trawld answers in UTC only, and `trawl query`
//! (daemon and embedded), the TUI and `trawl mcp` read `ui.timezone` once
//! at start and present each instant with its own offset.
//!
//! Every client is the real `trawl` binary, started from an empty
//! environment (only the dynamic loader's search path survives) with a
//! temp `HOME` and the `TZ` the case names. Tests cannot set the
//! environment in-process (`unsafe_code = forbid`), so a zone is always a
//! child's `TZ`. The TUI runs under a PTY, because crossterm needs a real
//! terminal, and is driven through its driver socket. Its screen is read
//! with the driver's `capture`, which draws the grid the way the terminal
//! does; `get-results` returns the UTC frames the grid converts from.

mod common;

use std::io::Read as _;
use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite};
use tokio::io::{AsyncWriteExt as _, BufReader};

use common::Server;
use common::mcp::loader_path;

/// How long one client run, one driver reply or one poll may take.
const DEADLINE: Duration = Duration::from_secs(60);

/// The service every case ingests under, and the query that finds it.
const SERVICE: &str = "zone_probe";
const QUERY: &str = "service=zone_probe | table _time, message";

/// The client zones the cases run under.
const KOLKATA: &str = "Asia/Kolkata";
const BERLIN: &str = "Europe/Berlin";

/// The one event of the cross-client case, and how each surface spells it.
const EVENT_AT: &str = "2026-11-01T06:00:00Z";
const EVENT_UTC_WIRE: &str = "2026-11-01T06:00:00.000000Z";
const EVENT_KOLKATA_WIRE: &str = "2026-11-01T11:30:00.000000+05:30";
const EVENT_KOLKATA_HUMAN: &str = "2026-11-01 11:30:00 +05:30";

/// The child the cross-client case re-runs under `TZ=UTC`.
const UTC_CHILD: &str = "daemon_at_utc_child";

// ---------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------

/// A temp `HOME` the clients run under, holding the configs they read.
struct Home {
    dir: tempfile::TempDir,
    configs: AtomicUsize,
}

impl Home {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
            configs: AtomicUsize::new(0),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Write a config whose `[server]` table is `server` and whose
    /// `ui.timezone` is `zone`.
    fn config(&self, server: &str, zone: &str) -> PathBuf {
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
    fn command(&self, tz: &str) -> tokio::process::Command {
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
    async fn query(&self, tz: &str, config: &Path, args: &[&str]) -> Output {
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
    async fn query_ok(&self, tz: &str, config: &Path, args: &[&str]) -> String {
        let output = self.query(tz, config, args).await;
        assert!(
            output.status.success(),
            "trawl query {args:?} under TZ={tz} failed\n{}",
            describe(&output)
        );
        String::from_utf8(output.stdout).expect("utf-8 stdout")
    }
}

/// What the clients may do: batch queries, the live tail, Parquet export
/// and the schema reads the TUI starts with.
const READER_PERMISSIONS: [&str; 4] = ["query", "stream", "export", "schema_read"];

/// Mint the clients' key on `server`.
async fn reader(server: &Server) -> String {
    server.mint("zone-reader", &READER_PERMISSIONS).await
}

/// `[server]` for a client that reaches `server` directly as `token`,
/// pinning its certificate.
fn pinned(server: &Server, token: &str) -> String {
    format!(
        "url = {:?}\ntoken = {token:?}\nca_cert = {:?}\n",
        server.url,
        server.cert_path.to_str().expect("utf-8 cert path")
    )
}

/// `[server]` for a client that reaches trawld through `proxy` as
/// `token`, in plain HTTP.
fn proxied(proxy: &Proxy, token: &str) -> String {
    format!(
        "url = {:?}\ntoken = {token:?}\ninsecure = true\n",
        proxy.url
    )
}

fn describe(output: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn event(at: &str, message: &str) -> Value {
    json!({ "_time": at, "service": SERVICE, "message": message })
}

/// The parquet files a compaction wrote, as embedded `--data` reads them.
fn data_glob(server: &Server) -> String {
    format!("{}/**/*.parquet", server.data.display())
}

/// Each NDJSON row's `_time` read as an instant, beside its `message`.
fn rows(ndjson: &str) -> Vec<(DateTime<Utc>, String)> {
    let mut rows: Vec<_> = ndjson
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let row: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("not a JSON row ({e}): {line}"));
            let at = row["_time"].as_str().expect("a _time string");
            let at = DateTime::parse_from_rfc3339(at)
                .unwrap_or_else(|e| panic!("_time {at:?} is not RFC 3339 ({e})"))
                .with_timezone(&Utc);
            (at, row["message"].as_str().expect("a message").to_owned())
        })
        .collect();
    rows.sort();
    rows
}

/// The one row of an NDJSON answer, by field.
fn only_row(ndjson: &str) -> Value {
    let lines: Vec<&str> = ndjson.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "one row expected:\n{ndjson}");
    serde_json::from_str(lines[0]).expect("a JSON row")
}

// ---------------------------------------------------------------------------
// A recording proxy
// ---------------------------------------------------------------------------

/// A plain-HTTP front for a trawld that records every byte each way. It
/// copies bytes, so the SSE stream of a live tail passes through as it
/// arrives. The clients send no `Accept-Encoding`, so both logs are text.
struct Proxy {
    url: String,
    /// What the clients sent: request lines, headers and bodies.
    sent: Arc<Mutex<Vec<u8>>>,
    /// What trawld answered.
    received: Arc<Mutex<Vec<u8>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Proxy {
    async fn start(server: &Server) -> Self {
        use rustls_pki_types::pem::PemObject as _;
        let upstream = server
            .url
            .strip_prefix("https://")
            .expect("an https url")
            .to_owned();
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_pki_types::CertificateDer::pem_file_iter(&server.cert_path)
            .expect("read the server certificate")
        {
            roots
                .add(cert.expect("a PEM certificate"))
                .expect("trust it");
        }
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(tls));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the proxy");
        let url = format!("http://{}", listener.local_addr().expect("proxy addr"));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn({
            let (sent, received) = (sent.clone(), received.clone());
            async move {
                while let Ok((client, _)) = listener.accept().await {
                    let (connector, upstream) = (connector.clone(), upstream.clone());
                    let (sent, received) = (sent.clone(), received.clone());
                    tokio::spawn(async move {
                        let tcp = tokio::net::TcpStream::connect(&upstream)
                            .await
                            .expect("reach trawld");
                        let name = rustls_pki_types::ServerName::try_from("localhost")
                            .expect("a server name");
                        let tls = connector.connect(name, tcp).await.expect("TLS to trawld");
                        let (from_client, to_client) = client.into_split();
                        let (from_server, to_server) = tokio::io::split(tls);
                        tokio::join!(
                            relay(from_client, to_server, sent),
                            relay(from_server, to_client, received),
                        );
                    });
                }
            }
        });
        Self {
            url,
            sent,
            received,
            task,
        }
    }

    fn sent(&self) -> String {
        String::from_utf8_lossy(&self.sent.lock().unwrap()).into_owned()
    }

    fn received(&self) -> String {
        String::from_utf8_lossy(&self.received.lock().unwrap()).into_owned()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Copy `from` into `to`, recording each chunk, until either side ends.
async fn relay(
    mut from: impl AsyncRead + Unpin,
    mut to: impl AsyncWrite + Unpin,
    log: Arc<Mutex<Vec<u8>>>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    while let Ok(n @ 1..) = from.read(&mut buf).await {
        log.lock().unwrap().extend_from_slice(&buf[..n]);
        if to.write_all(&buf[..n]).await.is_err() || to.flush().await.is_err() {
            break;
        }
    }
    let _ = to.shutdown().await;
}

// ---------------------------------------------------------------------------
// The TUI under a PTY
// ---------------------------------------------------------------------------

/// A bare `trawl` on a PTY. The master is drained by a thread into
/// `screen`, or the TUI would block on a full terminal.
struct Tui {
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
    fn spawn(home: &Home, tz: &str, config: &Path, socket: Option<&Path>) -> Self {
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
    fn screen(&self) -> String {
        String::from_utf8_lossy(&self.screen.lock().unwrap()).into_owned()
    }

    /// Start under a fresh driver socket and connect to it once the TUI
    /// listens.
    async fn start(home: &Home, tz: &str, config: &Path) -> Self {
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
    async fn send(&mut self, request: Value) -> Value {
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
    async fn batch(&mut self, query: &str) {
        self.send(json!({ "cmd": "set_query", "query": query }))
            .await;
        self.send(json!({ "cmd": "execute", "timeout_ms": DEADLINE.as_millis() }))
            .await;
    }

    /// The grid as the terminal draws it.
    async fn capture(&mut self) -> String {
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
    async fn table_with(&mut self, needle: &str) -> String {
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
    async fn wait(mut self) -> (std::process::ExitStatus, String) {
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
    async fn quit(mut self) {
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
fn line_with<'a>(screen: &'a str, needle: &str) -> Option<&'a str> {
    screen.lines().find(|line| line.contains(needle))
}

// ---------------------------------------------------------------------------
// A listener that counts connections
// ---------------------------------------------------------------------------

/// A TCP port that accepts and drops every connection, counting them.
struct Counting {
    port: u16,
    accepted: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Counting {
    fn start() -> Self {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("addr").port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let (accepted, stop) = (accepted.clone(), stop.clone());
            move || {
                loop {
                    let done = stop.load(Ordering::SeqCst);
                    // A connection made just before the stop is still queued.
                    while let Ok((stream, _)) = listener.accept() {
                        accepted.fetch_add(1, Ordering::SeqCst);
                        drop(stream);
                    }
                    if done {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        });
        Self {
            port,
            accepted,
            stop,
            thread: Some(thread),
        }
    }

    /// Stop accepting; returns how many connections arrived.
    fn finish(mut self) -> usize {
        self.stop.store(true, Ordering::SeqCst);
        self.thread
            .take()
            .expect("the thread")
            .join()
            .expect("join");
        self.accepted.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

/// The daemon's zone is not the reader's: with trawld at `TZ=UTC` and every
/// client at `TZ=Asia/Kolkata` under `ui.timezone = "local"`, the daemon
/// query, the embedded query, the TUI's batch and its live tail all print
/// the event at `+05:30`. The fixture boots trawld in-process, so the case
/// re-runs itself as a child whose whole process is at `TZ=UTC`.
#[test]
fn client_local_zone_is_independent_of_daemon_zone() {
    let mut command = std::process::Command::new(std::env::current_exe().expect("this binary"));
    command
        .args([UTC_CHILD, "--exact", "--ignored", "--test-threads=1"])
        .env_clear()
        .envs(loader_path())
        .env("TZ", "UTC");
    if let Some(url) = std::env::var_os("DATABASE_URL") {
        command.env("DATABASE_URL", url);
    }
    let output = command.output().expect("run the TZ=UTC child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "the TZ=UTC child did not pass\n{}",
        describe(&output)
    );
}

/// The body of [`client_local_zone_is_independent_of_daemon_zone`], in a
/// process at `TZ=UTC`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run by client_local_zone_is_independent_of_daemon_zone under TZ=UTC"]
async fn daemon_at_utc_child() {
    assert_eq!(
        std::env::var("TZ").as_deref(),
        Ok("UTC"),
        "this child must run with the daemon at TZ=UTC"
    );
    let server = Server::start(true).await;
    let proxy = Proxy::start(&server).await;
    server.ingest(&[event(EVENT_AT, "batch event")]).await;
    server.compact().await;

    let home = Home::new();
    let token = reader(&server).await;
    let local = home.config(&proxied(&proxy, &token), "local");
    let utc = home.config(&proxied(&proxy, &token), "UTC");

    let daemon = home
        .query_ok(KOLKATA, &local, &[QUERY, "--format", "table"])
        .await;
    assert!(
        daemon.contains(EVENT_KOLKATA_HUMAN),
        "daemon trawl query:\n{daemon}"
    );

    let glob = data_glob(&server);
    let embedded = home
        .query_ok(
            KOLKATA,
            &local,
            &[QUERY, "--data", &glob, "--format", "table"],
        )
        .await;
    assert!(
        embedded.contains(EVENT_KOLKATA_HUMAN),
        "embedded trawl query:\n{embedded}"
    );
    // The embedded executor answers in UTC: with no conversion asked, the
    // client in Kolkata prints the UTC wire text.
    let embedded_utc = home
        .query_ok(KOLKATA, &utc, &[QUERY, "--data", &glob, "--format", "json"])
        .await;
    assert_eq!(
        only_row(&embedded_utc)["_time"],
        EVENT_UTC_WIRE,
        "{embedded_utc}"
    );

    let mut tui = Tui::start(&home, KOLKATA, &local).await;
    tui.batch(QUERY).await;
    let batch = tui.table_with("batch event").await;
    let row = line_with(&batch, "batch event").expect("a batch row");
    assert!(row.contains(EVENT_KOLKATA_HUMAN), "TUI batch:\n{batch}");

    // Live: events arrive after the stream opens, at the same instant.
    // The tail flushes a snapshot every ten events, and the stream may
    // not be subscribed yet when a round lands, so rounds repeat until the
    // tab holds one.
    tui.send(json!({ "cmd": "key", "key": "f9" })).await;
    let status = tui.send(json!({ "cmd": "status" })).await;
    assert_eq!(status["live_mode"], true, "F9 started no tail: {status}");
    let started = Instant::now();
    let mut round = 0;
    'rounds: loop {
        let events: Vec<Value> = (0..10)
            .map(|i| event(EVENT_AT, &format!("live event {round}-{i}")))
            .collect();
        server.ingest(&events).await;
        round += 1;
        let polled = Instant::now();
        while polled.elapsed() < Duration::from_secs(2) {
            let results = tui.send(json!({ "cmd": "get_results" })).await;
            if results["rows"].to_string().contains("live event") {
                break 'rounds;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            started.elapsed() < DEADLINE,
            "the live tail showed no event within {DEADLINE:?}"
        );
    }
    let live = tui.table_with("live event").await;
    let row = line_with(&live, "live event").expect("a live row");
    assert!(row.contains(EVENT_KOLKATA_HUMAN), "TUI live:\n{live}");
    tui.quit().await;

    // The wire: no request named a zone, and trawld answered in UTC.
    let sent = proxy.sent();
    assert!(
        sent.contains("POST /api/v1/query") && sent.contains("GET /api/v1/stream"),
        "the proxy saw no batch query or stream:\n{sent}"
    );
    assert!(
        !sent.contains("timezone"),
        "a request carried a timezone:\n{sent}"
    );
    let received = proxy.received();
    assert!(
        received.contains(EVENT_UTC_WIRE),
        "trawld's answers held no UTC instant:\n{received}"
    );
    assert!(
        !received.contains("+05:30"),
        "trawld answered in the client's zone:\n{received}"
    );
    drop(proxy);
    server.stop().await;
}

/// Two instants that read 02:30 on Berlin's fall-back night keep their own
/// offsets, in the CLI table and on the TUI screen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_display_preserves_dst_identity() {
    const SUMMER: &str = "2026-10-25 02:30:00 +02:00";
    const WINTER: &str = "2026-10-25 02:30:00 +01:00";
    let server = Server::start(true).await;
    server
        .ingest(&[
            event("2026-10-25T00:30:00Z", "before the change"),
            event("2026-10-25T01:30:00Z", "after the change"),
        ])
        .await;
    server.compact().await;
    let home = Home::new();
    let token = reader(&server).await;
    let local = home.config(&pinned(&server, &token), "local");

    let table = home
        .query_ok(BERLIN, &local, &[QUERY, "--format", "table"])
        .await;
    for (message, reading) in [("before the change", SUMMER), ("after the change", WINTER)] {
        let row =
            line_with(&table, message).unwrap_or_else(|| panic!("no {message:?} row:\n{table}"));
        assert!(row.contains(reading), "CLI table, {message}:\n{table}");
    }

    let mut tui = Tui::start(&home, BERLIN, &local).await;
    tui.batch(QUERY).await;
    let screen = tui.table_with("before the change").await;
    for (message, reading) in [("before the change", SUMMER), ("after the change", WINTER)] {
        let row = line_with(&screen, message)
            .unwrap_or_else(|| panic!("the TUI drew no {message:?} row:\n{screen}"));
        assert!(row.contains(reading), "TUI, {message}:\n{screen}");
    }
    tui.quit().await;
    server.stop().await;
}

/// A zone the grammar refuses stops every client before it sends anything
/// or prints a result: no fallback to UTC.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_display_zone_never_falls_back() {
    const ZONE: &str = "Mars/Olympus";
    let listener = Counting::start();
    let url = format!("http://127.0.0.1:{}", listener.port);
    let home = Home::new();
    // The default config path, so `trawl mcp` reads it too.
    let dir = home.path().join(".config/trawl");
    std::fs::create_dir_all(&dir).expect("config dir");
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nurl = {url:?}\ntoken = \"flt_zonecase\"\n\n\
             [profiles.agent]\nurl = {url:?}\ntoken = \"flt_zonecase\"\n\n\
             [ui]\ntimezone = {ZONE:?}\n"
        ),
    )
    .expect("write the config");

    let query = home
        .query(KOLKATA, &config, &[QUERY, "--format", "json"])
        .await;
    let stderr = String::from_utf8_lossy(&query.stderr);
    assert_eq!(query.status.code(), Some(1), "{}", describe(&query));
    assert!(stderr.contains(ZONE), "{}", describe(&query));
    assert!(query.stdout.is_empty(), "{}", describe(&query));

    // Under a PTY, so a TUI that took the terminal first would show it.
    let (status, screen) = Tui::spawn(&home, KOLKATA, &config, None).wait().await;
    assert!(!status.success(), "the TUI ran:\n{screen}");
    assert!(
        screen.contains(ZONE),
        "the TUI did not name the zone:\n{screen}"
    );
    assert!(
        !screen.contains("\x1b[?1049h"),
        "the TUI entered the alternate screen:\n{screen:?}"
    );
    assert!(
        !dir.join("tui.log").exists(),
        "the TUI opened its log before refusing the zone"
    );

    let mut mcp = home
        .command(KOLKATA)
        .args(["mcp", "--profile", "agent"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn trawl mcp");
    let mut stdin = mcp.stdin.take().expect("stdin");
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "client_zone", "version": "0" }
        }
    });
    // A refusing server may already be gone: a broken pipe is fine.
    let _ = stdin.write_all(format!("{initialize}\n").as_bytes()).await;
    // stdin stays open: a server that served would answer and wait.
    let output = tokio::time::timeout(DEADLINE, mcp.wait_with_output())
        .await
        .expect("trawl mcp served instead of refusing")
        .expect("wait for trawl mcp");
    drop(stdin);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(
        stderr.contains("ui.timezone in") && stderr.contains(ZONE),
        "{}",
        describe(&output)
    );
    assert!(
        output.stdout.is_empty(),
        "trawl mcp answered:\n{}",
        describe(&output)
    );

    assert_eq!(listener.finish(), 0, "a client connected to the server");
}

/// The zone is configuration, read before the connection: with no
/// credential anywhere, the TUI's refusal names the invalid zone, as
/// `trawl query`'s does, and leaves no `tui.log`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_display_zone_precedes_the_credential_refusal() {
    const ZONE: &str = "Mars/Olympus";
    let home = Home::new();
    let config = home.config("", ZONE);

    let query = home
        .query(KOLKATA, &config, &[QUERY, "--format", "json"])
        .await;
    assert_eq!(query.status.code(), Some(1), "{}", describe(&query));
    assert!(
        String::from_utf8_lossy(&query.stderr).contains(ZONE),
        "trawl query did not name the zone\n{}",
        describe(&query)
    );

    let (status, screen) = Tui::spawn(&home, KOLKATA, &config, None).wait().await;
    assert!(!status.success(), "the TUI ran:\n{screen}");
    assert!(
        screen.contains(ZONE),
        "the TUI did not name the zone:\n{screen}"
    );
    assert!(
        !home.path().join(".config/trawl/tui.log").exists(),
        "the TUI opened its log before refusing the zone"
    );
}

/// json and csv carry the wire spelling in the display zone; Parquet,
/// which never reads the zone, and the HTTP export stay UTC.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_timestamp_output_formats() {
    let server = Server::start(true).await;
    server.ingest(&[event(EVENT_AT, "format probe")]).await;
    server.compact().await;
    let home = Home::new();
    let token = reader(&server).await;
    let fixed = home.config(&pinned(&server, &token), "+05:30");
    let utc = home.config(&pinned(&server, &token), "UTC");
    let glob = data_glob(&server);

    for (mode, data) in [("daemon", None), ("embedded", Some(glob.as_str()))] {
        let with = |format: &'static str| {
            let mut args = vec![QUERY, "--format", format];
            if let Some(data) = data {
                args.extend(["--data", data]);
            }
            args
        };
        let json = home.query_ok(KOLKATA, &fixed, &with("json")).await;
        assert_eq!(
            only_row(&json)["_time"],
            EVENT_KOLKATA_WIRE,
            "{mode} json:\n{json}"
        );
        let csv = home.query_ok(KOLKATA, &fixed, &with("csv")).await;
        assert!(csv.contains(EVENT_KOLKATA_WIRE), "{mode} csv:\n{csv}");
        assert!(!csv.contains(EVENT_UTC_WIRE), "{mode} csv:\n{csv}");

        // A zone the grammar refuses: Parquet never reads it.
        let refused = home.config(&pinned(&server, &token), "Mars/Olympus");
        let out = home.path().join(format!("{mode}.parquet"));
        let out_arg = out.to_str().expect("utf-8 path");
        let mut args = with("parquet");
        args.extend(["-o", out_arg]);
        home.query_ok(KOLKATA, &refused, &args).await;
        // Read back with no conversion asked: the stored instant, in UTC.
        let back = home
            .query_ok(
                KOLKATA,
                &utc,
                &[
                    "* | table _time, message",
                    "--data",
                    out_arg,
                    "--format",
                    "json",
                ],
            )
            .await;
        let row = only_row(&back);
        assert_eq!(row["_time"], EVENT_UTC_WIRE, "{mode} parquet:\n{back}");
        assert_eq!(row["message"], "format probe", "{mode} parquet:\n{back}");
    }

    let client = server.client(&token);
    for format in [trawl_api::ExportFormat::Json, trawl_api::ExportFormat::Csv] {
        let name = format.to_string();
        let bytes = client
            .export(QUERY, format, None)
            .await
            .expect("the export answers");
        let text = String::from_utf8(bytes).expect("utf-8 export");
        assert!(text.contains(EVENT_UTC_WIRE), "{name} export:\n{text}");
        assert!(!text.contains("+05:30"), "{name} export:\n{text}");
    }
    server.stop().await;
}

/// The display zone is presentation only: a Rust-tail filter on `_time`
/// keeps the same rows under every `ui.timezone`, daemon and embedded.
///
/// One RFC 3339 bound serves both modes, because both tails read `_time`
/// as the stored instant. The daemon's tail reads it under trawld's `_time`
/// TIMESTAMP pin. Embedded `--data` has no catalog (ADR-0011), and its tail
/// reads `_time` under the TIMESTAMP type the SQL step returned. A bound
/// or a value read in +05:30 would move the cut by 5h30 and keep `k=b`
/// (05:59:59Z reads 11:29:59 there).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn display_zone_does_not_change_evaluation() {
    const TAIL: &str = "service=zone_probe | extract kv \
        | where _time > \"2026-11-01T06:00:00Z\" | table _time, message";
    let server = Server::start(true).await;
    server
        .ingest(&[
            event("2026-11-01T01:00:00Z", "k=a"),
            event("2026-11-01T05:59:59Z", "k=b"),
            event("2026-11-01T06:30:00Z", "k=c"),
            event("2026-11-01T12:00:00Z", "k=d"),
        ])
        .await;
    server.compact().await;
    let home = Home::new();
    let token = reader(&server).await;
    let glob = data_glob(&server);
    let expected = vec![
        ("2026-11-01T06:30:00Z".parse().unwrap(), "k=c".to_owned()),
        ("2026-11-01T12:00:00Z".parse().unwrap(), "k=d".to_owned()),
    ];
    for zone in ["UTC", "+05:30", "local"] {
        let config = home.config(&pinned(&server, &token), zone);
        let daemon = home
            .query_ok(KOLKATA, &config, &[TAIL, "--format", "json"])
            .await;
        assert_eq!(
            rows(&daemon),
            expected,
            "daemon, ui.timezone={zone}:\n{daemon}"
        );
        let embedded = home
            .query_ok(
                KOLKATA,
                &config,
                &[TAIL, "--data", &glob, "--format", "json"],
            )
            .await;
        assert_eq!(
            rows(&embedded),
            expected,
            "embedded, ui.timezone={zone}:\n{embedded}"
        );
    }
    server.stop().await;
}

/// Embedded `--data` answers a kv-tail filter on `_time` with the rows the
/// same query keeps without `extract kv`, in csv and in json.
///
/// The SQL step returns `_time` as a TIMESTAMP, so the tail compares it as
/// an instant against the RFC 3339 bound, as the all-SQL lane does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn embedded_kv_tail_matches_the_sql_lane() {
    const TAIL: &str = "service=zone_probe | extract kv \
        | where _time > \"2026-11-01T06:00:00Z\" | table _time, message";
    const SQL: &str = "service=zone_probe \
        | where _time > \"2026-11-01T06:00:00Z\" | table _time, message";
    let server = Server::start(true).await;
    server
        .ingest(&[
            event("2026-11-01T01:00:00Z", "k=a"),
            event("2026-11-01T05:59:59Z", "k=b"),
            event("2026-11-01T06:30:00Z", "k=c"),
            event("2026-11-01T12:00:00Z", "k=d"),
        ])
        .await;
    server.compact().await;
    let home = Home::new();
    let token = reader(&server).await;
    let glob = data_glob(&server);
    let config = home.config(&pinned(&server, &token), "UTC");
    let embedded = |query: &'static str, format: &'static str| {
        let (home, config, glob) = (&home, &config, &glob);
        async move {
            home.query_ok(
                KOLKATA,
                config,
                &[query, "--data", glob, "--format", format],
            )
            .await
        }
    };

    let tail = embedded(TAIL, "json").await;
    let sql = embedded(SQL, "json").await;
    let expected = vec![
        ("2026-11-01T06:30:00Z".parse().unwrap(), "k=c".to_owned()),
        ("2026-11-01T12:00:00Z".parse().unwrap(), "k=d".to_owned()),
    ];
    assert_eq!(
        rows(&sql),
        expected,
        "embedded json without the tail:\n{sql}"
    );
    assert_eq!(
        rows(&tail),
        expected,
        "embedded json with the tail:\n{tail}"
    );

    // csv: the same header, and the same data lines in any order.
    let lines = |csv: &str| {
        let mut lines = csv.lines().filter(|line| !line.trim().is_empty());
        let header = lines.next().map(str::to_owned);
        let mut data: Vec<String> = lines.map(str::to_owned).collect();
        data.sort();
        (header, data)
    };
    let tail = embedded(TAIL, "csv").await;
    let sql = embedded(SQL, "csv").await;
    let (tail_header, tail_rows) = lines(&tail);
    let (sql_header, sql_rows) = lines(&sql);
    assert_eq!(tail_header, sql_header, "csv headers:\n{tail}\n{sql}");
    assert_eq!(tail_rows, sql_rows, "csv rows:\n{tail}\n{sql}");
    assert_eq!(tail_rows.len(), 2, "embedded csv with the tail:\n{tail}");
    for (line, message) in tail_rows.iter().zip(["k=c", "k=d"]) {
        assert!(line.contains(message), "{message} in csv line {line:?}");
    }
    server.stop().await;
}
