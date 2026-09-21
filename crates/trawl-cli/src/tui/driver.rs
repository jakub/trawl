// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unix socket driver for programmatic TUI control.
//!
//! External tools (scripts, claude code, etc.) connect to a unix socket and
//! send NDJSON commands to inspect and control the running TUI. One client
//! at a time, inline in the accept loop.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

use trawl_client::QueryResponse;

// ---------------------------------------------------------------------------
// Protocol types
// ---------------------------------------------------------------------------

/// Inbound request from a driver client (deserialized from NDJSON).
///
/// Also serializable so the CLI client can construct and send requests.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum DriverRequest {
    /// Introspect current TUI state.
    Status,
    /// Replace the editor content.
    SetQuery { query: String },
    /// Execute the current query and wait for results.
    Execute {
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    /// Render the TUI to a virtual terminal and return as text.
    Capture {
        width: Option<u16>,
        height: Option<u16>,
    },
    /// Inject a single keystroke.
    Key { key: String },
    /// Inject multiple keystrokes sequentially.
    Keys { keys: Vec<String> },
    /// Get structured result data without rendering.
    GetResults { tab: Option<usize> },
    /// Clean exit.
    Quit,
}

fn default_timeout_ms() -> u64 {
    300_000 // 5 minutes
}

/// Outbound response to a driver client (serialized as NDJSON).
#[derive(Debug, Deserialize, Serialize)]
pub struct DriverResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(flatten)]
    pub data: DriverData,
}

/// Response payload: one flat struct shared by every command, with each
/// absent field omitted from the wire.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct DriverData {
    // status fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main_tab: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_rows: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_columns: Option<Vec<String>>,

    // execute / get_results fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<Vec<Vec<serde_json::Value>>>,

    // capture fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u16>,
}

impl DriverResponse {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            data: DriverData::default(),
        }
    }

    pub fn ok_with(data: DriverData) -> Self {
        Self {
            ok: true,
            error: None,
            data,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            data: DriverData::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Internal command type (sent from socket task → event loop)
// ---------------------------------------------------------------------------

/// A driver command carrying a oneshot reply channel.
#[derive(Debug)]
pub struct DriverCommand {
    pub request: DriverRequest,
    pub reply: oneshot::Sender<DriverResponse>,
}

/// Pending execute waiter: reply channel for the query tab.
pub struct ExecuteWaiter {
    pub reply: oneshot::Sender<DriverResponse>,
}

impl std::fmt::Debug for ExecuteWaiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecuteWaiter").finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Result data extraction
// ---------------------------------------------------------------------------

/// Convert a `QueryResponse` into driver-friendly JSON rows.
pub fn query_response_to_data(response: &QueryResponse) -> DriverData {
    let columns: Vec<String> = response
        .result
        .columns
        .iter()
        .map(|c| c.name.clone())
        .collect();
    let rows: Vec<Vec<serde_json::Value>> = response
        .result
        .rows
        .iter()
        .map(|row| row.iter().map(value_to_json).collect())
        .collect();
    DriverData {
        row_count: Some(rows.len()),
        columns: Some(columns),
        rows: Some(rows),
        ..DriverData::default()
    }
}

fn value_to_json(v: &trawl_engine::value::Value) -> serde_json::Value {
    match v {
        trawl_engine::value::Value::Null => serde_json::Value::Null,
        trawl_engine::value::Value::Boolean(b) => serde_json::Value::Bool(*b),
        trawl_engine::value::Value::Integer(i) => serde_json::json!(i),
        trawl_engine::value::Value::UInt(u) => serde_json::json!(u),
        trawl_engine::value::Value::Float(f) => serde_json::json!(f),
        trawl_engine::value::Value::String(s) => serde_json::Value::String(s.clone()),
        trawl_engine::value::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(value_to_json).collect())
        }
    }
}

// ---------------------------------------------------------------------------
// Socket listener
// ---------------------------------------------------------------------------

/// Default socket path: `~/.config/trawl/driver.sock`.
pub fn default_socket_path() -> PathBuf {
    let config_dir = shellexpand::tilde("~/.config/trawl");
    PathBuf::from(config_dir.as_ref()).join("driver.sock")
}

