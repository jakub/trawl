// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl mcp` against a real trawld, in both protocol eras (ADR-0058).
//!
//! The server is trawld's own stack, booted in-process by `common` with a
//! seeded corpus: two services in two environments over two hosts,
//! ingested through `/api/v1/ingest` and compacted, so searches have rows,
//! facets, a dominant service, a lagging sender and long cells. The MCP
//! server is the real `trawl mcp --profile agent` child, started from an
//! empty environment (only the dynamic loader's search path survives, see
//! `loader_path`) with a temp `HOME` whose config names the profile:
//! the server's URL, its pinned certificate and the agent key in the
//! profile's own table.
//!
//! Each child is driven over its pipes with hand-written JSON-RPC lines,
//! once per era:
//!
//! - 2025-11-25: `initialize`, `notifications/initialized`, then
//!   requests that carry no per-request metadata;
//! - 2026-07-28: `server/discover`, then requests whose `_meta` carries the
//!   protocol version and client capabilities the revision requires.
//!
//! Both eras list the same tools, in the same order, with the same
//! schemas, and every tool is called against the seeded server. Every
//! result is one text block, and a successful one carries a
//! `structuredContent` that validates against the tool's listed
//! `outputSchema`. Every stdout line is a JSON-RPC message, and the child
//! exits 0 once its stdin closes.
//!
//! A cancelled search drops its trawld request: the test holds the
//! corpus's publication write guard so the capture waits, in flight and
//! listed by `/api/v1/queries`, then sends `notifications/cancelled` and
//! sees the query recorded as abandoned and no response for its id.
//!
//! An expired capture reads as trawld's 410 `result_gone`: the server's
//! results clock is a fixed one the test moves past the capture's
//! deadline, so expiry is deterministic, and neither `read` nor `fields`
//! runs or captures anything to replace it.
//!
//! Without `DATABASE_URL` these tests fail rather than skip. The binary is
//! in the ADR-0021 `postgres` nextest group.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use common::{Options, Server};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;
use trawl_server::results::ResultsClock;

/// The profile the config names.
const PROFILE: &str = "agent";

/// The origin the link test's server builds search URLs under.
const LINK_ORIGIN: &str = "https://trawl.example";

/// How long one response, or the end of a session, may take.
const DEADLINE: Duration = Duration::from_secs(60);

/// The `api` events: newest first, half a second apart, so every one is
/// under [`trawl_mcp::hints::LAG_THRESHOLD_SECS`] old when ingested.
const API_EVENTS: usize = 340;

/// The `worker` events, twenty minutes behind their arrival.
const WORKER_EVENTS: usize = 60;

/// Every this many `api` events, one carries a message long enough to be
/// abbreviated.
const LONG_EVERY: usize = 25;

/// How far behind its arrival a `worker` event's `_time` is.
const WORKER_LAG: TimeDelta = TimeDelta::minutes(20);

/// The tools, in the order `tools/list` must send them.
const TOOLS: [&str; 7] = [
    "orient",
    "search",
    "read",
    "context",
    "sightings",
    "compare",
    "fields",
];

/// The literal `sightings` looks for: every [`LONG_EVERY`]th `api`
/// message carries it, and no other event does.
const SIGHTED: &str = "payload";

/// The two protocol eras a client may speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Era {
    /// 2025-11-25: `initialize`, then a session.
    Initialize,
    /// 2026-07-28: `server/discover`, then self-contained requests.
    Discover,
}

impl Era {
    const ALL: [Self; 2] = [Self::Initialize, Self::Discover];

    fn version(self) -> &'static str {
        match self {
            Self::Initialize => "2025-11-25",
            Self::Discover => "2026-07-28",
        }
    }

    /// The `_meta` every 2026-07-28 request carries.
    fn meta(self) -> Option<Value> {
        (self == Self::Discover).then(|| {
            json!({
                "io.modelcontextprotocol/protocolVersion": self.version(),
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/clientInfo": {"name": "mcp_conformance", "version": "0"},
            })
        })
    }
}

/// One stdout line of the child, or the bytes after its last newline.
type Line = Result<String, Vec<u8>>;

/// A running `trawl mcp` child and everything it wrote.
struct Mcp {
    era: Era,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::UnboundedReceiver<Line>,
    /// Every whole stdout line so far, in order.
    seen: Vec<String>,
    /// Bytes after the last newline: a partial message.
    trailing: Vec<u8>,
    stderr: tokio::task::JoinHandle<String>,
    next_id: u64,
    /// The validator of each tool's `outputSchema`, once listed.
    outputs: BTreeMap<String, jsonschema::Validator>,
}

/// One tool call's result.
struct Call {
    raw: Value,
    text: String,
    structured: Option<Value>,
    is_error: bool,
}

/// What a session left behind once its stdin closed.
struct Finished {
    status: Option<i32>,
    lines: Vec<String>,
    trailing: Vec<u8>,
    stderr: String,
}

/// The dynamic loader's search path, kept when the rest of the environment
/// is cleared. It is not configuration trawl reads: the test runner sets it
/// so a binary built in this target directory finds the libraries it links
/// (libduckdb in CI's archive), and a child that lost it would die before
/// reading its first frame. Every other variable stays cleared.
fn loader_path() -> impl Iterator<Item = (&'static str, std::ffi::OsString)> {
    [
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
    ]
    .into_iter()
    .filter_map(|name| std::env::var_os(name).map(|value| (name, value)))
}

