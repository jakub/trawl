// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl preview-ingest` against a real trawld (ADR-0049).
//!
//! The server is trawld's own stack, booted in-process by `common` the
//! way trawl-server's integration fixture boots it: its own fleet and app
//! databases minted from `DATABASE_URL`, the real boot path (advisory
//! lock, migrations, corpus preparation) and the real HTTPS listener on an
//! ephemeral port with a self-signed certificate. The CLI is the real
//! `trawl` binary in a subprocess, so the exit status, stdin and stdout are
//! the ones an operator sees. Every run strips the inherited `TRAWL_*`
//! environment, gets a temp `HOME`, and pins the server's certificate
//! through its own config file.
//!
//! Each test drops the databases it minted when it finishes. A panicking
//! test leaves them behind under trawl-server's fixture naming scheme
//! (`{prefix}_{run}_{pid}_{rand}`), which that suite's sweeper collects
//! once the owning process is gone.
//!
//! Without `DATABASE_URL` these tests fail rather than skip: a real-server
//! suite that silently skips proves nothing.

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use common::{DEFAULT_BODY_LIMIT, Server};
use tokio::io::AsyncWriteExt as _;
use trawl_client::{PreviewEvent, PreviewResponse};

/// A sample with every row shape the table renders: an event that keeps
/// its own slots, one repaired from the peer and the arrival time, a blank
/// line (not a position, but it counts toward the next index), a line that
/// is not JSON, an event refused for having no service, and one whose
/// severity maps to nothing.
const MIXED: &str = concat!(
    r#"{"_time":"2026-01-01T10:00:00Z","service":"api","env":"prod","host":"web-1","level":"warn","msg":"disk 91%"}"#,
    "\n",
    r#"{"service":"worker","Level":"error","timestamp":"yesterday","ctx":{"job":7}}"#,
    "\n",
    "\n",
    "not json\n",
    r#"{"msg":"who sent this"}"#,
    "\n",
    r#"{"service":"api","host":"web-2","severity":"loud","_time":"2026-01-01T10:00:01Z"}"#,
    "\n",
);

/// The two events of [`MIXED`] that are accepted, as a JSON array.
const ACCEPTED: &str = r#"[
  {"_time":"2026-01-01T10:00:00Z","service":"api","env":"prod","host":"web-1","level":"warn","msg":"disk 91%"},
  {"service":"worker","Level":"error","timestamp":"yesterday","ctx":{"job":7}}
]"#;

/// Run `trawl -c <config> preview-ingest <args>`, with `stdin` on its
/// standard input and `token` as `TRAWL_TOKEN`.
async fn trawl(config: &Path, token: Option<&str>, args: &[&str], stdin: &[u8]) -> Output {
    let home = config.parent().expect("config dir");
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_trawl"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("TRAWL_") {
            cmd.env_remove(key);
        }
    }
    cmd.env("HOME", home)
        .arg("-c")
        .arg(config)
        .arg("preview-ingest")
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(token) = token {
        cmd.env("TRAWL_TOKEN", token);
    }
    let mut child = cmd.spawn().expect("spawn trawl");
    let mut input = child.stdin.take().expect("stdin");
    input.write_all(stdin).await.expect("write stdin");
    drop(input);
    tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("trawl finished within 30s")
        .expect("trawl ran")
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("utf-8 output")
}

/// Whether `stderr` is exactly `line`, then the ` (request <id>)` suffix
/// the CLI ends every server error with, then a newline. The id is a
/// ULID, new on every request, so only its shape can be asserted: 26
/// Crockford base32 characters.
fn is_server_line(stderr: &str, line: &str) -> bool {
    stderr
        .strip_prefix(line)
        .and_then(|rest| rest.strip_prefix(" (request "))
        .and_then(|rest| rest.strip_suffix(")\n"))
        .is_some_and(|id| {
            id.len() == 26
                && id.bytes().all(|b| {
                    b.is_ascii_digit() || (b.is_ascii_uppercase() && !b"ILOU".contains(&b))
                })
        })
}