// ---------------------------------------------------------------------------
// Client (used by `trawl driver` subcommands)
// ---------------------------------------------------------------------------

/// Connect to a running TUI's driver socket, send a request, and return the
/// response. Short-lived: opens one connection per invocation.
pub async fn send_command(
    socket_path: &Path,
    request: &DriverRequest,
) -> Result<DriverResponse, std::io::Error> {
    let stream = tokio::net::UnixStream::connect(socket_path).await?;
    let (reader, mut writer) = stream.into_split();

    let mut json = serde_json::to_string(request)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    json.push('\n');
    writer.write_all(json.as_bytes()).await?;
    writer.flush().await?;

    let mut lines = BufReader::new(reader).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no response"))?;

    serde_json::from_str(&line).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Remove stale socket file if it exists.
fn cleanup_socket(path: &Path) {
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
}

/// Spawn the unix socket listener task. Returns a receiver for driver commands.
pub fn spawn_listener(
    path: &Path,
) -> Result<mpsc::UnboundedReceiver<DriverCommand>, std::io::Error> {
    cleanup_socket(path);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(path)?;

    // Set socket permissions to owner-only (0o600).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }

    tracing::info!("driver socket listening at {}", path.display());

    let (tx, rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    tracing::info!("driver client connected");
                    handle_client(stream, &tx).await;
                    tracing::info!("driver client disconnected");
                }
                Err(e) => {
                    tracing::error!("driver socket accept error: {e}");
                    break;
                }
            }
        }
    });

    Ok(rx)
}