impl Mcp {
    /// Spawn `trawl mcp --profile agent` under `home`, speaking `era`.
    fn spawn(home: &Path, era: Era) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_trawl"))
            .env_clear()
            .envs(loader_path())
            .env("HOME", home)
            .env("XDG_STATE_HOME", home.join(".state"))
            .args(["mcp", "--profile", PROFILE])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn trawl mcp");
        let stdin = child.stdin.take();
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut stderr = child.stderr.take().expect("stderr");
        let (tx, lines) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let mut line = Vec::new();
                match stdout.read_until(b'\n', &mut line).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        let line = match line.strip_suffix(b"\n") {
                            Some(body) => Ok(String::from_utf8_lossy(body).into_owned()),
                            None => Err(line),
                        };
                        if tx.send(line).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        let stderr = tokio::spawn(async move {
            let mut text = String::new();
            stderr.read_to_string(&mut text).await.ok();
            text
        });
        Self {
            era,
            child,
            stdin,
            lines,
            seen: Vec::new(),
            trailing: Vec::new(),
            stderr,
            next_id: 1,
            outputs: BTreeMap::new(),
        }
    }

    /// Write one message as a line.
    async fn send(&mut self, message: &Value) {
        let mut line = serde_json::to_vec(message).expect("serialize a frame");
        line.push(b'\n');
        let stdin = self.stdin.as_mut().expect("stdin is open");
        stdin.write_all(&line).await.expect("write to trawl mcp");
        stdin.flush().await.expect("flush trawl mcp's stdin");
    }

    /// Send a request without waiting for its answer; returns its id. A
    /// 2026-07-28 request carries the era's `_meta`.
    async fn start(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let mut params = params;
        if let Some(meta) = self.era.meta() {
            params
                .as_object_mut()
                .expect("params are an object")
                .insert("_meta".to_owned(), meta);
        }
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        id
    }

    /// Send a request and wait for its response.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.start(method, params).await;
        self.response(id).await
    }

    /// Send a notification.
    async fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    /// The response to `id`: one already read, or the next one with that
    /// id, read under [`DEADLINE`].
    async fn response(&mut self, id: u64) -> Value {
        let answers = |line: &str| {
            serde_json::from_str::<Value>(line)
                .ok()
                .filter(|message| message["id"] == json!(id) && message.get("method").is_none())
        };
        if let Some(message) = self.seen.iter().find_map(|line| answers(line)) {
            return message;
        }
        let deadline = Instant::now() + DEADLINE;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.lines.recv()).await {
                Ok(Some(Ok(line))) => {
                    let answer = answers(&line);
                    self.seen.push(line);
                    if let Some(message) = answer {
                        return message;
                    }
                }
                Ok(Some(Err(partial))) => self.trailing.extend(partial),
                Ok(None) => panic!("stdout closed before a response to {id}: {:?}", self.seen),
                Err(elapsed) => panic!("no response to {id}: {elapsed}: {:?}", self.seen),
            }
        }
    }

    /// Open the session the era's way.
    async fn open(&mut self) {
        match self.era {
            Era::Initialize => {
                let init = self
                    .request(
                        "initialize",
                        json!({
                            "protocolVersion": self.era.version(),
                            "capabilities": {},
                            "clientInfo": {"name": "mcp_conformance", "version": "0"},
                        }),
                    )
                    .await;
                let result = &init["result"];
                assert_eq!(result["protocolVersion"], self.era.version(), "{init}");
                assert_eq!(result["serverInfo"]["name"], "trawl", "{init}");
                self.notify("notifications/initialized", json!({})).await;
            }
            Era::Discover => {
                let discover = self.request("server/discover", json!({})).await;
                let result = &discover["result"];
                let versions: Vec<&str> = result["supportedVersions"]
                    .as_array()
                    .unwrap_or_else(|| panic!("no supported versions: {discover}"))
                    .iter()
                    .filter_map(Value::as_str)
                    .collect();
                assert!(versions.contains(&"2026-07-28"), "{discover}");
                assert!(versions.contains(&"2025-11-25"), "{discover}");
                assert_eq!(
                    result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "trawl",
                    "{discover}"
                );
                assert!(result["capabilities"]["tools"].is_object(), "{discover}");
            }
        }
    }

    /// `tools/list`: the tool table, whose output schemas every later
    /// call is checked against.
    async fn list_tools(&mut self) -> Vec<Value> {
        let listed = self.request("tools/list", json!({})).await;
        let tools = listed["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("no tools: {listed}"))
            .clone();
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("a tool name"))
            .collect();
        assert_eq!(names, TOOLS, "{listed}");
        for tool in &tools {
            let name = tool["name"].as_str().unwrap().to_owned();
            let validator = jsonschema::draft202012::new(&tool["outputSchema"])
                .unwrap_or_else(|e| panic!("{name}'s outputSchema does not compile: {e}"));
            self.outputs.insert(name, validator);
        }
        tools
    }

    /// Call `tool` with `arguments` and hold its result to the text
    /// contract: one text block, and on success a `structuredContent`
    /// object valid under the tool's `outputSchema`.
    async fn call(&mut self, tool: &str, arguments: Value) -> Call {
        let response = self
            .request("tools/call", json!({"name": tool, "arguments": arguments}))
            .await;
        Self::check(&self.outputs, tool, &response)
    }

    fn check(
        outputs: &BTreeMap<String, jsonschema::Validator>,
        tool: &str,
        response: &Value,
    ) -> Call {
        assert!(
            response.get("error").is_none(),
            "{tool}: a protocol error: {response}"
        );
        let result = &response["result"];
        let content = result["content"]
            .as_array()
            .unwrap_or_else(|| panic!("{tool}: no content: {response}"));
        assert_eq!(content.len(), 1, "{tool}: exactly one block: {response}");
        assert_eq!(content[0]["type"], "text", "{tool}: {response}");
        let text = content[0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool}: the block is not text: {response}"))
            .to_owned();
        let is_error = result["isError"] == json!(true);
        let structured = result.get("structuredContent").cloned();
        if is_error {
            assert!(
                structured.is_none(),
                "{tool}: an error carries text only: {response}"
            );
        } else {
            let mirror = structured
                .as_ref()
                .unwrap_or_else(|| panic!("{tool}: success without a mirror: {response}"));
            assert!(mirror.is_object(), "{tool}: {response}");
            let validator = outputs
                .get(tool)
                .unwrap_or_else(|| panic!("{tool} was not listed"));
            let errors: Vec<String> = validator
                .iter_errors(mirror)
                .map(|e| format!("{e} at {}", e.instance_path()))
                .collect();
            assert!(
                errors.is_empty(),
                "{tool}: structuredContent fails its outputSchema: {errors:?}\n{response}"
            );
        }
        Call {
            raw: response.clone(),
            text,
            structured,
            is_error,
        }
    }

    /// Close stdin, read stdout to EOF and wait for the exit, under
    /// [`DEADLINE`].
    async fn finish(mut self) -> Finished {
        drop(self.stdin.take());
        let deadline = Instant::now() + DEADLINE;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.lines.recv()).await {
                Ok(Some(Ok(line))) => self.seen.push(line),
                Ok(Some(Err(partial))) => self.trailing.extend(partial),
                Ok(None) => break,
                Err(elapsed) => panic!(
                    "stdout did not reach EOF after stdin closed: {elapsed}: {:?}",
                    self.seen
                ),
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        let status = tokio::time::timeout(left, self.child.wait())
            .await
            .expect("trawl mcp exits after stdin closed")
            .expect("wait for trawl mcp");
        Finished {
            status: status.code(),
            lines: self.seen,
            trailing: self.trailing,
            stderr: self.stderr.await.expect("the stderr reader"),
        }
    }
}

