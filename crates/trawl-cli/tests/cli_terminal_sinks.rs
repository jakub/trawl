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
//! The terminal is a PTY: `trawl` sees a TTY on stderr, and on stdout
//! unless the case pipes it. The test reads the master until the last
//! slave end closes, drains a piped stdout on its own thread meanwhile,
//! then reaps the child. Nothing sleeps or polls; the timeout is a hang
//! guard only. Captured text reaches an assertion message escaped, so a
//! failing run cannot drive the terminal that shows it.

mod common;

use std::io::{self, Read as _};
use std::path::PathBuf;
use std::process::{ExitStatus, Output, Stdio};
use std::time::Duration;

use nix::errno::Errno;
use nix::pty::Winsize;
use serde_json::{Map, Value};
use tokio::io::AsyncReadExt as _;
use trawl_api::IngestResponse;
use trawl_core::sanitize::is_unsafe_display_char;

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

/// [`HOSTILE_VALUE`] as a table cell and a terminal csv field show it:
/// U+FFFD for each control and the bidi override, one for one.
const HOSTILE_VALUE_SHOWN: &str =
    "pre\u{fffd}]52;c;aGk=\u{fffd}mid\u{fffd}[2J\u{fffd}31m\u{fffd}post";

/// A field name a sender chose: as sent, as the catalog stores it, and as
/// a terminal shows it.
#[derive(Clone, Copy)]
struct HostileName {
    sent: &'static str,
    stored: &'static str,
    shown: &'static str,
}

/// Ingest folds a name's ASCII uppercase to lowercase and keeps every
/// other character, controls included.
const HOSTILE_NAMES: [HostileName; 4] = [
    HostileName {
        sent: "esc\u{1b}[2Jname",
        stored: "esc\u{1b}[2jname",
        shown: "esc\u{fffd}[2jname",
    },
    HostileName {
        sent: "bidi\u{202e}name",
        stored: "bidi\u{202e}name",
        shown: "bidi\u{fffd}name",
    },
    HostileName {
        sent: "esc\u{1b}[8mname",
        stored: "esc\u{1b}[8mname",
        shown: "esc\u{fffd}[8mname",
    },
    HostileName {
        sent: "osc\u{1b}]0;x\u{7}name",
        stored: "osc\u{1b}]0;x\u{7}name",
        shown: "osc\u{fffd}]0;x\u{fffd}name",
    },
];

/// The value of every hostile-named field.
const NAMED_VALUE: &str = "named field";

/// The hostile name `trawl schema field` looks up.
const LOOKED_UP: HostileName = HOSTILE_NAMES[2];

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

    /// `args` with stderr on a PTY and stdout where `stdout` says, to a
    /// successful exit.
    async fn run_on_pty(&self, args: &[&str], stdout: StdoutTo) -> PtyRun {
        let run = pty_run(self.command(args), stdout, DEADLINE)
            .await
            .unwrap_or_else(|| {
                panic!("trawl {args:?} on a PTY did not finish within {DEADLINE:?}")
            });
        assert!(
            run.status.success(),
            "trawl {args:?} on a PTY exited {}:\nterminal:\n{}\npiped stdout:\n{}",
            run.status,
            run.terminal.escape_debug(),
            String::from_utf8_lossy(&run.stdout).escape_debug()
        );
        run
    }

    /// `args` with stdout and stderr on a PTY; the terminal's text once
    /// the run succeeded.
    async fn on_pty(&self, args: &[&str]) -> String {
        self.run_on_pty(args, StdoutTo::Pty).await.terminal
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

    /// `schema field <name> -f json` with stdout on a pipe, so its labels
    /// go to stderr, which stays on the PTY.
    async fn schema_field_json_labels_on_pty(&self, name: &str) -> PtyRun {
        self.run_on_pty(&["schema", "field", name, "-f", "json"], StdoutTo::Pipe)
            .await
    }
}

/// Where a PTY run sends the child's stdout. Its stderr is always the PTY.
#[derive(Clone, Copy)]
enum StdoutTo {
    Pty,
    Pipe,
}

/// A finished PTY run.
struct PtyRun {
    status: ExitStatus,
    /// What the PTY showed, with the `\r\n` the line discipline's ONLCR
    /// writes for each `\n` folded back.
    terminal: String,
    /// A piped stdout's bytes; empty when stdout was the PTY.
    stdout: Vec<u8>,
}

