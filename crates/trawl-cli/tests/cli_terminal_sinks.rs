// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What sender-chosen text does on its way to an operator's terminal.
//!
//! A real trawld stores one event whose field names and values hold
//! terminal controls: ESC sequences (a screen clear, concealed text, an
//! OSC 52 clipboard write, a window title), the C1 CSI U+009B and the bidi
//! override U+202E. The real `trawl` binary reads it back from an empty
//! environment (only the dynamic loader's search path survives) with a
//! temp `HOME` and XDG dirs, the fixture's config and the fixture's token,
//! so no saved profile or `TRAWL_*` variable can address another server.
//!
//! The terminal is a PTY: `trawl` sees a TTY on stdout and stderr, and the
//! test reads the master until the last slave end closes, then reaps the
//! child. Nothing sleeps or polls; the timeout is a hang guard only.
//! Captured text reaches an assertion message escaped, so a failing run
//! cannot drive the terminal that shows it.

mod common;

use std::io::{self, Read as _};
use std::path::PathBuf;
use std::process::{ExitStatus, Output, Stdio};
use std::time::Duration;

use nix::errno::Errno;
use nix::pty::Winsize;
use serde_json::{Map, Value};
use trawl_api::IngestResponse;

use common::Server;
use common::mcp::loader_path;

/// How long one `trawl` run may take.
const DEADLINE: Duration = Duration::from_secs(60);

/// The service the event arrives under, and the query that finds it. A
/// bare search projects every column, so the hostile names are headers.
const SERVICE: &str = "term_probe";
const QUERY: &str = "service=term_probe";

/// The ordinary field that carries the hostile value. The value holds
/// plain text around an OSC 52 clipboard write, a screen clear, a C1 CSI
/// and a bidi override.
const VALUE_FIELD: &str = "term_payload";
const HOSTILE_VALUE: &str = "pre\u{1b}]52;c;aGk=\u{7}mid\u{1b}[2J\u{9b}31m\u{202e}post";

/// A field name a sender chose, as sent and as the catalog stores it.
struct HostileName {
    sent: &'static str,
    stored: &'static str,
}

/// Ingest folds a name's ASCII uppercase to lowercase and keeps every
/// other character, controls included.
const HOSTILE_NAMES: [HostileName; 4] = [
    HostileName {
        sent: "esc\u{1b}[2Jname",
        stored: "esc\u{1b}[2jname",
    },
    HostileName {
        sent: "bidi\u{202e}name",
        stored: "bidi\u{202e}name",
    },
    HostileName {
        sent: "esc\u{1b}[8mname",
        stored: "esc\u{1b}[8mname",
    },
    HostileName {
        sent: "osc\u{1b}]0;x\u{7}name",
        stored: "osc\u{1b}]0;x\u{7}name",
    },
];

/// The value of every hostile-named field.
const NAMED_VALUE: &str = "named field";

/// The hostile name `trawl schema field` looks up.
const LOOKED_UP: &str = HOSTILE_NAMES[2].stored;

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// The `trawl` binary, configured for one fixture server.
struct Client {
    home: tempfile::TempDir,
    config: PathBuf,
    token: String,
}

impl Client {
    fn new(server: &Server) -> Self {
        let home = tempfile::tempdir().expect("tempdir");
        let config = server.config(home.path());
        Self {
            home,
            config,
            token: server.agent_token.clone(),
        }
    }

    /// `trawl --config <config> args...` with stdin on `/dev/null`.
    fn command(&self, args: &[&str]) -> std::process::Command {
        let home = self.home.path();
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_trawl"));
        cmd.env_clear()
            .envs(loader_path())
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_STATE_HOME", home.join(".state"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("TERM", "xterm-256color")
            .env("TRAWL_TOKEN", &self.token)
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .stdin(Stdio::null());
        cmd
    }

    /// `args` with stdout and stderr on pipes, to completion.
    async fn piped(&self, args: &[&str]) -> Output {
        let mut cmd = tokio::process::Command::from(self.command(args));
        cmd.kill_on_drop(true);
        tokio::time::timeout(DEADLINE, cmd.output())
            .await
            .unwrap_or_else(|_| panic!("trawl {args:?} did not finish within {DEADLINE:?}"))
            .expect("spawn trawl")
    }