impl Finished {
    /// Every stdout line is one JSON-RPC 2.0 message, nothing follows the
    /// last newline, and the exit was 0. stderr is the caller's to judge.
    fn assert_clean(&self, case: &str) {
        let context = format!(
            "{case}\nstatus {:?}\ntrailing: {:?}\nstderr: {}",
            self.status,
            String::from_utf8_lossy(&self.trailing),
            self.stderr
        );
        assert!(self.trailing.is_empty(), "partial message: {context}");
        for line in &self.lines {
            let message: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("not JSON ({e}): {line:?}\n{context}"));
            assert_eq!(message["jsonrpc"], "2.0", "not JSON-RPC: {line}\n{context}");
            assert!(
                message.get("id").is_some() || message.get("method").is_some(),
                "neither a response nor a request: {line}\n{context}"
            );
        }
        assert_eq!(self.status, Some(0), "clean exit after EOF: {context}");
    }

    /// Whether any stdout message answers `id`.
    fn answered(&self, id: u64) -> bool {
        self.lines.iter().any(|line| {
            serde_json::from_str::<Value>(line)
                .is_ok_and(|message| message["id"] == json!(id) && message.get("method").is_none())
        })
    }
}

/// RFC 3339 with microseconds, in UTC.
fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// A server that accepts the corpus's two environments.
fn options() -> Options {
    Options {
        envs: vec!["prod".to_owned(), "staging".to_owned()],
        ..Options::default()
    }
}

/// Ingest the corpus, compact it into Parquet and the catalog, and fill
/// the per-service schema cache from it.
///
/// - [`API_EVENTS`] of `api` in `prod`, newest at `now`, alternating hosts
///   `web-1` and `web-2`; every [`LONG_EVERY`]th carries a 1500-byte
///   message, so a page abbreviates it.
/// - [`WORKER_EVENTS`] of `worker` in `staging` on `web-2`, each
///   [`WORKER_LAG`] behind its arrival: the lagging sender.
///
/// `api` holds 85 % of the corpus: the dominant service.
async fn seed(server: &mut Server) -> DateTime<Utc> {
    let now = Utc::now();
    let mut events = Vec::with_capacity(API_EVENTS + WORKER_EVENTS);
    for i in 0..API_EVENTS {
        let at = now - TimeDelta::milliseconds(500 * i64::try_from(i).unwrap());
        let message = if i % LONG_EVERY == 0 {
            format!("request {i} {SIGHTED} {}", "x".repeat(1500))
        } else {
            format!("request {i} ok")
        };
        events.push(json!({
            "_time": rfc3339(at),
            "service": "api",
            "env": "prod",
            "host": if i % 2 == 0 { "web-1" } else { "web-2" },
            "level": "info",
            "message": message,
        }));
    }
    for i in 0..WORKER_EVENTS {
        let at = now - WORKER_LAG - TimeDelta::seconds(i64::try_from(i).unwrap());
        events.push(json!({
            "_time": rfc3339(at),
            "service": "worker",
            "env": "staging",
            "host": "web-2",
            "level": "warn",
            "message": format!("job {i} done"),
        }));
    }
    for batch in events.chunks(100) {
        server.ingest(batch).await;
    }
    server.compact().await;
    server.refresh_schema(2).await;
    now
}

/// A temp `HOME` whose default config names the [`PROFILE`] profile:
/// `server`'s URL, its pinned certificate, and the agent key in the
/// profile's own table. Times display in UTC.
fn home(server: &Server) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("tempdir");
    let dir = home.path().join(".config/trawl");
    std::fs::create_dir_all(&dir).expect("config dir");
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[ui]\ntimezone = \"UTC\"\n\n[profiles.{PROFILE}]\nurl = {:?}\nca_cert = {:?}\ntoken = {:?}\n",
            server.url,
            server.cert_path.to_str().expect("utf-8 cert path"),
            server.agent_token,
        ),
    )
    .expect("write the CLI config");
    home
}

/// The lines of `text` after its `rows from captured row N:` line.
fn row_lines(text: &str) -> Vec<&str> {
    text.lines()
        .skip_while(|line| !line.starts_with("rows from captured row "))
        .skip(1)
        .collect()
}

/// The hint lines of `text`, without their `hint: ` prefix.
fn hints(text: &str) -> Vec<&str> {
    text.lines()
        .filter_map(|line| line.strip_prefix("hint: "))
        .collect()
}