/// The table with the header's arrival instant, the one value that
/// changes between runs, masked. The instant must have the report's fixed
/// shape (RFC 3339 UTC, microseconds) to be masked at all.
fn mask_arrival(table: &str) -> String {
    let (header, rest) = table.split_once('\n').expect("a header line");
    let (head, arrival) = header.rsplit_once("; arrival ").expect("an arrival");
    let shape = arrival.len() == "2026-01-01T00:00:00.000000Z".len()
        && arrival.ends_with('Z')
        && chrono::DateTime::parse_from_rfc3339(arrival).is_ok();
    assert!(
        shape,
        "arrival {arrival:?} is not RFC 3339 UTC microseconds"
    );
    format!("{head}; arrival [ARRIVAL]\n{rest}")
}

fn write_sample(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write the sample");
    path
}

/// The table, read from stdin with no peer: a rejected event exits 1, the
/// header names the placeholder, and host-less events are marked as
/// depending on the sender. The snapshot pins the whole table; only the
/// arrival instant changes between runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_event_exits_1_and_the_table_shows_every_outcome() {
    let server = Server::start(true).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = server.config(dir.path());

    let output = trawl(&config, Some(&server.admin_token), &["-"], MIXED.as_bytes()).await;
    assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));
    assert_eq!(text(&output.stderr), "");
    insta::assert_snapshot!(
        "table_mixed_sample_no_peer",
        mask_arrival(text(&output.stdout))
    );

    // No FILE argument reads stdin the same way `-` does.
    let bare = trawl(&config, Some(&server.admin_token), &[], MIXED.as_bytes()).await;
    assert_eq!(bare.status.code(), Some(1), "{}", text(&bare.stderr));
    assert_eq!(
        text(&bare.stdout).lines().skip(1).collect::<Vec<_>>(),
        text(&output.stdout).lines().skip(1).collect::<Vec<_>>(),
        "only the header's arrival may differ"
    );
    server.stop().await;
}

/// Every event accepted, read from a file with a peer: exit 0, the peer
/// fills `host`, and nothing depends on the sender.
#[tokio::test(flavor = "multi_thread")]
async fn all_accepted_from_a_file_with_a_peer_exits_0() {
    let server = Server::start(true).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = server.config(dir.path());
    let sample = write_sample(dir.path(), "accepted.json", ACCEPTED);

    let output = trawl(
        &config,
        Some(&server.admin_token),
        &[sample.to_str().unwrap(), "--peer-ip", "10.0.0.7"],
        b"",
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(
        stdout.starts_with("peer 10.0.0.7 (given), not a trusted relay; arrival "),
        "{stdout}"
    );
    assert!(
        stdout.contains("┆ 10.0.0.7 "),
        "the peer fills host: {stdout}"
    );
    assert!(!stdout.contains("(sender)"), "{stdout}");
    assert!(
        stdout.ends_with("2 event(s): 2 accepted, 0 rejected\n"),
        "{stdout}"
    );
    server.stop().await;
}

/// An NDJSON event `depth` containers deep: the object and the arrays
/// nested inside its `payload`.
fn event_of_depth(depth: usize) -> String {
    let arrays = depth - 1;
    format!(
        r#"{{"service":"api","payload":{}1{}}}"#,
        "[".repeat(arrays),
        "]".repeat(arrays)
    )
}

/// The deepest event ingest accepts previews as accepted, and one
/// container more as refused, in a complete report: the CLI decodes the
/// report even though it echoes that event three containers further in.
#[tokio::test(flavor = "multi_thread")]
async fn the_deepest_event_ingest_accepts_previews_whole() {
    // `serde_json`'s default recursion limit, which ingest parses with,
    // refuses the 128th open container.
    const DEEPEST: usize = 127;
    let server = Server::start(true).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = server.config(dir.path());
    let sample = format!(
        "{}\n{}\n",
        event_of_depth(DEEPEST),
        event_of_depth(DEEPEST + 1)
    );

    let output = trawl(
        &config,
        Some(&server.admin_token),
        &["--peer-ip", "10.0.0.7"],
        sample.as_bytes(),
    )
    .await;
    assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));
    assert_eq!(text(&output.stderr), "");
    let stdout = text(&output.stdout);
    assert!(
        stdout.ends_with("2 event(s): 1 accepted, 1 rejected\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("┆ invalid_json: invalid JSON: recursion limit exceeded"),
        "{stdout}"
    );
    server.stop().await;
}