/// Run `cmd` on a PTY to its exit. `None` when the child is still running
/// at `deadline`, whether or not it still holds the terminal: it is then
/// killed and reaped first.
async fn pty_run(
    cmd: std::process::Command,
    stdout: StdoutTo,
    deadline: Duration,
) -> Option<PtyRun> {
    let (mut child, master) = spawn_on_pty(cmd, stdout);
    let mut piped = child.stdout.take();
    let mut terminal = tokio::task::spawn_blocking(move || read_master(master));
    let finished = tokio::time::timeout(deadline, async {
        let read_piped = async {
            let mut bytes = Vec::new();
            if let Some(pipe) = piped.as_mut() {
                pipe.read_to_end(&mut bytes).await.map(|_| bytes)
            } else {
                Ok(bytes)
            }
        };
        // Both streams drain while the child runs, so neither can fill
        // and stall it, and the child is reaped whatever the reads answer.
        tokio::join!(&mut terminal, read_piped, child.wait())
    })
    .await;
    let Ok((terminal, stdout, status)) = finished else {
        child.kill().await.expect("kill and reap the child");
        // The kill closed the child's slave ends, which ends the read.
        if !terminal.is_finished() {
            let _ = terminal.await;
        }
        return None;
    };
    let status = status.expect("reap the child");
    let bytes = terminal.expect("the PTY reader");
    let text = String::from_utf8(bytes).unwrap_or_else(|e| {
        panic!(
            "the child wrote non-UTF-8 to its terminal: {}",
            String::from_utf8_lossy(e.as_bytes()).escape_debug()
        )
    });
    Some(PtyRun {
        status,
        terminal: text.replace("\r\n", "\n"),
        stdout: stdout.expect("read the child's stdout"),
    })
}