/// The proposed `read` call of `structured` with `action`.
fn next_action<'a>(structured: &'a Value, action: &str) -> Option<&'a Value> {
    structured["next_actions"]
        .as_array()?
        .iter()
        .find(|next| next["tool"] == "read" && next["arguments"]["action"] == action)
}

/// An RFC 3339 instant.
fn instant(text: &Value) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text.as_str().unwrap_or_else(|| panic!("not text: {text}")))
        .unwrap_or_else(|e| panic!("not RFC 3339 ({e}): {text}"))
        .with_timezone(&Utc)
}

/// The cell of `view`'s printed `row` under the column named `name`.
fn cell_of<'a>(view: &'a Value, row: usize, name: &str) -> &'a Value {
    let position = view["columns"]
        .as_array()
        .unwrap()
        .iter()
        .position(|column| column["name"] == name)
        .unwrap_or_else(|| panic!("no column {name:?}: {view}"));
    &view["rows"][row][position]
}

/// An integer cell, which the wire carries as an exact decimal string.
fn integer(cell: &Value) -> u64 {
    cell.as_str()
        .and_then(|digits| digits.parse().ok())
        .or_else(|| cell.as_u64())
        .unwrap_or_else(|| panic!("not an integer: {cell}"))
}

/// `at` as the seed sent it: to the microsecond.
fn sent(at: DateTime<Utc>) -> DateTime<Utc> {
    instant(&json!(rfc3339(at)))
}

/// What `/api/v1/queries` and the agent's result list hold: a read that
/// re-ran a query, or captured again, changes one of them.
async fn executions(server: &Server) -> (Vec<u64>, Vec<u64>, Vec<String>) {
    let queries = server
        .client(&server.admin_token)
        .queries()
        .await
        .expect("list the queries");
    let results = server
        .client(&server.agent_token)
        .list_results()
        .await
        .expect("list the agent's results");
    (
        queries.active.iter().map(|q| q.snapshot.id).collect(),
        queries.recent.iter().map(|q| q.snapshot.id).collect(),
        results.results.into_iter().map(|r| r.id).collect(),
    )
}