    /// `args` with stdout and stderr on a PTY; the terminal's text once
    /// the run succeeded.
    async fn on_pty(&self, args: &[&str]) -> String {
        let cmd = self.command(args);
        let (status, text) =
            tokio::time::timeout(DEADLINE, tokio::task::spawn_blocking(move || on_pty(cmd)))
                .await
                .unwrap_or_else(|_| {
                    panic!("trawl {args:?} on a PTY did not finish within {DEADLINE:?}")
                })
                .expect("the PTY task");
        assert!(
            status.success(),
            "trawl {args:?} on a PTY exited {status}:\n{}",
            text.escape_debug()
        );
        text
    }

    async fn query_on_pty(&self, format: &str) -> String {
        self.on_pty(&["query", QUERY, "-f", format]).await
    }

    async fn schema_fields_on_pty(&self) -> String {
        self.on_pty(&["schema", "fields"]).await
    }

    async fn schema_field_on_pty(&self, name: &str) -> String {
        self.on_pty(&["schema", "field", name]).await
    }
}

/// Run `cmd` with stdout and stderr on the slave side of a fresh PTY, read
/// the master to EOF, then reap the child. Returns the exit status and the
/// output, with the `\r\n` the line discipline's ONLCR writes for each
/// `\n` folded back.
fn on_pty(mut cmd: std::process::Command) -> (ExitStatus, String) {
    let size = Winsize {
        ws_row: 50,
        ws_col: 400,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = nix::pty::openpty(Some(&size), None).expect("open a PTY");
    // openpty's descriptors are inheritable and their duplicates are
    // close-on-exec, so the child gets the slave as stdout and stderr and
    // no other end of the PTY.
    let mut master = std::fs::File::from(pty.master.try_clone().expect("dup the master"));
    cmd.stdout(Stdio::from(pty.slave.try_clone().expect("dup the slave")))
        .stderr(Stdio::from(pty.slave.try_clone().expect("dup the slave")));
    drop(pty);
    let mut child = cmd.spawn().expect("spawn trawl");
    // The command holds the parent's slave ends; the master reads EOF only
    // once the child's are the last ones.
    drop(cmd);
    let mut bytes = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match master.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // Linux answers EIO once the last slave end has closed.
            Err(e) if e.raw_os_error() == Some(Errno::EIO as i32) => break,
            Err(e) => panic!("read the PTY master: {e}"),
        }
    }
    let status = child.wait().expect("reap trawl");
    let text = String::from_utf8(bytes).unwrap_or_else(|e| {
        panic!(
            "trawl wrote non-UTF-8 to its terminal: {}",
            String::from_utf8_lossy(e.as_bytes()).escape_debug()
        )
    });
    (status, text.replace("\r\n", "\n"))
}

fn describe(output: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).escape_debug(),
        String::from_utf8_lossy(&output.stderr).escape_debug()
    )
}

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

fn hostile_event() -> Value {
    let mut event = Map::new();
    event.insert("service".into(), SERVICE.into());
    event.insert(VALUE_FIELD.into(), HOSTILE_VALUE.into());
    for name in &HOSTILE_NAMES {
        event.insert(name.sent.into(), NAMED_VALUE.into());
    }
    Value::Object(event)
}

/// Each line of an NDJSON stdout as a JSON object.
fn ndjson_rows(stdout: &[u8]) -> Vec<Map<String, Value>> {
    std::str::from_utf8(stdout)
        .expect("utf-8 stdout")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("not a JSON object ({e}): {}", line.escape_debug()))
        })
        .collect()
}

/// RFC 4180 records: a quoted cell may hold commas, doubled quotes and
/// line breaks.
fn csv_records(text: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                chars.next();
                cell.push('"');
            }
            (true, '"') => quoted = false,
            (false, '"') => quoted = true,
            (false, ',') => record.push(std::mem::take(&mut cell)),
            (false, '\n') => {
                record.push(std::mem::take(&mut cell));
                records.push(std::mem::take(&mut record));
            }
            (_, c) => cell.push(c),
        }
    }
    if !cell.is_empty() || !record.is_empty() {
        record.push(cell);
        records.push(record);
    }
    records
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/// The catalog lists every hostile name with its controls in place, read
/// through a pipe.
async fn assert_names_stored(client: &Client, ingest: &IngestResponse) {
    let output = client.piped(&["schema", "fields", "-f", "json"]).await;
    assert!(output.status.success(), "{}", describe(&output));
    let listed: Vec<String> = ndjson_rows(&output.stdout)
        .iter()
        .map(|row| {
            row.get("field")
                .and_then(Value::as_str)
                .expect("a field name")
                .to_owned()
        })
        .collect();
    let lost: Vec<(&str, &str)> = HOSTILE_NAMES
        .iter()
        .filter(|name| !listed.iter().any(|listed| listed == name.stored))
        .map(|name| (name.sent, name.stored))
        .collect();
    assert!(
        lost.is_empty(),
        "the catalog does not list these (sent, expected) names: {lost:?}\n\
         ingest answered {ingest:?}\nthe catalog listed {listed:?}"
    );
}