/// Spawn `cmd` with stderr, and stdout unless `stdout` pipes it, on the
/// slave side of a fresh PTY. Returns the child and the master side.
fn spawn_on_pty(
    mut cmd: std::process::Command,
    stdout: StdoutTo,
) -> (tokio::process::Child, std::fs::File) {
    let size = Winsize {
        ws_row: 50,
        ws_col: 400,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = nix::pty::openpty(Some(&size), None).expect("open a PTY");
    // openpty's descriptors are inheritable and their duplicates are
    // close-on-exec, so the child gets the slave as its terminal streams
    // and no other end of the PTY.
    let master = std::fs::File::from(pty.master.try_clone().expect("dup the master"));
    let slave = || Stdio::from(pty.slave.try_clone().expect("dup the slave"));
    match stdout {
        StdoutTo::Pty => cmd.stdout(slave()),
        StdoutTo::Pipe => cmd.stdout(Stdio::piped()),
    };
    cmd.stderr(slave());
    drop(pty);
    let mut cmd = tokio::process::Command::from(cmd);
    cmd.kill_on_drop(true);
    let child = cmd.spawn().expect("spawn the child");
    // The command holds the parent's slave ends; the master reads EOF only
    // once the child's are the last ones.
    drop(cmd);
    (child, master)
}

/// Read the PTY master to EOF.
fn read_master(mut master: std::fs::File) -> Vec<u8> {
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
    bytes
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

/// The cells of each row of a drawn table, the header first.
///
/// A row is the run of lines that open with the table's left border, and
/// a cell is the text between two vertical lines. comfy-table sizes the
/// table to the terminal it finds, which is the developer's own when the
/// test runs from one, so a cell may wrap: its pieces are joined without
/// their padding. That restores any text without spaces, which is all
/// this test compares.
fn table_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let Some(inner) = line
            .strip_prefix('│')
            .and_then(|line| line.strip_suffix('│'))
        else {
            if !row.is_empty() {
                rows.push(std::mem::take(&mut row));
            }
            continue;
        };
        for (i, piece) in inner.split(['│', '┆']).enumerate() {
            if i == row.len() {
                row.push(String::new());
            }
            row[i].push_str(piece.trim());
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

/// The index of the column `header` names.
fn column(header: &[String], name: &str) -> usize {
    header
        .iter()
        .position(|column| column == name)
        .unwrap_or_else(|| panic!("no {name:?} column in {header:?}"))
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
/// Returns its rows.
async fn assert_pipe_json_exact(client: &Client) -> Vec<Map<String, Value>> {
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
    rows
}

/// `-f csv -o <file>` with stdout on a PTY writes the hostile names and
/// value bare: none holds a character CSV quotes, and none starts with a
/// formula trigger. The file is data, and the terminal gets no control.
async fn assert_csv_file_exact(client: &Client) {
    let path = client.home.path().join("out.csv");
    let path_arg = path.to_str().expect("utf-8 path");
    let terminal = client
        .on_pty(&["query", QUERY, "-f", "csv", "-o", path_arg])
        .await;
    assert_no_terminal_control("query -f csv -o", &terminal);
    let text =
        String::from_utf8(std::fs::read(&path).expect("read the CSV file")).expect("utf-8 CSV");
    assert!(text.contains(HOSTILE_VALUE), "{}", text.escape_debug());
    let records = csv_records(&text);
    assert_eq!(records.len(), 2, "{}", text.escape_debug());
    let (header, row) = (&records[0], &records[1]);
    assert_eq!(row[column(header, VALUE_FIELD)], HOSTILE_VALUE, "{row:?}");
    for name in &HOSTILE_NAMES {
        assert_eq!(row[column(header, name.stored)], NAMED_VALUE, "{row:?}");
    }
}

fn assert_mentions(what: &str, text: &str, marker: &str) {
    assert!(
        text.contains(marker),
        "{what} on a PTY does not mention {marker:?}:\n{}",
        text.escape_debug()
    );
}

/// Whether `c` can drive or rewrite a terminal: every C0 and C1 control
/// (ESC, BEL and the C1 CSI among them), the bidi override, and the rest
/// of what the sanitize rule names. The first two are spelled out so the
/// check does not rest on the rule it checks.
fn is_terminal_control(c: char) -> bool {
    c.is_control() || c == '\u{202e}' || is_unsafe_display_char(c)
}

/// `text`, as a PTY showed it, holds no terminal control but the line
/// breaks the command writes as framing: table and prose lines, csv and
/// ndjson record ends. It is read as characters, because 0x9B, the C1 CSI
/// as a byte, is also a UTF-8 continuation byte.
fn assert_no_terminal_control(what: &str, text: &str) {
    let found: Vec<String> = text
        .split('\n')
        .enumerate()
        .flat_map(|(n, line)| {
            line.chars()
                .filter(|&c| is_terminal_control(c))
                .map(move |c| format!("{c:?} on line {}: {}", n + 1, line.escape_debug()))
        })
        .collect();
    assert!(
        found.is_empty(),
        "{what} on a PTY wrote terminal control:\n{}",
        found.join("\n")
    );
}

/// The terminal shows `name` through the `schema field` label, U+FFFD in
/// place of its controls.
fn assert_field_label(what: &str, text: &str, name: HostileName) {
    assert!(
        text.split('\n').any(|line| line
            .strip_prefix("field:")
            .is_some_and(|rest| rest.trim() == name.shown)),
        "{what} on a PTY has no `field:` label showing {:?}:\n{}",
        name.shown,
        text.escape_debug()
    );
}

/// The table holds U+FFFD for each control: in the header, every hostile
/// name, and in the payload cell, the value.
async fn assert_terminal_table(client: &Client) {
    let text = client.query_on_pty("table").await;
    assert_no_terminal_control("query -f table", &text);
    assert_mentions("query -f table", &text, "1 row(s)");
    let rows = table_rows(&text);
    let [header, row] = rows.as_slice() else {
        panic!(
            "query -f table on a PTY drew {} rows, not a header and one row:\n{}",
            rows.len(),
            text.escape_debug()
        );
    };
    assert_eq!(
        row[column(header, VALUE_FIELD)],
        HOSTILE_VALUE_SHOWN,
        "{row:?}"
    );
    for name in &HOSTILE_NAMES {
        assert!(
            header.iter().any(|column| column == name.shown),
            "query -f table on a PTY has no {:?} column: {header:?}",
            name.shown
        );
    }
}

/// Terminal json escapes each control instead of replacing it, so every
/// line decodes to the piped rows: the stored names as keys, the sent value
/// unchanged.
async fn assert_terminal_json(client: &Client, piped: &[Map<String, Value>]) {
    let text = client.query_on_pty("json").await;
    assert_no_terminal_control("query -f json", &text);
    for escape in [r"\u001b", r"\u0007", r"\u009b", r"\u202e"] {
        assert_mentions("query -f json", &text, escape);
    }
    assert_eq!(
        ndjson_rows(text.as_bytes()),
        piped,
        "query -f json on a PTY decodes to other rows than in a pipe"
    );
}

/// Terminal csv holds U+FFFD for each control, in the header names and the
/// payload field.
async fn assert_terminal_csv(client: &Client) {
    let text = client.query_on_pty("csv").await;
    assert_no_terminal_control("query -f csv", &text);
    let records = csv_records(&text);
    assert_eq!(records.len(), 2, "{}", text.escape_debug());
    let (header, row) = (&records[0], &records[1]);
    assert_eq!(row[column(header, "service")], SERVICE, "{row:?}");
    assert_eq!(
        row[column(header, VALUE_FIELD)],
        HOSTILE_VALUE_SHOWN,
        "{row:?}"
    );
    for name in &HOSTILE_NAMES {
        assert_eq!(row[column(header, name.shown)], NAMED_VALUE, "{row:?}");
    }
}

/// The `schema fields` table lists every hostile name with U+FFFD for each
/// control.
async fn assert_terminal_schema_fields(client: &Client) {
    let text = client.schema_fields_on_pty().await;
    assert_no_terminal_control("schema fields", &text);
    assert_mentions("schema fields", &text, "pins used");
    let rows = table_rows(&text);
    let (header, rows) = rows.split_first().unwrap_or_else(|| {
        panic!(
            "schema fields on a PTY drew no table:\n{}",
            text.escape_debug()
        )
    });
    let at = column(header, "field");
    let listed: Vec<&str> = rows.iter().map(|row| row[at].as_str()).collect();
    for name in &HOSTILE_NAMES {
        assert!(
            listed.contains(&name.shown),
            "schema fields on a PTY does not list {:?}: {listed:?}",
            name.shown
        );
    }
    assert!(listed.contains(&VALUE_FIELD), "{listed:?}");
}

/// `schema field <name>` shows the name in its label on stdout and lists
/// the service that sent it.
async fn assert_terminal_schema_field(client: &Client) {
    let text = client.schema_field_on_pty(LOOKED_UP.stored).await;
    assert_no_terminal_control("schema field", &text);
    assert_field_label("schema field", &text, LOOKED_UP);
    let rows = table_rows(&text);
    let (header, rows) = rows.split_first().unwrap_or_else(|| {
        panic!(
            "schema field on a PTY drew no services table:\n{}",
            text.escape_debug()
        )
    });
    let at = column(header, "service");
    assert!(
        rows.iter().any(|row| row[at] == SERVICE),
        "schema field on a PTY does not list service {SERVICE:?}: {rows:?}"
    );
}

/// With `-f json` and stdout on a pipe, `schema field` writes its labels to
/// stderr, which stays on the PTY: the name shows there with U+FFFD for
/// each control, and the services go to the pipe.
async fn assert_schema_labels_on_stderr(client: &Client) {
    let run = client
        .schema_field_json_labels_on_pty(LOOKED_UP.stored)
        .await;
    let what = "schema field -f json's stderr";
    assert_no_terminal_control(what, &run.terminal);
    assert_field_label(what, &run.terminal, LOOKED_UP);
    let services = ndjson_rows(&run.stdout);
    assert!(
        services
            .iter()
            .any(|row| row.get("service").and_then(Value::as_str) == Some(SERVICE)),
        "schema field -f json piped no {SERVICE:?} row: {services:?}"
    );
}

/// A stored field name may start with `--`. clap refuses it as an option
/// and quotes it on stderr.
async fn assert_usage_error_on_stderr(client: &Client) {
    let option_shaped = "--x\u{202e}\u{9b}31m\u{1b}[2Jname";
    let run = pty_run(
        client.command(&["schema", "field", option_shaped]),
        StdoutTo::Pty,
        DEADLINE,
    )
    .await
    .expect("the usage error within the deadline");
    assert_eq!(
        run.status.code(),
        Some(2),
        "{}",
        run.terminal.escape_debug()
    );
    assert!(
        run.terminal.contains("unexpected argument"),
        "{}",
        run.terminal.escape_debug()
    );
    assert_no_terminal_control("an option-shaped field name's usage error", &run.terminal);
    assert!(
        run.terminal.contains('\u{fffd}'),
        "{}",
        run.terminal.escape_debug()
    );
}

// ---------------------------------------------------------------------------
// Case
// ---------------------------------------------------------------------------

/// A sender's hostile field names and value reach the catalog, and the
/// data formats keep them exact in a pipe and in a file. On a PTY, `query`
/// in every format and both `schema` views write no terminal control: the
/// table, csv and schema views show U+FFFD in its place, and json escapes
/// it.
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
    let piped = assert_pipe_json_exact(&client).await;
    assert_csv_file_exact(&client).await;

    assert_terminal_table(&client).await;
    assert_terminal_json(&client, &piped).await;
    assert_terminal_csv(&client).await;
    assert_terminal_schema_fields(&client).await;
    assert_terminal_schema_field(&client).await;
    assert_schema_labels_on_stderr(&client).await;
    assert_usage_error_on_stderr(&client).await;

    server.stop().await;
}

/// The harness's deadline covers the child's exit, not only its terminal.
#[tokio::test(flavor = "multi_thread")]
async fn a_child_past_the_pty_deadline_is_killed_and_reaped() {
    let deadline = Duration::from_millis(200);
    let mut holds_the_terminal = std::process::Command::new("sleep");
    holds_the_terminal.arg("600").stdin(Stdio::null());
    assert!(
        pty_run(holds_the_terminal, StdoutTo::Pty, deadline)
            .await
            .is_none(),
        "a child that outlives the deadline has no run"
    );

    let mut closed_the_terminal = std::process::Command::new("sh");
    closed_the_terminal
        .args(["-c", "exec sleep 600 >&- 2>&-"])
        .stdin(Stdio::null());
    assert!(
        pty_run(closed_the_terminal, StdoutTo::Pty, deadline)
            .await
            .is_none(),
        "a child that closed its terminal and runs on has no run"
    );
}