/// Drive every tool through `era` against `server`, whose `link_origin`
/// is [`LINK_ORIGIN`]; returns the listed tools.
#[allow(clippy::too_many_lines)] // one session, each call asserted in order
async fn every_tool(server: &Server, home: &Path, era: Era, now: DateTime<Utc>) -> Vec<Value> {
    let case = format!("era {}", era.version());
    let mut mcp = Mcp::spawn(home, era);
    mcp.open().await;
    let tools = mcp.list_tools().await;

    let resources = mcp.request("resources/list", json!({})).await;
    let uris: Vec<&str> = resources["result"]["resources"]
        .as_array()
        .unwrap_or_else(|| panic!("{case}: no resources: {resources}"))
        .iter()
        .filter_map(|r| r["uri"].as_str())
        .collect();
    assert!(
        uris.contains(&"trawl://dsl/search-stage"),
        "{case}: {uris:?}"
    );

    // ping is a session request: 2026-07-28 removed it from the inline
    // lifecycle.
    let ping_id = mcp.start("ping", json!({})).await;
    let ping = mcp.response(ping_id).await;
    match era {
        Era::Initialize => assert!(ping["result"].is_object(), "{case}: {ping}"),
        Era::Discover => assert_eq!(ping["error"]["code"], -32601, "{case}: {ping}"),
    }

    // orient: the installation, in UTC, naming what was seeded.
    let orient = mcp.call("orient", json!({})).await;
    assert!(!orient.is_error, "{case}: {}", orient.text);
    let mirror = orient.structured.as_ref().unwrap();
    assert_eq!(
        mirror["display_zone"], "UTC, the mcp host's setting",
        "{case}: {}",
        orient.text
    );
    assert!(
        mirror.get("degraded_note").is_none(),
        "{case}: the schema routes answered: {}",
        orient.text
    );
    let services: Vec<&str> = mirror["inventory"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["service"].as_str())
        .collect();
    for service in ["api", "worker"] {
        assert!(services.contains(&service), "{case}: {}", orient.text);
    }
    assert_eq!(
        mirror["key"]["permissions"],
        json!(["query", "schema_read", "validate"]),
        "{case}: {}",
        orient.text
    );

    // fields of a service, from the catalog the compaction filled.
    let fields = mcp
        .call("fields", json!({"source": "service", "service": "api"}))
        .await;
    assert!(!fields.is_error, "{case}: {}", fields.text);
    let names: Vec<&str> = fields.structured.as_ref().unwrap()["fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert!(names.contains(&"host"), "{case}: {}", fields.text);
    assert!(names.contains(&"message"), "{case}: {}", fields.text);

    // validate: a valid query, and an invalid one that is still a
    // successful call, with a caret under the error.
    let valid = mcp
        .call(
            "search",
            json!({"mode": "validate", "query": "service=api last=1h"}),
        )
        .await;
    assert!(!valid.is_error, "{case}: {}", valid.text);
    assert_eq!(
        valid.structured.as_ref().unwrap()["validation"]["valid"],
        true,
        "{case}: {}",
        valid.text
    );
    let invalid = mcp
        .call(
            "search",
            json!({"mode": "validate", "query": "service=api last=1h | stats cout("}),
        )
        .await;
    assert!(
        !invalid.is_error,
        "{case}: a failed validation is a successful call: {}",
        invalid.text
    );
    let verdict = &invalid.structured.as_ref().unwrap()["validation"];
    assert_eq!(verdict["valid"], false, "{case}: {}", invalid.text);
    let excerpt = verdict["errors"][0]["excerpt"]
        .as_str()
        .unwrap_or_else(|| panic!("{case}: no excerpt: {}", invalid.text));
    assert!(excerpt.contains('^'), "{case}: {excerpt}");
    assert!(invalid.text.contains('^'), "{case}: {}", invalid.text);

    // execute over everything: rows, facets, the search URL, and the
    // dominant service named over the captured rows.
    let all = mcp
        .call("search", json!({"mode": "execute", "query": "last=1h"}))
        .await;
    assert!(!all.is_error, "{case}: {}", all.text);
    let reply = all.structured.as_ref().unwrap();
    let view = &reply["result"];
    let total = API_EVENTS + WORKER_EVENTS;
    assert_eq!(view["rows_captured"], total, "{case}: {}", all.text);
    // The response read the default rows; the ones it printed and the
    // ones it dropped to fit the default budget make them up.
    let printed = view["rows"].as_array().unwrap().len();
    assert!(printed > 0, "{case}: {}", all.text);
    assert_eq!(row_lines(&all.text).len(), printed, "{case}: {}", all.text);
    assert_eq!(
        printed as u64
            + view["presentation"]["rows_dropped_to_fit"]
                .as_u64()
                .unwrap(),
        u64::from(trawl_mcp::tools::schema::DEFAULT_ROWS),
        "{case}: {}",
        all.text
    );
    assert!(
        view["facets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|facet| facet["field"] == "service"),
        "{case}: {}",
        all.text
    );
    let url = view["link"]["url"]
        .as_str()
        .unwrap_or_else(|| panic!("{case}: no search URL: {}", all.text));
    assert!(url.starts_with(&format!("{LINK_ORIGIN}/")), "{case}: {url}");
    assert!(
        all.text.contains(&format!("search URL: {url}")),
        "{case}: {}",
        all.text
    );
    let dominance = format!(
        "dominance: \"service\" \"api\" holds {API_EVENTS} of the {total} captured rows (85%)"
    );
    assert!(
        hints(&all.text).iter().any(|h| h.starts_with(&dominance)),
        "{case}: expected {dominance:?} in {}",
        all.text
    );
    assert!(
        !view["abbreviated"].as_array().unwrap().is_empty(),
        "{case}: the long messages are abbreviated: {}",
        all.text
    );

    // The executed bounds are the capture's.
    let result_id = view["result_id"].as_str().unwrap().to_owned();
    let listed = server
        .client(&server.agent_token)
        .list_results()
        .await
        .expect("list the agent's results");
    let captured = listed
        .results
        .iter()
        .find(|r| r.id == result_id)
        .unwrap_or_else(|| panic!("{case}: {result_id} is not listed"));
    assert_eq!(
        instant(&view["bounds"]["earliest"]),
        instant(&json!(
            captured.earliest.as_deref().expect("captured earliest")
        )),
        "{case}"
    );
    assert_eq!(
        instant(&view["bounds"]["latest"]),
        instant(&json!(captured.latest.as_deref().expect("captured latest"))),
        "{case}"
    );

    // The lagging sender, over the rows the response read.
    let worker = mcp
        .call(
            "search",
            json!({"mode": "execute", "query": "service=worker last=1h"}),
        )
        .await;
    assert!(!worker.is_error, "{case}: {}", worker.text);
    assert!(
        hints(&worker.text)
            .iter()
            .any(|h| h.starts_with("lag: service \"worker\" on host \"web-2\" sent an event ")),
        "{case}: {}",
        worker.text
    );

    // No rows in the bounds: the nearest bounds holding data for the same
    // search.
    let empty_query = format!(
        "service=api earliest=\"{}\" latest=\"{}\"",
        rfc3339(now - TimeDelta::hours(3)),
        rfc3339(now - TimeDelta::hours(2))
    );
    let empty = mcp
        .call("search", json!({"mode": "execute", "query": empty_query}))
        .await;
    assert!(!empty.is_error, "{case}: {}", empty.text);
    assert_eq!(
        empty.structured.as_ref().unwrap()["result"]["rows_produced"],
        0,
        "{case}: {}",
        empty.text
    );
    assert!(
        hints(&empty.text).iter().any(|h| h
            .starts_with("nearest bounds over events matching the same search stage from ")
            && h.contains("the first event after them is at ")),
        "{case}: {}",
        empty.text
    );

    // Wide rows under a small budget: the renderer drops rows, and the
    // page continuation starts after the rows the text printed.
    let tight = mcp
        .call(
            "search",
            json!({"mode": "execute", "query": "service=api last=1h", "rows": 200, "max_bytes": 4096}),
        )
        .await;
    assert!(!tight.is_error, "{case}: {}", tight.text);
    let tight_view = &tight.structured.as_ref().unwrap()["result"];
    assert!(
        tight_view["presentation"]["rows_dropped_to_fit"]
            .as_u64()
            .unwrap()
            > 0,
        "{case}: {}",
        tight.text
    );
    let printed = row_lines(&tight.text).len() as u64;
    let continuation = next_action(tight.structured.as_ref().unwrap(), "page")
        .unwrap_or_else(|| panic!("{case}: no page continuation: {}", tight.text));
    assert_eq!(
        continuation["arguments"]["offset"].as_u64().unwrap(),
        tight_view["offset"].as_u64().unwrap() + printed,
        "{case}: {}",
        tight.text
    );

    // read never runs the query again.
    let before = executions(server).await;
    let page = mcp
        .call(
            "read",
            json!({"action": "page", "result_id": result_id, "offset": 50, "rows": 10}),
        )
        .await;
    assert!(!page.is_error, "{case}: {}", page.text);
    let page_view = &page.structured.as_ref().unwrap()["result"];
    assert_eq!(page_view["offset"], 50, "{case}: {}", page.text);
    assert_eq!(
        page_view["rows"].as_array().unwrap().len(),
        10,
        "{case}: {}",
        page.text
    );
    assert_eq!(row_lines(&page.text).len(), 10, "{case}: {}", page.text);

    let abbreviated = view["abbreviated"][0].clone();
    let cell = mcp
        .call(
            "read",
            json!({
                "action": "cell",
                "result_id": result_id,
                "row": abbreviated["row"],
                "column": abbreviated["column"],
            }),
        )
        .await;
    assert!(!cell.is_error, "{case}: {}", cell.text);
    let cell_view = &cell.structured.as_ref().unwrap()["cell"];
    assert_eq!(
        cell_view["total_bytes"], abbreviated["total_bytes"],
        "{case}: {}",
        cell.text
    );
    assert_eq!(cell_view["offset"], 0, "{case}: {}", cell.text);
    assert_eq!(
        cell_view["length"], abbreviated["total_bytes"],
        "{case}: the whole cell fits the default budget: {}",
        cell.text
    );

    let columns = mcp
        .call(
            "fields",
            json!({"source": "result", "result_id": result_id}),
        )
        .await;
    assert!(!columns.is_error, "{case}: {}", columns.text);
    assert_eq!(
        columns.structured.as_ref().unwrap()["fields"]
            .as_array()
            .unwrap()
            .len(),
        view["columns"].as_array().unwrap().len()
            + usize::try_from(view["presentation"]["columns_omitted"].as_u64().unwrap()).unwrap(),
        "{case}: {}",
        columns.text
    );
    assert_eq!(
        executions(server).await,
        before,
        "{case}: read and fields ran nothing and captured nothing"
    );

    // context of row 0, the newest event: `api` at `now` on `web-1` in
    // `prod`. That host holds every other `api` event, one a second, so
    // the 30 seconds before it hold 30 rows and the default `before` keeps
    // the nearest 10; nothing shares its `_time` and nothing is later.
    let newest = sent(now);
    assert_eq!(
        instant(cell_of(view, 0, "_time")),
        newest,
        "{case}: {}",
        all.text
    );
    assert_eq!(cell_of(view, 0, "host"), "web-1", "{case}: {}", all.text);
    let context = mcp
        .call("context", json!({"result_id": result_id, "row": 0}))
        .await;
    assert!(!context.is_error, "{case}: {}", context.text);
    let reply = context.structured.as_ref().unwrap();
    let facts = &reply["correlation"]["context"];
    assert_eq!(reply["correlation"]["kind"], "context", "{case}: {reply}");
    assert_eq!(
        facts["scope_used"],
        json!({"env": "prod", "host": "web-1", "seconds": 30}),
        "{case}: {}",
        context.text
    );
    assert_eq!(facts["focus"]["result_id"], result_id, "{case}: {facts}");
    assert_eq!(facts["focus"]["row"], 0, "{case}: {facts}");
    assert_eq!(instant(&facts["focus"]["time"]), newest, "{case}: {facts}");
    assert_eq!(
        [&facts["earlier"], &facts["same_time"], &facts["later"]],
        [
            &json!({"start": 0, "end": 10, "rows": 10}),
            &json!({"start": 10, "end": 11, "rows": 1}),
            &json!({"start": 11, "end": 11, "rows": 0}),
        ],
        "{case}: {}",
        context.text
    );
    assert_eq!(
        reply["result"]["rows_captured"], 11,
        "{case}: {}",
        context.text
    );
    assert_ne!(
        reply["result"]["result_id"], result_id,
        "{case}: a new capture"
    );
    assert!(
        context
            .text
            .lines()
            .any(|line| line == trawl_mcp::render::correlation::NO_ORDER),
        "{case}: {}",
        context.text
    );

    // sightings of a literal every LONG_EVERYth `api` message carries:
    // one (env, service) group, the newest at `now`, the oldest
    // 325 half-seconds before it, all in `message`.
    let sighted: Vec<usize> = (0..API_EVENTS).step_by(LONG_EVERY).collect();
    let matches = sighted.len();
    let oldest =
        sent(now - TimeDelta::milliseconds(500 * i64::try_from(*sighted.last().unwrap()).unwrap()));
    let sightings = mcp
        .call("sightings", json!({"literal": SIGHTED, "last": "1h"}))
        .await;
    assert!(!sightings.is_error, "{case}: {}", sightings.text);
    let reply = sightings.structured.as_ref().unwrap();
    let facts = &reply["correlation"]["sightings"];
    assert_eq!(reply["correlation"]["kind"], "sightings", "{case}: {reply}");
    assert_eq!(facts["literal"], SIGHTED, "{case}: {facts}");
    assert_eq!(
        facts["matches_total"], matches,
        "{case}: {}",
        sightings.text
    );
    assert_eq!(facts["captured"], matches, "{case}: {}", sightings.text);
    let groups = facts["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{case}: {}", sightings.text);
    assert_eq!(
        groups[0].as_array().unwrap()[..3],
        [json!("prod"), json!("api"), json!(matches)],
        "{case}: {}",
        sightings.text
    );
    assert_eq!(instant(&groups[0][3]), oldest, "{case}: {facts}");
    assert_eq!(instant(&groups[0][4]), newest, "{case}: {facts}");
    assert_eq!(
        facts["per_service"],
        json!([{"key": "api", "count": matches}]),
        "{case}: {facts}"
    );
    assert!(
        facts["fields"]
            .as_array()
            .unwrap()
            .contains(&json!({"key": "message", "count": matches})),
        "{case}: {}",
        sightings.text
    );
    assert_eq!(facts["raw_only_rows"], 0, "{case}: {facts}");
    let denominators = format!(
        "sightings of \"{SIGHTED}\": field counts are over the {matches} captured matches; \
         service counts are over all {matches}"
    );
    assert!(
        sightings.text.lines().any(|line| line == denominators),
        "{case}: {}",
        sightings.text
    );
    let sightings_id = reply["result"]["result_id"].as_str().unwrap().to_owned();
    let sighted_facts = facts.clone();
    assert_eq!(
        reply["result"]["rows_captured"], matches,
        "{case}: {}",
        sightings.text
    );

    // compare `host=web-2` over the hour around the seed against the hour
    // before it, which the tool defaults: the odd `api` events and every
    // `worker` event are in the foreground, nothing is in the baseline.
    let foreground = (
        sent(now - TimeDelta::minutes(30)),
        sent(now + TimeDelta::minutes(30)),
    );
    let compare = mcp
        .call(
            "compare",
            json!({
                "filter": "host=web-2",
                "foreground_earliest": rfc3339(foreground.0),
                "foreground_latest": rfc3339(foreground.1),
                "group_by": ["service"],
            }),
        )
        .await;
    assert!(!compare.is_error, "{case}: {}", compare.text);
    let reply = compare.structured.as_ref().unwrap();
    let facts = &reply["correlation"]["compare"];
    assert_eq!(reply["correlation"]["kind"], "compare", "{case}: {reply}");
    assert_eq!(facts["rows_ranked"], 2, "{case}: {}", compare.text);
    assert_eq!(facts["group_by"], json!(["service"]), "{case}: {facts}");
    assert_eq!(
        facts["foreground"]["duration_ms"], 3_600_000,
        "{case}: {facts}"
    );
    assert_eq!(
        instant(&facts["baseline"]["start"]),
        foreground.0 - TimeDelta::hours(1),
        "{case}: {facts}"
    );
    assert_eq!(
        instant(&facts["baseline"]["end"]),
        foreground.0,
        "{case}: {facts}"
    );
    let ranked = &reply["result"];
    assert_eq!(
        ranked["rows"].as_array().unwrap().len(),
        2,
        "{case}: {}",
        compare.text
    );
    let api_on_web_2 = API_EVENTS / 2;
    for (row, service, count) in [(0, "api", api_on_web_2), (1, "worker", WORKER_EVENTS)] {
        assert_eq!(
            cell_of(ranked, row, "service"),
            service,
            "{case}: {}",
            compare.text
        );
        assert_eq!(
            integer(cell_of(ranked, row, "compare_foreground_count")),
            count as u64,
            "{case}: {}",
            compare.text
        );
        assert_eq!(
            integer(cell_of(ranked, row, "compare_baseline_count")),
            0,
            "{case}: {}",
            compare.text
        );
        assert_eq!(
            cell_of(ranked, row, "compare_status"),
            "new_in_foreground",
            "{case}: {}",
            compare.text
        );
    }

    // read of the sightings capture: page 0 carries the same block.
    let sighted_page = mcp
        .call(
            "read",
            json!({"action": "page", "result_id": sightings_id, "offset": 0}),
        )
        .await;
    assert!(!sighted_page.is_error, "{case}: {}", sighted_page.text);
    let page_reply = sighted_page.structured.as_ref().unwrap();
    assert_eq!(
        page_reply["correlation"]["sightings"], sighted_facts,
        "{case}: {}",
        sighted_page.text
    );
    assert_eq!(
        page_reply["result"]["offset"], 0,
        "{case}: {}",
        sighted_page.text
    );
    assert!(
        sighted_page.text.lines().any(|line| line == denominators),
        "{case}: {}",
        sighted_page.text
    );

    // A DSL reference section.
    let section = mcp
        .request("resources/read", json!({"uri": "trawl://dsl/search-stage"}))
        .await;
    let contents = &section["result"]["contents"][0];
    assert_eq!(contents["mimeType"], "text/markdown", "{case}: {section}");
    assert!(
        contents["text"]
            .as_str()
            .unwrap()
            .starts_with("## Search stage"),
        "{case}: {section}"
    );

    // A well-formed id that names no result is the server's 404, as a
    // tool error.
    let gone = mcp
        .call(
            "read",
            json!({"action": "page", "result_id": "0123456789abcdef0123456789abcdef"}),
        )
        .await;
    assert!(gone.is_error, "{case}: {}", gone.raw);
    assert!(gone.text.contains("HTTP 404"), "{case}: {}", gone.text);

    let finished = mcp.finish().await;
    finished.assert_clean(&case);
    // rmcp logs a refused request at WARN, to stderr: the 2026-07-28 ping
    // is the only one, and nothing else is logged.
    let logged: Vec<&str> = finished.stderr.lines().collect();
    match era {
        Era::Initialize => assert!(logged.is_empty(), "{case}: {}", finished.stderr),
        Era::Discover => assert!(
            logged.len() == 1
                && logged[0].contains(&format!("WARN rmcp::service: response error id={ping_id} "))
                && logged[0].contains("ErrorCode(-32601)"),
            "{case}: {}",
            finished.stderr
        ),
    }
    tools
}

/// Both eras list the same tools, in the same order, with the same
/// schemas, and call every tool against a seeded trawld.
#[tokio::test(flavor = "multi_thread")]
async fn every_tool_answers_in_both_protocol_eras() {
    let mut server = Server::start_with(Options {
        link_origin: Some(LINK_ORIGIN.to_owned()),
        ..options()
    })
    .await;
    let now = seed(&mut server).await;
    let home = home(&server);
    let mut listed = Vec::new();
    for era in Era::ALL {
        listed.push(every_tool(&server, home.path(), era, now).await);
    }
    assert_eq!(
        listed[0], listed[1],
        "tools/list differs between 2025-11-25 and 2026-07-28"
    );
    server.stop().await;
}

/// The id of the query `/api/v1/queries` lists active whose text holds
/// `marker`, if any.
async fn active_capture(server: &Server, marker: &str) -> Option<u64> {
    let queries = server
        .client(&server.admin_token)
        .queries()
        .await
        .expect("list the queries");
    queries
        .active
        .iter()
        .find(|q| q.snapshot.query.contains(marker))
        .map(|q| q.snapshot.id)
}

/// Poll `probe` until it answers `Some`, failing after [`DEADLINE`].
async fn until<T, F, Fut>(what: &str, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(found) = probe().await {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Without `link_origin` the search says why it has no URL; a cancelled
/// search drops its trawld request, which the server records as
/// abandoned, and the child sends no response for it. In both eras.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_search_drops_its_server_query_in_both_eras() {
    let mut server = Server::start_with(options()).await;
    seed(&mut server).await;
    let home = home(&server);
    for era in Era::ALL {
        let case = format!("era {}", era.version());
        let mut mcp = Mcp::spawn(home.path(), era);
        mcp.open().await;
        mcp.list_tools().await;

        let plain = mcp
            .call(
                "search",
                json!({"mode": "execute", "query": "service=api last=1h"}),
            )
            .await;
        assert!(!plain.is_error, "{case}: {}", plain.text);
        let link = &plain.structured.as_ref().unwrap()["result"]["link"];
        assert!(link.get("url").is_none(), "{case}: {link}");
        let reason = link["reason"]
            .as_str()
            .unwrap_or_else(|| panic!("{case}: no reason: {link}"));
        assert!(reason.contains("link_origin"), "{case}: {reason}");
        assert!(
            plain.text.contains(&format!("no search URL: {reason}")),
            "{case}: {}",
            plain.text
        );

        // Hold the corpus: a capture over a window waits for the
        // publication read guard, tracked and in flight.
        let gate = server.state.query.pool.publication();
        let held = gate.write().await;
        let marker = format!("cancelme{}", era.version().replace('-', ""));
        let query = format!("service=api {marker} last=1h");
        let id = mcp
            .start(
                "tools/call",
                json!({"name": "search", "arguments": {"mode": "execute", "query": query}}),
            )
            .await;
        let query_id = until("the capture in flight", || active_capture(&server, &marker)).await;

        mcp.notify(
            "notifications/cancelled",
            json!({"requestId": id, "reason": "the test cancels it"}),
        )
        .await;
        until("the capture recorded as abandoned", || async {
            let queries = server
                .client(&server.admin_token)
                .queries()
                .await
                .expect("list the queries");
            let gone = !queries.active.iter().any(|q| q.snapshot.id == query_id);
            let recent = queries.recent.iter().find(|q| q.snapshot.id == query_id)?;
            gone.then(|| {
                assert_eq!(
                    recent.snapshot.error.as_deref(),
                    Some(trawl_server::tracker::ABANDONED),
                    "{case}: {recent:?}"
                );
                assert_eq!(recent.snapshot.rows, None, "{case}: {recent:?}");
            })
        })
        .await;
        drop(held);

        // The session goes on; the cancelled request is never answered.
        mcp.list_tools().await;
        let finished = mcp.finish().await;
        finished.assert_clean(&case);
        // rmcp logs the dropped call's error at WARN, to stderr; it is the
        // only line there.
        let logged: Vec<&str> = finished.stderr.lines().collect();
        assert!(
            logged.len() == 1
                && logged[0].contains(&format!("WARN rmcp::service: response error id={id} "))
                && logged[0].contains("the request was cancelled"),
            "{case}: {}",
            finished.stderr
        );
        assert!(
            !finished.answered(id),
            "{case}: a response arrived for the cancelled request {id}: {:?}",
            finished.lines
        );
    }
    server.stop().await;
}

/// A capture past its deadline is trawld's 410 `result_gone` to `read`
/// and to `fields`, as a tool error, and neither runs a query or captures
/// a replacement. In both eras.
#[tokio::test(flavor = "multi_thread")]
async fn an_expired_capture_reads_as_result_gone_in_both_eras() {
    let clock = ResultsClock::fixed(Utc::now());
    let mut server = Server::start_with(Options {
        results_clock: clock.clone(),
        ..options()
    })
    .await;
    seed(&mut server).await;
    let home = home(&server);
    for era in Era::ALL {
        let case = format!("era {}", era.version());
        // The capture's bounds resolve against the results clock: now,
        // after the corpus was seeded.
        clock.set(Utc::now());
        let mut mcp = Mcp::spawn(home.path(), era);
        mcp.open().await;
        mcp.list_tools().await;

        let search = mcp
            .call(
                "search",
                json!({"mode": "execute", "query": "service=api last=1h"}),
            )
            .await;
        assert!(!search.is_error, "{case}: {}", search.text);
        let view = &search.structured.as_ref().unwrap()["result"];
        assert!(
            view["rows_captured"].as_u64().unwrap() > 0,
            "{case}: {view}"
        );
        let result_id = view["result_id"].as_str().unwrap().to_owned();
        let deadline = instant(&view["expires_at"]);
        let before = executions(&server).await;
        assert!(before.2.contains(&result_id), "{case}: {before:?}");

        clock.set(deadline + TimeDelta::seconds(1));

        let page = mcp
            .call("read", json!({"action": "page", "result_id": result_id}))
            .await;
        assert!(page.is_error, "{case}: {}", page.raw);
        assert!(page.structured.is_none(), "{case}: {}", page.raw);
        assert!(
            page.text
                .starts_with("read was refused: result_gone (HTTP 410)."),
            "{case}: {}",
            page.text
        );

        let fields = mcp
            .call(
                "fields",
                json!({"source": "result", "result_id": result_id}),
            )
            .await;
        assert!(fields.is_error, "{case}: {}", fields.raw);
        assert!(fields.structured.is_none(), "{case}: {}", fields.raw);
        assert!(
            fields.text.contains("result_gone (HTTP 410)"),
            "{case}: {}",
            fields.text
        );

        let after = executions(&server).await;
        assert_eq!(
            (&after.0, &after.1),
            (&before.0, &before.1),
            "{case}: the reads of an expired capture ran no query"
        );
        assert!(
            after.2.iter().all(|id| before.2.contains(id)),
            "{case}: no capture replaced the expired one: {before:?} then {after:?}"
        );

        let finished = mcp.finish().await;
        finished.assert_clean(&case);
        assert!(finished.stderr.is_empty(), "{case}: {}", finished.stderr);
    }
    server.stop().await;
}
