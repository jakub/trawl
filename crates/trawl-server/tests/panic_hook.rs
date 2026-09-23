// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawld's panic hook logs where a panic happened, never what it said, and
//! the diagnostic never reaches the WAL (ADR-0040).
//!
//! Its own test binary because the panic hook and the subscriber are both
//! process-global: the production subscriber from
//! `telemetry::build_subscriber`, with a stdout capture and a WAL layer, is
//! installed with `.init()` exactly as `trawld` does.

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

/// Everything the global subscriber writes to stdout.
#[derive(Clone, Default)]
struct Stdout(Arc<Mutex<Vec<u8>>>);

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Stdout {
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

#[test]
fn panic_diagnostic_carries_location_not_payload_and_never_persists() {
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
    let stdout = Stdout::default();
    let (subscriber, _) = telemetry::build_subscriber(
        telemetry::DEFAULT_LOG_FILTER,
        LogSinks {
            stdout: Some(stdout.clone()),
            wal: Some(wal.clone()),
            file_log: false,
        },
    );
    subscriber.init();

    // A previous hook that only records it ran, standing in for the
    // payload-printing default: chaining either would defeat the
    // replacement.
    std::panic::set_hook(Box::new(|_| PREVIOUS_RAN.store(true, Ordering::SeqCst)));
    telemetry::install_panic_hook();

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
    let panic_line = rx.recv().unwrap();
    assert!(
        !PREVIOUS_RAN.load(Ordering::SeqCst),
        "the previous hook was chained, not replaced"
    );

    let text = strip_ansi(&String::from_utf8(stdout.0.lock().unwrap().clone()).unwrap());
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