/// `--json` prints the server's report, whole, and keeps the exit status.
#[tokio::test(flavor = "multi_thread")]
async fn json_prints_the_report_and_keeps_the_exit_status() {
    let server = Server::start(true).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = server.config(dir.path());
    let sample = write_sample(dir.path(), "mixed.ndjson", MIXED);

    let output = trawl(
        &config,
        Some(&server.admin_token),
        &["--json", sample.to_str().unwrap()],
        b"",
    )
    .await;
    assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));
    let report: PreviewResponse =
        serde_json::from_slice(&output.stdout).expect("stdout is the report");
    assert_eq!(report.producer, "http");
    assert_eq!(report.peer.ip, "192.0.2.1");
    assert!(!report.peer.given);
    assert_eq!((report.accepted, report.rejected), (3, 2));
    let outcomes: Vec<(usize, &str, bool)> = report
        .events
        .iter()
        .map(|event| match event {
            PreviewEvent::Accepted {
                index,
                host_depends_on_sender,
                ..
            } => (*index, "accepted", *host_depends_on_sender),
            PreviewEvent::Rejected {
                index,
                host_depends_on_sender,
                ..
            } => (*index, "rejected", *host_depends_on_sender),
        })
        .collect();
    assert_eq!(
        outcomes,
        vec![
            (0, "accepted", false),
            (1, "accepted", true),
            (3, "rejected", false),
            (4, "rejected", true),
            (5, "accepted", false),
        ]
    );
    server.stop().await;
}

/// A key without `server_manage` gets the server's 403, which is no
/// report: exit 2, and nothing on stdout.
#[tokio::test(flavor = "multi_thread")]
async fn a_key_without_server_manage_exits_2() {
    let server = Server::start(true).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = server.config(dir.path());

    let output = trawl(
        &config,
        Some(&server.ingest_token),
        &["-"],
        MIXED.as_bytes(),
    )
    .await;
    assert_eq!(output.status.code(), Some(2), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "");
    assert!(
        text(&output.stderr).starts_with("trawl: server error (HTTP 403)"),
        "{}",
        text(&output.stderr)
    );
    server.stop().await;
}

/// A server with ingest off has no preview route. Its 404 says the server
/// does not ingest, and exits 2.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_without_ingest_exits_2_and_says_it_does_not_ingest() {
    let server = Server::start(false).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = server.config(dir.path());

    let output = trawl(&config, Some(&server.admin_token), &["-"], MIXED.as_bytes()).await;
    assert_eq!(output.status.code(), Some(2), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        format!("trawl: {}\n", trawl_cli::preview_ingest::DOES_NOT_INGEST)
    );
    server.stop().await;
}

/// A `--peer-ip` that is not an address is clap's usage error, exit 2,
/// before anything is read or sent.
#[tokio::test]
async fn a_bad_peer_ip_is_a_usage_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = write_sample(
        dir.path(),
        "config.toml",
        "[server]\nurl = \"https://127.0.0.1:1\"\n",
    );
    let output = trawl(
        &config,
        Some("unused"),
        &["--peer-ip", "not-an-ip", "-"],
        b"",
    )
    .await;
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(text(&output.stdout), "");
    assert!(
        text(&output.stderr).contains("--peer-ip <IP>"),
        "{}",
        text(&output.stderr)
    );
}

/// A sample that cannot be read is exit 2, naming the path, and no
/// request is attempted (the configured port has nothing behind it).
#[tokio::test]
async fn an_unreadable_sample_exits_2() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = write_sample(
        dir.path(),
        "config.toml",
        "[server]\nurl = \"https://127.0.0.1:1\"\n",
    );
    let missing = dir.path().join("missing.ndjson");
    let output = trawl(&config, Some("unused"), &[missing.to_str().unwrap()], b"").await;
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(text(&output.stdout), "");
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with(&format!("trawl: cannot read {}: ", missing.display())),
        "{stderr}"
    );
}

/// `lines` valid NDJSON events of `line_len` bytes each, newline
/// included, so a refusal can only be about the body's size.
fn padded_events(lines: usize, line_len: usize) -> Vec<u8> {
    let shell = r#"{"service":"api","msg":""}"#.len() + 1;
    let line = format!(
        "{{\"service\":\"api\",\"msg\":\"{}\"}}\n",
        "x".repeat(line_len - shell)
    );
    assert_eq!(line.len(), line_len);
    line.repeat(lines).into_bytes()
}

