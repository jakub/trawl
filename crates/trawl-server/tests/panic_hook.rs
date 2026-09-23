// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawld's panic hook logs where a panic happened, never what it said, and
//! the diagnostic never reaches the WAL (ADR-0040).
//!
//! Its own test binary because the panic hook and the subscriber are both
//! process-global: the production subscriber from
//! `telemetry::build_subscriber`, with a WAL layer and, outside monitor
//! mode, a stdout capture, is installed with `.init()` exactly as `trawld`
//! does. nextest runs each test in its own process, so each installs its
//! own.
//!
//! In monitor mode no text sink records the diagnostic, so the hook writes
//! one location line to stderr itself; with a text sink it writes none.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt as _;
use trawl_server::ingest::producer::Derivation;
use trawl_server::ingest::wal::WalWriter;
use trawl_server::telemetry::{self, LogSinks, PANIC_TARGET, WalHandle, WalLayer};

const ENV: &str = "default";
const PAYLOAD_SENTINEL: &str = "zz-panic-payload-sentinel";
const THREAD: &str = "zz-panicking-worker";

/// Set by the hook the test installs before trawld's.
static PREVIOUS_RAN: AtomicBool = AtomicBool::new(false);

/// Everything written to a captured stdout or stderr.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        strip_ansi(&String::from_utf8(self.0.lock().unwrap().clone()).unwrap())
    }
}

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Drop ANSI SGR sequences, so a line reads the same with or without
/// colour.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn read_wal(dir: &std::path::Path) -> Vec<serde_json::Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "ndjson"))
        .flat_map(|path| {
            std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// A WAL layer writing under a fresh directory, and the directory.
fn wal_layer() -> (WalLayer, tempfile::TempDir) {
    let wal_dir = tempfile::tempdir().unwrap();
    let handle = WalHandle::new();
    handle.set(Arc::new(WalWriter::new(wal_dir.path().to_path_buf())), ENV);
    let wal = WalLayer::new_with_buffer_cap(
        handle,
        &[ENV.to_owned()],
        ENV,
        Arc::new(Derivation::defaults()),
        trawl_config::DEFAULT_TELEMETRY_BUFFER_MAX_BYTES,
    );
    (wal, wal_dir)
}

/// Install trawld's subscriber and panic hook, panic once with the payload
/// sentinel on a thread named [`THREAD`], and return the line of the
/// `panic!`. `text_sink` is what `init_tracing` passes: whether `stdout`
/// (or a file logger) records the diagnostic.
fn install_and_panic(
    stdout: Option<Capture>,
    wal: &WalLayer,
    text_sink: bool,
    stderr: &Capture,
) -> u32 {
    let (subscriber, _) = telemetry::build_subscriber(
        telemetry::DEFAULT_LOG_FILTER,
        LogSinks {
            stdout,
            wal: Some(wal.clone()),
            file_log: false,
        },
    );
    subscriber.init();

    // A previous hook that only records it ran, standing in for the
    // payload-printing default: chaining either would defeat the
    // replacement.
    std::panic::set_hook(Box::new(|_| PREVIOUS_RAN.store(true, Ordering::SeqCst)));
    telemetry::install_panic_hook(text_sink, stderr.clone());

    let (tx, rx) = std::sync::mpsc::channel();
    let joined = std::thread::Builder::new()
        .name(THREAD.to_owned())
        .spawn(move || {
            tx.send(line!() + 1).unwrap();
            panic!("{PAYLOAD_SENTINEL}");
        })
        .unwrap()
        .join();
    assert!(joined.is_err(), "the thread panicked");
    assert!(
        !PREVIOUS_RAN.load(Ordering::SeqCst),
        "the previous hook was chained, not replaced"
    );
    rx.recv().unwrap()
}

/// With a stdout sink the diagnostic is one tracing event there, and the
/// hook writes nothing to stderr.
#[test]
fn panic_diagnostic_carries_location_not_payload_and_never_persists() {
    let (wal, wal_dir) = wal_layer();
    let stdout = Capture::default();
    let stderr = Capture::default();
    let panic_line = install_and_panic(Some(stdout.clone()), &wal, true, &stderr);

    assert_eq!(
        stderr.text(),
        "",
        "a text sink records the panic, so stderr gets no second line"
    );
    let text = stdout.text();
    let diagnostics: Vec<&str> = text
        .lines()
        .filter(|line| line.contains(PANIC_TARGET))
        .collect();
    assert_eq!(diagnostics.len(), 1, "one diagnostic: {text}");
    let diagnostic = diagnostics[0];
    assert!(diagnostic.contains("ERROR"), "{diagnostic}");
    assert!(diagnostic.contains("event_type=\"panic\""), "{diagnostic}");
    assert!(
        diagnostic.contains(&format!("file=\"{}\"", file!())),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains(&format!(" line={panic_line} ")),
        "{diagnostic}"
    );
    assert!(diagnostic.contains(" column="), "{diagnostic}");
    assert!(
        diagnostic.contains(&format!("thread=\"{THREAD}\"")),
        "{diagnostic}"
    );
    assert!(
        !text.contains(PAYLOAD_SENTINEL),
        "the payload reached stdout: {text}"
    );

    // A control event proves the WAL path is live, so an absent
    // diagnostic means refused, not lost.
    tracing::info!(target: "trawl_server::control", event_type = "zz_control", "control");
    wal.flush();
    let records = read_wal(&wal_dir.path().join(ENV));
    assert!(
        records
            .iter()
            .any(|record| record["event_type"] == "zz_control"),
        "{records:?}"
    );
    for record in &records {
        let serialized = record.to_string();
        assert!(
            record["target"] != PANIC_TARGET && record["event_type"] != "panic",
            "the panic diagnostic reached the WAL: {serialized}"
        );
        assert!(
            !serialized.contains(PAYLOAD_SENTINEL),
            "the payload reached the WAL: {serialized}"
        );
    }
}

/// In monitor mode no text sink records the diagnostic, so the hook writes
/// exactly one line to stderr: the location and the thread, never the
/// payload.
#[test]
fn a_panic_without_a_text_sink_writes_its_location_to_stderr() {
    let (wal, _wal_dir) = wal_layer();
    let stderr = Capture::default();
    let panic_line = install_and_panic(None, &wal, false, &stderr);

    let text = stderr.text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "one stderr line: {text}");
    let prefix = format!("trawld: panicked at {}:{panic_line}:", file!());
    let suffix = format!(" on thread '{THREAD}'");
    let line = lines[0];
    assert!(line.starts_with(&prefix), "{line}");
    assert!(line.ends_with(&suffix), "{line}");
    let column = &line[prefix.len()..line.len() - suffix.len()];
    assert!(column.parse::<u32>().is_ok(), "a column: {line}");
    assert!(
        !text.contains(PAYLOAD_SENTINEL),
        "the payload reached stderr: {text}"
    );
}