/// Handle a single client connection (runs inline — one client at a time).
async fn handle_client(stream: tokio::net::UnixStream, tx: &mpsc::UnboundedSender<DriverCommand>) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_owned();
        if line.is_empty() {
            continue;
        }

        let request: DriverRequest = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(e) => {
                let resp = DriverResponse::err(format!("invalid request: {e}"));
                let _ = write_response(&mut writer, &resp).await;
                continue;
            }
        };

        let is_quit = matches!(request, DriverRequest::Quit);

        let timeout_ms = match &request {
            DriverRequest::Execute { timeout_ms, .. } => *timeout_ms,
            _ => 30_000, // 30s default for non-execute commands
        };

        let (reply_tx, reply_rx) = oneshot::channel();
        let cmd = DriverCommand {
            request,
            reply: reply_tx,
        };

        if tx.send(cmd).is_err() {
            let resp = DriverResponse::err("TUI event loop is gone");
            let _ = write_response(&mut writer, &resp).await;
            break;
        }

        let resp = match tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            reply_rx,
        )
        .await
        {
            Ok(Ok(resp)) => resp,
            Ok(Err(_)) => DriverResponse::err("event loop dropped reply channel"),
            Err(_) => DriverResponse::err("timeout waiting for response"),
        };

        let _ = write_response(&mut writer, &resp).await;

        if is_quit {
            break;
        }
    }
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    resp: &DriverResponse,
) -> Result<(), std::io::Error> {
    let mut json = serde_json::to_string(resp)
        .unwrap_or_else(|e| format!(r#"{{"ok":false,"error":"serialize error: {e}"}}"#));
    json.push('\n');
    writer.write_all(json.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Key string parser
// ---------------------------------------------------------------------------

/// Parse a human-readable key string (e.g. `"ctrl+enter"`, `"F5"`, `"a"`)
/// into a crossterm `KeyEvent`.
pub fn parse_key_string(s: &str) -> Result<KeyEvent, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty key string".to_owned());
    }

    let parts: Vec<&str> = s.split('+').collect();
    let mut modifiers = KeyModifiers::empty();
    let key_name = parts.last().ok_or_else(|| "empty key string".to_owned())?;

    // Parse modifier prefixes (everything before the last part).
    for &part in &parts[..parts.len() - 1] {
        match part.to_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
            "alt" | "meta" | "option" => modifiers |= KeyModifiers::ALT,
            "shift" => modifiers |= KeyModifiers::SHIFT,
            other => return Err(format!("unknown modifier: {other}")),
        }
    }

    // Single character keys preserve their original case.
    if key_name.len() == 1 {
        let ch = key_name.chars().next().unwrap();
        return Ok(KeyEvent::new(KeyCode::Char(ch), modifiers));
    }

    let code = match key_name.to_lowercase().as_str() {
        "enter" | "return" | "cr" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pgdown" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        "f1" => KeyCode::F(1),
        "f2" => KeyCode::F(2),
        "f3" => KeyCode::F(3),
        "f4" => KeyCode::F(4),
        "f5" => KeyCode::F(5),
        "f6" => KeyCode::F(6),
        "f7" => KeyCode::F(7),
        "f8" => KeyCode::F(8),
        "f9" => KeyCode::F(9),
        "f10" => KeyCode::F(10),
        "f11" => KeyCode::F(11),
        "f12" => KeyCode::F(12),
        _ => return Err(format!("unknown key: {key_name}")),
    };

    Ok(KeyEvent::new(code, modifiers))
}

// ---------------------------------------------------------------------------
// App integration (split impl — methods that handle driver commands from
// within the TUI event loop)
// ---------------------------------------------------------------------------

use ratatui::Terminal;

use super::App;
use super::state::TabStatus;
use super::ui;

impl App {
    /// Start the driver socket listener if a path is provided.
    pub(crate) fn start_driver(&mut self, path: &Path) {
        match spawn_listener(path) {
            Ok(rx) => {
                self.driver_rx = Some(rx);
                self.driver_socket_path = Some(path.to_owned());
                tracing::info!("driver started at {}", path.display());
            }
            Err(e) => {
                tracing::error!("failed to start driver socket: {e}");
            }
        }
    }

    /// Clean up the driver socket file.
    pub(crate) fn cleanup_driver(&mut self) {
        if let Some(ref path) = self.driver_socket_path {
            let _ = std::fs::remove_file(path);
            tracing::info!("cleaned up driver socket at {}", path.display());
        }
    }

    /// Process pending driver commands (up to 10 per tick to avoid starving UI).
    pub(crate) fn poll_driver_commands<B: ratatui::backend::Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
    ) {
        // Take the receiver out to avoid borrow conflicts with &mut self.
        let Some(mut rx) = self.driver_rx.take() else {
            return;
        };

        for _ in 0..10 {
            let Ok(cmd) = rx.try_recv() else { break };

            tracing::debug!("processing driver command: {:?}", cmd.request);

            match cmd.request {
                DriverRequest::Status => {
                    let resp = self.handle_driver_status();
                    let _ = cmd.reply.send(resp);
                }
                DriverRequest::SetQuery { query } => {
                    self.handle_driver_set_query(&query);
                    let _ = cmd.reply.send(DriverResponse::ok());
                }
                DriverRequest::Execute { .. } => {
                    self.handle_driver_execute(cmd.reply);
                    // Don't send reply here — it's deferred until query completes.
                }
                DriverRequest::Capture { width, height } => {
                    let resp = self.handle_driver_capture(terminal, width, height);
                    let _ = cmd.reply.send(resp);
                }
                DriverRequest::Key { key } => {
                    let resp = self.handle_driver_key(&key);
                    let _ = cmd.reply.send(resp);
                }
                DriverRequest::Keys { keys } => {
                    let resp = self.handle_driver_keys(&keys);
                    let _ = cmd.reply.send(resp);
                }
                DriverRequest::GetResults { tab } => {
                    let resp = self.handle_driver_get_results(tab);
                    let _ = cmd.reply.send(resp);
                }
                DriverRequest::Quit => {
                    let _ = cmd.reply.send(DriverResponse::ok());
                    self.should_quit = true;
                }
            }
        }

        self.driver_rx = Some(rx);
    }

    fn handle_driver_status(&self) -> DriverResponse {
        let tab = self.active_tab();
        let tab_status = match &tab.status {
            TabStatus::Idle => "idle",
            TabStatus::Running { .. } => "running",
            TabStatus::Success { .. } => "success",
            TabStatus::Error { .. } => "error",
        };
        let (result_rows, result_columns) = match &tab.result {
            Some(r) => (
                Some(r.result.row_count()),
                Some(r.result.columns.iter().map(|c| c.name.clone()).collect()),
            ),
            None => (None, None),
        };

        DriverResponse::ok_with(DriverData {
            focus: Some(format!("{:?}", self.focus).to_lowercase()),
            main_tab: Some(format!("{:?}", self.main_tab).to_lowercase()),
            tab_status: Some(tab_status.to_owned()),
            query: Some(tab.editor.text()),
            live_mode: Some(self.live_mode),
            result_rows,
            result_columns,
            ..DriverData::default()
        })
    }

    fn handle_driver_set_query(&mut self, query: &str) {
        let tab = self.active_tab_mut();
        tab.editor.clear();
        tab.editor.insert_text(query);
        tab.mark_editor_dirty();
    }

    fn handle_driver_execute(&mut self, reply: tokio::sync::oneshot::Sender<DriverResponse>) {
        if self.driver_execute_waiter.is_some() {
            let _ = reply.send(DriverResponse::err("another execute is already pending"));
            return;
        }

        if self.active_tab().editor.text().trim().is_empty() {
            let _ = reply.send(DriverResponse::err("empty query"));
            return;
        }

        self.driver_execute_waiter = Some(ExecuteWaiter { reply });

        // Trigger query execution (same as F5 / ctrl+enter).
        self.execute_query();
    }

    fn handle_driver_capture<B: ratatui::backend::Backend>(
        &mut self,
        _real_terminal: &mut Terminal<B>,
        width: Option<u16>,
        height: Option<u16>,
    ) -> DriverResponse {
        let w = width.unwrap_or(120);
        let h = height.unwrap_or(40);

        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut test_terminal = match Terminal::new(backend) {
            Ok(t) => t,
            Err(e) => return DriverResponse::err(format!("failed to create test terminal: {e}")),
        };

        if let Err(e) = test_terminal.draw(|f| ui::render(self, f)) {
            return DriverResponse::err(format!("render failed: {e}"));
        }

        let content = test_terminal.backend().to_string();

        DriverResponse::ok_with(DriverData {
            content: Some(content),
            width: Some(w),
            height: Some(h),
            ..DriverData::default()
        })
    }

    fn handle_driver_key(&mut self, key: &str) -> DriverResponse {
        match parse_key_string(key) {
            Ok(key_event) => {
                self.handle_key(key_event);
                DriverResponse::ok()
            }
            Err(e) => DriverResponse::err(e),
        }
    }

    fn handle_driver_keys(&mut self, keys: &[String]) -> DriverResponse {
        for key_str in keys {
            match parse_key_string(key_str) {
                Ok(key_event) => self.handle_key(key_event),
                Err(e) => return DriverResponse::err(format!("key '{key_str}': {e}")),
            }
        }
        DriverResponse::ok()
    }

    fn handle_driver_get_results(&self, _tab: Option<usize>) -> DriverResponse {
        match &self.tab.result {
            Some(response) => DriverResponse::ok_with(query_response_to_data(response)),
            None => DriverResponse::ok_with(DriverData {
                row_count: Some(0),
                columns: Some(Vec::new()),
                rows: Some(Vec::new()),
                ..DriverData::default()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_char() {
        let ev = parse_key_string("a").unwrap();
        assert_eq!(ev.code, KeyCode::Char('a'));
        assert_eq!(ev.modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn parse_uppercase_char() {
        let ev = parse_key_string("A").unwrap();
        assert_eq!(ev.code, KeyCode::Char('A'));
        assert_eq!(ev.modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn parse_ctrl_modifier() {
        let ev = parse_key_string("ctrl+c").unwrap();
        assert_eq!(ev.code, KeyCode::Char('c'));
        assert_eq!(ev.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn parse_ctrl_enter() {
        let ev = parse_key_string("ctrl+enter").unwrap();
        assert_eq!(ev.code, KeyCode::Enter);
        assert_eq!(ev.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn parse_alt_modifier() {
        let ev = parse_key_string("alt+1").unwrap();
        assert_eq!(ev.code, KeyCode::Char('1'));
        assert_eq!(ev.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn parse_shift_tab() {
        let ev = parse_key_string("shift+tab").unwrap();
        assert_eq!(ev.code, KeyCode::Tab);
        assert_eq!(ev.modifiers, KeyModifiers::SHIFT);
    }

    #[test]
    fn parse_f5() {
        let ev = parse_key_string("F5").unwrap();
        assert_eq!(ev.code, KeyCode::F(5));
        assert_eq!(ev.modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn parse_function_keys() {
        for n in 1..=12 {
            let ev = parse_key_string(&format!("f{n}")).unwrap();
            assert_eq!(ev.code, KeyCode::F(n));
        }
    }

    #[test]
    fn parse_enter() {
        let ev = parse_key_string("enter").unwrap();
        assert_eq!(ev.code, KeyCode::Enter);
    }

    #[test]
    fn parse_escape() {
        let ev = parse_key_string("esc").unwrap();
        assert_eq!(ev.code, KeyCode::Esc);
        let ev2 = parse_key_string("escape").unwrap();
        assert_eq!(ev2.code, KeyCode::Esc);
    }

    #[test]
    fn parse_space() {
        let ev = parse_key_string("space").unwrap();
        assert_eq!(ev.code, KeyCode::Char(' '));
    }

    #[test]
    fn parse_arrow_keys() {
        assert_eq!(parse_key_string("up").unwrap().code, KeyCode::Up);
        assert_eq!(parse_key_string("down").unwrap().code, KeyCode::Down);
        assert_eq!(parse_key_string("left").unwrap().code, KeyCode::Left);
        assert_eq!(parse_key_string("right").unwrap().code, KeyCode::Right);
    }

    #[test]
    fn parse_backspace_delete() {
        assert_eq!(
            parse_key_string("backspace").unwrap().code,
            KeyCode::Backspace
        );
        assert_eq!(parse_key_string("bs").unwrap().code, KeyCode::Backspace);
        assert_eq!(parse_key_string("delete").unwrap().code, KeyCode::Delete);
        assert_eq!(parse_key_string("del").unwrap().code, KeyCode::Delete);
    }

    #[test]
    fn parse_page_keys() {
        assert_eq!(parse_key_string("pageup").unwrap().code, KeyCode::PageUp);
        assert_eq!(
            parse_key_string("pagedown").unwrap().code,
            KeyCode::PageDown
        );
    }

    #[test]
    fn parse_home_end() {
        assert_eq!(parse_key_string("home").unwrap().code, KeyCode::Home);
        assert_eq!(parse_key_string("end").unwrap().code, KeyCode::End);
    }

    #[test]
    fn parse_case_insensitive() {
        let ev = parse_key_string("CTRL+ENTER").unwrap();
        assert_eq!(ev.code, KeyCode::Enter);
        assert_eq!(ev.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn parse_multiple_modifiers() {
        let ev = parse_key_string("ctrl+shift+a").unwrap();
        assert_eq!(ev.code, KeyCode::Char('a'));
        assert!(ev.modifiers.contains(KeyModifiers::CONTROL));
        assert!(ev.modifiers.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn parse_meta_alias() {
        let ev = parse_key_string("meta+x").unwrap();
        assert_eq!(ev.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn parse_option_alias() {
        let ev = parse_key_string("option+x").unwrap();
        assert_eq!(ev.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn parse_empty_is_error() {
        assert!(parse_key_string("").is_err());
    }

    #[test]
    fn parse_unknown_key_is_error() {
        assert!(parse_key_string("banana").is_err());
    }

    #[test]
    fn parse_unknown_modifier_is_error() {
        assert!(parse_key_string("super+a").is_err());
    }

    #[test]
    fn parse_backtab() {
        let ev = parse_key_string("backtab").unwrap();
        assert_eq!(ev.code, KeyCode::BackTab);
    }

    // -- request deserialization tests --

    #[test]
    fn deserialize_status() {
        let req: DriverRequest = serde_json::from_str(r#"{"cmd":"status"}"#).unwrap();
        assert!(matches!(req, DriverRequest::Status));
    }

    #[test]
    fn deserialize_set_query() {
        let req: DriverRequest =
            serde_json::from_str(r#"{"cmd":"set_query","query":"* | head 5"}"#).unwrap();
        assert!(matches!(req, DriverRequest::SetQuery { query } if query == "* | head 5"));
    }

    #[test]
    fn deserialize_execute_default_timeout() {
        let req: DriverRequest = serde_json::from_str(r#"{"cmd":"execute"}"#).unwrap();
        match req {
            DriverRequest::Execute { timeout_ms } => assert_eq!(timeout_ms, 300_000),
            _ => panic!("expected Execute"),
        }
    }

    #[test]
    fn deserialize_execute_custom_timeout() {
        let req: DriverRequest =
            serde_json::from_str(r#"{"cmd":"execute","timeout_ms":5000}"#).unwrap();
        match req {
            DriverRequest::Execute { timeout_ms } => assert_eq!(timeout_ms, 5000),
            _ => panic!("expected Execute"),
        }
    }

    #[test]
    fn deserialize_capture() {
        let req: DriverRequest =
            serde_json::from_str(r#"{"cmd":"capture","width":120,"height":40}"#).unwrap();
        assert!(matches!(
            req,
            DriverRequest::Capture {
                width: Some(120),
                height: Some(40)
            }
        ));
    }

    #[test]
    fn deserialize_key() {
        let req: DriverRequest =
            serde_json::from_str(r#"{"cmd":"key","key":"ctrl+enter"}"#).unwrap();
        assert!(matches!(req, DriverRequest::Key { key } if key == "ctrl+enter"));
    }

    #[test]
    fn deserialize_keys() {
        let req: DriverRequest =
            serde_json::from_str(r#"{"cmd":"keys","keys":["a","b","enter"]}"#).unwrap();
        assert!(matches!(req, DriverRequest::Keys { keys } if keys.len() == 3));
    }

    #[test]
    fn deserialize_get_results() {
        let req: DriverRequest = serde_json::from_str(r#"{"cmd":"get_results","tab":1}"#).unwrap();
        assert!(matches!(req, DriverRequest::GetResults { tab: Some(1) }));
    }

    #[test]
    fn deserialize_get_results_no_tab() {
        let req: DriverRequest = serde_json::from_str(r#"{"cmd":"get_results"}"#).unwrap();
        assert!(matches!(req, DriverRequest::GetResults { tab: None }));
    }

    #[test]
    fn deserialize_quit() {
        let req: DriverRequest = serde_json::from_str(r#"{"cmd":"quit"}"#).unwrap();
        assert!(matches!(req, DriverRequest::Quit));
    }

    #[test]
    fn deserialize_unknown_cmd_is_error() {
        let result = serde_json::from_str::<DriverRequest>(r#"{"cmd":"foobar"}"#);
        assert!(result.is_err());
    }

    // -- response serialization tests --

    #[test]
    fn serialize_ok_response() {
        let resp = DriverResponse::ok();
        let json: serde_json::Value = serde_json::to_value(resp).unwrap();
        assert_eq!(json["ok"], true);
        assert!(json.get("error").is_none());
    }

    #[test]
    fn serialize_error_response() {
        let resp = DriverResponse::err("something broke");
        let json: serde_json::Value = serde_json::to_value(resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"], "something broke");
    }

    #[test]
    fn serialize_ok_with_data_skips_none() {
        let resp = DriverResponse::ok_with(DriverData {
            focus: Some("editor".to_owned()),
            main_tab: Some("query".to_owned()),
            ..DriverData::default()
        });
        let json: serde_json::Value = serde_json::to_value(resp).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["focus"], "editor");
        assert_eq!(json["main_tab"], "query");
        assert!(json.get("query").is_none());
        assert!(json.get("content").is_none());
    }
}