/// A body moderately over `max_request_body_bytes` gets the server's
/// `request_too_large` message, never `unknown error`, and exits 2.
///
/// The 1 KiB limit and a 4 KiB body keep this deterministic: the whole
/// request is a few TLS records that land in trawld's first read, so when
/// it refuses and hangs up nothing it was sent is left unread. The close
/// is a FIN, not a reset, and the client always reads the 413.
#[tokio::test(flavor = "multi_thread")]
async fn a_body_moderately_over_the_limit_prints_the_servers_413() {
    let server = Server::start_with_body_limit(true, 1024).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = server.config(dir.path());
    let body = padded_events(32, 128);
    assert_eq!(body.len(), 4096);

    let output = trawl(&config, Some(&server.admin_token), &["-"], &body).await;
    assert_eq!(output.status.code(), Some(2), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "");
    let stderr = text(&output.stderr);
    assert!(!stderr.contains("unknown error"), "{stderr}");
    assert!(
        is_server_line(
            stderr,
            "trawl: server error (HTTP 413): request body exceeds [server] \
             max_request_body_bytes (1024 bytes)"
        ),
        "{stderr}"
    );
    server.stop().await;
}

/// More events than one preview reads, in a body under the byte limit,
/// gets the server's `preview_too_large` message and exits 2.
#[tokio::test(flavor = "multi_thread")]
async fn more_events_than_a_preview_reads_prints_the_servers_413() {
    let server = Server::start(true).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = server.config(dir.path());
    let limit = trawl_api::ingest_preview::MAX_PREVIEW_EVENTS;
    let body = r#"{"service":"api"}"#.to_owned() + "\n";
    let body = body.repeat(limit + 1);
    assert!(body.len() < DEFAULT_BODY_LIMIT);

    let output = trawl(&config, Some(&server.admin_token), &["-"], body.as_bytes()).await;
    assert_eq!(output.status.code(), Some(2), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "");
    let stderr = text(&output.stderr);
    assert!(
        is_server_line(
            stderr,
            &format!(
                "trawl: server error (HTTP 413): the sample holds more than {limit} events; \
                 preview at most {limit} at a time"
            )
        ),
        "{stderr}"
    );
    server.stop().await;
}

/// A body several times over `max_request_body_bytes` exits 2 with one of
/// two exact lines, and never `request failed`.
///
/// Both outcomes are legitimate. trawld refuses on `Content-Length` and
/// hangs up without draining the body (a ruling: draining would spend
/// bandwidth on refused bodies, before authentication). The client is
/// still uploading, so whether it reads the 413 first or hits the reset
/// of the unread upload depends on socket buffers and timing. A reset is
/// reported as a cut-off upload. The test never retries.
#[tokio::test(flavor = "multi_thread")]
async fn a_body_far_over_the_limit_prints_the_413_or_a_cut_off_upload() {
    let server = Server::start(true).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = server.config(dir.path());
    // 256 events, under the preview's event cap, so the byte limit is the
    // only refusal on offer.
    let body = padded_events(256, 8 * 1024);
    assert_eq!(body.len(), 2 * 1024 * 1024);
    const { assert!(256 <= trawl_api::ingest_preview::MAX_PREVIEW_EVENTS) };

    let output = trawl(&config, Some(&server.admin_token), &["-"], &body).await;
    assert_eq!(output.status.code(), Some(2), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "");
    let stderr = text(&output.stderr);
    assert!(!stderr.contains("request failed"), "{stderr}");
    assert!(!stderr.contains("unknown error"), "{stderr}");
    let refused = format!(
        "trawl: server error (HTTP 413): request body exceeds [server] \
         max_request_body_bytes ({DEFAULT_BODY_LIMIT} bytes)"
    );
    let cut_off = "trawl: network error: the server closed the connection before the \
                   upload finished; the request may exceed the server's request size limit, \
                   or the server may be at its request limit\n";
    assert!(
        is_server_line(stderr, &refused) || stderr == cut_off,
        "{stderr}"
    );
    eprintln!("far-over-limit outcome: {}", stderr.trim_end());
    server.stop().await;
}