/// In a pipe, `-f json` decodes to the stored names and the sent values.
async fn assert_pipe_json_exact(client: &Client) {
    let output = client.piped(&["query", QUERY, "-f", "json"]).await;
    assert!(output.status.success(), "{}", describe(&output));
    let rows = ndjson_rows(&output.stdout);
    assert_eq!(rows.len(), 1, "{}", describe(&output));
    let row = &rows[0];
    assert_eq!(
        row.get(VALUE_FIELD).and_then(Value::as_str),
        Some(HOSTILE_VALUE),
        "{row:?}"
    );
    for name in &HOSTILE_NAMES {
        assert_eq!(
            row.get(name.stored).and_then(Value::as_str),
            Some(NAMED_VALUE),
            "key {:?} in {row:?}",
            name.stored
        );
    }
}

/// `-f csv -o <file>` with stdout on a PTY writes the hostile names and
/// value bare: none holds a character CSV quotes, and none starts with a
/// formula trigger.
async fn assert_csv_file_exact(client: &Client) {
    let path = client.home.path().join("out.csv");
    let path_arg = path.to_str().expect("utf-8 path");
    client
        .on_pty(&["query", QUERY, "-f", "csv", "-o", path_arg])
        .await;
    let text =
        String::from_utf8(std::fs::read(&path).expect("read the CSV file")).expect("utf-8 CSV");
    assert!(text.contains(HOSTILE_VALUE), "{}", text.escape_debug());
    let records = csv_records(&text);
    assert_eq!(records.len(), 2, "{}", text.escape_debug());
    let (header, row) = (&records[0], &records[1]);
    let at = |name: &str| {
        header
            .iter()
            .position(|column| column == name)
            .unwrap_or_else(|| panic!("no {name:?} column in {header:?}"))
    };
    assert_eq!(row[at(VALUE_FIELD)], HOSTILE_VALUE, "{row:?}");
    for name in &HOSTILE_NAMES {
        assert_eq!(row[at(name.stored)], NAMED_VALUE, "{row:?}");
    }
}

fn assert_mentions(what: &str, text: &str, marker: &str) {
    assert!(
        text.contains(marker),
        "{what} on a PTY does not mention {marker:?}:\n{}",
        text.escape_debug()
    );
}

// ---------------------------------------------------------------------------
// Case
// ---------------------------------------------------------------------------

/// A sender's hostile field names and value reach the catalog, the data
/// formats keep them exact in a pipe and in a file, and every terminal
/// command that shows them runs to success on a PTY.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_terminal_sinks_reach_no_terminal_control() {
    let server = Server::start(true).await;
    let ingest = server
        .client(&server.ingest_token)
        .ingest(&[hostile_event()])
        .await
        .unwrap_or_else(|e| panic!("ingest refused the hostile event: {e:?}"));
    assert_eq!(ingest.accepted, 1, "{ingest:?}");
    server.compact().await;
    let client = Client::new(&server);

    assert_names_stored(&client, &ingest).await;
    assert_pipe_json_exact(&client).await;
    assert_csv_file_exact(&client).await;

    let table = client.query_on_pty("table").await;
    assert_mentions("query -f table", &table, "1 row(s)");
    let json = client.query_on_pty("json").await;
    assert_mentions("query -f json", &json, SERVICE);
    let csv = client.query_on_pty("csv").await;
    assert_mentions("query -f csv", &csv, SERVICE);
    let fields = client.schema_fields_on_pty().await;
    assert_mentions("schema fields", &fields, "pins used");
    let field = client.schema_field_on_pty(LOOKED_UP).await;
    assert_mentions("schema field", &field, SERVICE);

    server.stop().await;
}
