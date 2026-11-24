// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A `trawl mcp` child driven over its pipes with hand-written JSON-RPC
//! lines, for the suites that hold it to the MCP contract against a real
//! trawld (ADR-0058, ADR-0059).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;

use super::Server;

pub const PROFILE: &str = "agent";

pub const DEADLINE: Duration = Duration::from_secs(60);

pub const TOOLS: [&str; 8] = [
    "orient",
    "search",
    "read",
    "context",
    "sightings",
    "compare",
    "fields",
    "investigation",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Era {
    Initialize,
    Discover,
}

impl Era {
    pub const ALL: [Self; 2] = [Self::Initialize, Self::Discover];

    pub fn version(self) -> &'static str {
        match self {
            Self::Initialize => "2025-11-25",
            Self::Discover => "2026-07-28",
        }
    }

    pub fn meta(self) -> Option<Value> {
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
pub type Line = Result<String, Vec<u8>>;

pub struct Mcp {
    pub era: Era,
    pub child: Child,
    pub stdin: Option<ChildStdin>,
    pub lines: mpsc::UnboundedReceiver<Line>,
    pub seen: Vec<String>,
    pub trailing: Vec<u8>,
    pub stderr: tokio::task::JoinHandle<String>,
    pub next_id: u64,
    pub outputs: BTreeMap<String, jsonschema::Validator>,
}

pub struct Call {
    pub raw: Value,
    pub text: String,
    pub structured: Option<Value>,
    pub is_error: bool,
}

pub struct Finished {
    pub status: Option<i32>,
    pub lines: Vec<String>,
    pub trailing: Vec<u8>,
    pub stderr: String,
}

/// The test runner sets the loader path so a binary built in this target
/// directory finds the libraries it links (libduckdb in CI's archive), and a
/// child that lost it would die before reading its first frame.
pub fn loader_path() -> impl Iterator<Item = (&'static str, std::ffi::OsString)> {
    [
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
    ]
    .into_iter()
    .filter_map(|name| std::env::var_os(name).map(|value| (name, value)))
}

impl Mcp {
    pub fn spawn(home: &Path, era: Era) -> Self {
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

    pub async fn send(&mut self, message: &Value) {
        let mut line = serde_json::to_vec(message).expect("serialize a frame");
        line.push(b'\n');
        let stdin = self.stdin.as_mut().expect("stdin is open");
        stdin.write_all(&line).await.expect("write to trawl mcp");
        stdin.flush().await.expect("flush trawl mcp's stdin");
    }

    pub async fn start(&mut self, method: &str, params: Value) -> u64 {
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

    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.start(method, params).await;
        self.response(id).await
    }

    pub async fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    pub async fn response(&mut self, id: u64) -> Value {
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

    pub async fn open(&mut self) {
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

    pub async fn list_tools(&mut self) -> Vec<Value> {
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

    pub async fn call(&mut self, tool: &str, arguments: Value) -> Call {
        let response = self
            .request("tools/call", json!({"name": tool, "arguments": arguments}))
            .await;
        Self::check(&self.outputs, tool, &response)
    }

    pub fn check(
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

    pub async fn finish(mut self) -> Finished {
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
    pub fn assert_clean(&self, case: &str) {
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

    pub fn answered(&self, id: u64) -> bool {
        self.lines.iter().any(|line| {
            serde_json::from_str::<Value>(line)
                .is_ok_and(|message| message["id"] == json!(id) && message.get("method").is_none())
        })
    }
}

pub fn home(server: &Server) -> tempfile::TempDir {
    home_as(server, &server.agent_token)
}

pub fn home_as(server: &Server, token: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("tempdir");
    let dir = home.path().join(".config/trawl");
    std::fs::create_dir_all(&dir).expect("config dir");
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[ui]\ntimezone = \"UTC\"\n\n[profiles.{PROFILE}]\nurl = {:?}\nca_cert = {:?}\ntoken = {:?}\n",
            server.url,
            server.cert_path.to_str().expect("utf-8 cert path"),
            token,
        ),
    )
    .expect("write the CLI config");
    home
}
