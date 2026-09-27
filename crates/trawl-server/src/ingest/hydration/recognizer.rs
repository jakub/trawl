// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Whole-file recognition of live WAL writer output (ADR-0041 slice 2).
//!
//! Hydration loads a WAL file only if its bytes are exactly what the live
//! writer produces: a writer file name ([`WalName`]) on a regular file, and
//! lines that each parse to an object of scalar values with folded keys and
//! encode back to the same bytes through [`wal::encode_line`]. The check
//! covers the whole file. A file that fails anywhere is rejected whole,
//! never loaded as a prefix, and stays for compaction's decoder, which
//! handles every other file as it always has. Hydration never
//! canonicalizes an event again, and it takes no row whose envelope
//! instants compaction would repair, so a hydrated event reads the same hot
//! as it will cold.
//!
//! Only Linux opens a WAL file without following a symlink and without
//! blocking on a FIFO. Off Linux every entry is [`Rejection::Unreadable`],
//! so the boot hydrates nothing and compaction clears the WAL as
//! overhang.

use std::fs::File;
use std::io::Read as _;
use std::path::Path;

use serde_json::{Map, Value};

use crate::ingest::compaction;
use crate::ingest::envelope::is_folded_name;
use crate::ingest::no_follow;
use crate::ingest::wal::{self, WalName};

/// One WAL line's event.
pub(crate) type Event = Map<String, Value>;

/// A WAL file the live writer produced, recognized in full.
#[derive(Debug)]
pub(crate) struct Recognized {
    pub name: WalName,
    /// One event per line, in file order.
    pub events: Vec<Event>,
    /// The file's length: exactly the ndjson bytes `ServiceBatch::push`
    /// charged for `events`.
    pub bytes: usize,
}

/// Why [`examine`] rejected a WAL entry. The file is left where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rejection {
    /// The name is not one the writer produces. Nothing was opened.
    Name,
    /// The entry is not a regular file, is a symlink, or could not be read
    /// in full at one length.
    Unreadable,
    /// The file is longer than the caller allowed. Nothing was read.
    TooLong { len: u64 },
    /// The file was read and has more lines than the caller allowed events.
    /// No line was parsed.
    TooMany { lines: usize },
    /// The bytes were read in full and are not writer output.
    Unrecognized,
}

/// How much of one WAL file [`examine`] may take.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// The longest file it reads.
    pub bytes: u64,
    /// The most lines it parses. Writer output has one event per line.
    pub events: usize,
}

/// Examine one WAL entry: its name, then the file, which is read in full
/// if it is at most `limits.bytes` long, then its lines, which are parsed
/// only if there are at most `limits.events` of them. Counting the
/// newlines of bytes already read costs no allocation, so a file of tiny
/// lines never builds more events than fit. Every byte read is added to
/// `spent`, including the bytes of a file rejected after reading.
pub(crate) fn examine(
    path: &Path,
    limits: Limits,
    spent: &mut u64,
) -> Result<Recognized, Rejection> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(WalName::parse)
        .ok_or(Rejection::Name)?;
    let file = WalFile::open(path).map_err(|_| Rejection::Unreadable)?;
    if file.len > limits.bytes {
        return Err(Rejection::TooLong { len: file.len });
    }
    let bytes = file.read_whole(spent).map_err(|_| Rejection::Unreadable)?;
    // Writer output ends every line, and so every event, with a newline.
    let lines = memchr::memchr_iter(b'\n', &bytes).count();
    if lines > limits.events {
        return Err(Rejection::TooMany { lines });
    }
    let events = recognize(&bytes).ok_or(Rejection::Unrecognized)?;
    Ok(Recognized {
        name,
        events,
        bytes: bytes.len(),
    })
}

/// The events of a whole WAL file, if and only if `bytes` is exactly what
/// the live writer produces for them. `None` rejects the whole file.
///
/// Accepted: non-empty UTF-8 with no NUL, ending in `\n`, with no blank
/// line, where every line is a JSON object whose values are scalars
/// (null, bool, number, string), whose keys are folded
/// ([`is_folded_name`]), whose `_time` and `_ingested` compaction's repair
/// leaves as they are ([`compaction::repair_leaves_unchanged`]), and where
/// [`wal::encode_line`] of the parsed object gives back that line byte for
/// byte. The last check rejects whitespace, key order, escapes and number
/// spellings the writer does not produce, and a duplicate key, which
/// parses to one fewer key. Float values need `serde_json`'s
/// `float_roundtrip` to parse back to the value the writer printed.
///
/// The charge of the returned events is `(events.len(), bytes.len())`,
/// what `ServiceBatch::push` charged for them, and `events.len()` is the
/// number of newlines in `bytes`.
pub(crate) fn recognize(bytes: &[u8]) -> Option<Vec<Event>> {
    if bytes.is_empty() || bytes.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    if !text.ends_with('\n') {
        return None;
    }
    let mut events = Vec::new();
    let mut encoded = Vec::new();
    for line in text.split_inclusive('\n') {
        let json = line.strip_suffix('\n')?;
        if json.is_empty() {
            return None;
        }
        #[cfg(test)]
        PARSED_LINES.with(|parsed| parsed.set(parsed.get() + 1));
        let event: Event = serde_json::from_str(json).ok()?;
        if !event
            .iter()
            .all(|(key, value)| is_folded_name(key) && is_scalar(value))
            || !compaction::repair_leaves_unchanged(&event)
        {
            return None;
        }
        encoded.clear();
        wal::encode_line(&event, &mut encoded).ok()?;
        if encoded != line.as_bytes() {
            return None;
        }
        events.push(event);
    }
    Some(events)
}

#[cfg(test)]
thread_local! {
    /// Lines [`recognize`] parsed on this thread.
    static PARSED_LINES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many lines [`recognize`] has parsed on this thread, for a test that
/// checks a file was refused before any of its lines was parsed.
#[cfg(test)]
pub(crate) fn parsed_lines_for_test() -> u64 {
    PARSED_LINES.with(std::cell::Cell::get)
}

const fn is_scalar(value: &Value) -> bool {
    !matches!(value, Value::Array(_) | Value::Object(_))
}

/// A WAL entry opened as a regular file, without following a symlink.
#[derive(Debug)]
struct WalFile {
    file: File,
    /// The length `fstat` reported on the open descriptor.
    len: u64,
}

impl WalFile {
    /// Open `path` read-only, refusing a symlink at the last component, and
    /// check with `fstat` on the descriptor that it is a regular file. The
    /// open does not block: a FIFO is refused after it opens, never read.
    fn open(path: &Path) -> std::io::Result<Self> {
        let file = no_follow::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        Ok(Self {
            file,
            len: metadata.len(),
        })
    }

    /// Read exactly [`Self::len`] bytes, adding every byte read to `spent`.
    /// A file that is shorter than that, or whose length changed by the
    /// end of the read, is an error.
    fn read_whole(mut self, spent: &mut u64) -> std::io::Result<Vec<u8>> {
        let capacity = usize::try_from(self.len).map_err(std::io::Error::other)?;
        let mut bytes = Vec::with_capacity(capacity);
        // `read_to_end` keeps what it read before an error, so the bytes
        // count against the budget either way.
        let read = (&mut self.file)
            .take(self.len)
            .read_to_end(&mut bytes)
            .map(|_| ());
        *spent = spent.saturating_add(bytes.len() as u64);
        read?;
        if bytes.len() as u64 != self.len || self.file.metadata()?.len() != self.len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the file changed length while it was read",
            ));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::test_support::{instant, stamped};
    use super::*;
    use crate::ingest::pipeline::ServiceBatch;

    /// Events the canonicalizer can emit, with values chosen to be awkward
    /// to round-trip: floats that need exact parsing, extreme integers,
    /// escapes and non-ASCII text.
    fn awkward_events() -> Vec<Event> {
        let floats = [
            0.1,
            0.1 + 0.2,
            1.0 / 3.0,
            1e300,
            -0.0,
            f64::MAX,
            f64::MIN,
            5e-324,
            f64::MIN_POSITIVE,
            f64::EPSILON,
            1.0,
            -1.5,
            1e21,
            1e-7,
            2f64.powi(60),
            std::f64::consts::PI,
            // Printed as 1.4000000000000001; without `float_roundtrip`,
            // serde_json parses this and the next two one ULP off.
            0.1 * 14.0,
            1.071_566_039_146_582_6e-75,
            -1.603_964_615_428_183e143,
        ];
        let mut events: Vec<Event> = floats
            .iter()
            .map(|&f| {
                let mut event = Event::new();
                event.insert("service".into(), json!("svc"));
                event.insert("float".into(), json!(f));
                event
            })
            .collect();
        let mut extremes = Event::new();
        extremes.insert("min".into(), json!(i64::MIN));
        extremes.insert("max".into(), json!(u64::MAX));
        extremes.insert("zero".into(), json!(0));
        extremes.insert("negative".into(), json!(-1));
        extremes.insert("yes".into(), json!(true));
        extremes.insert("no".into(), json!(false));
        extremes.insert("nothing".into(), Value::Null);
        events.push(extremes);
        let mut text = Event::new();
        text.insert("unicode".into(), json!("żółw 🐢 日本語 \u{2028}\u{2029}"));
        text.insert(
            "escapes".into(),
            json!(
                "quote \" backslash \\ slash / nl \n cr \r tab \t bell \u{7} nul \u{0} del \u{7f}"
            ),
        );
        text.insert("empty".into(), json!(""));
        text.insert("_time".into(), json!("2026-09-27T12:00:00.123456Z"));
        text.insert("k8s".into(), json!(r#"{"pod":"a","n":[1,2]}"#));
        text.insert("ключ".into(), json!("non-ASCII key"));
        events.push(text);
        events.push(Event::new());
        events.into_iter().map(stamped).collect()
    }

    /// The opening of a writer line whose envelope instants are stamped,
    /// for hand-spelled lines: every key after it sorts after `_time`.
    fn stamped_prefix() -> String {
        let line = String::from_utf8(line(&stamped(Event::new()))).unwrap();
        format!("{},", line.strip_suffix("}\n").unwrap())
    }

    /// The bytes and charge the live writer produces for `events`.
    fn written(events: &[Event]) -> ServiceBatch {
        let mut batch = ServiceBatch::default();
        for event in events {
            batch.push(event.clone());
        }
        batch
    }

    /// One writer line for `event`.
    fn line(event: &Event) -> Vec<u8> {
        let mut out = Vec::new();
        wal::encode_line(event, &mut out).unwrap();
        out
    }

    /// Writer lines for `events`, with `bad` spliced in between them, so a
    /// rejection is of the whole file and not of the lines after `bad`.
    fn with_bad_line_inside(bad: &[u8]) -> Vec<u8> {
        let events = awkward_events();
        let (before, after) = events.split_at(events.len() / 2);
        let mut bytes = written(before).ndjson;
        bytes.extend_from_slice(bad);
        bytes.extend_from_slice(&written(after).ndjson);
        bytes
    }

    /// Limits of `bytes` and as many events as the lines any test writes.
    fn limits(bytes: u64) -> Limits {
        Limits {
            bytes,
            events: usize::MAX,
        }
    }

    fn assert_rejected(bytes: &[u8], why: &str) {
        assert!(recognize(bytes).is_none(), "{why}: {bytes:?}");
    }

    #[test]
    fn writer_output_round_trips_with_its_charge() {
        let events = awkward_events();
        let batch = written(&events);
        assert_eq!(batch.maps.len(), events.len(), "push kept every event");
        let recognized = recognize(&batch.ndjson).expect("writer output is recognized");
        assert_eq!(recognized, batch.maps);
        // `Value` equality treats -0.0 and 0.0 as equal: compare bytes too.
        let mut again = Vec::new();
        for event in &recognized {
            wal::encode_line(event, &mut again).unwrap();
        }
        assert_eq!(again, batch.ndjson);
        let lines = std::str::from_utf8(&batch.ndjson).unwrap().lines().count();
        assert_eq!(
            crate::hot_buffer::Charge {
                events: recognized.len(),
                bytes: batch.ndjson.len(),
            },
            batch.charge()
        );
        assert_eq!(lines, recognized.len());
    }

    #[test]
    fn every_awkward_value_round_trips_on_its_own() {
        for event in awkward_events() {
            let batch = written(std::slice::from_ref(&event));
            assert_eq!(
                recognize(&batch.ndjson).as_deref(),
                Some(std::slice::from_ref(&event)),
                "{}",
                String::from_utf8_lossy(&batch.ndjson)
            );
        }
    }

    #[test]
    fn examine_reads_a_writer_file_in_full() {
        let tmp = tempfile::tempdir().unwrap();
        let batch = written(&awkward_events());
        let name = WalName {
            service: "api.v2_x-y".into(),
            millis: 1_790_000_000_123,
            nonce: 0x0a1f,
        };
        let path = tmp.path().join(name.file_name());
        std::fs::write(&path, &batch.ndjson).unwrap();
        let mut spent = 0;
        let recognized = examine(&path, limits(batch.ndjson.len() as u64), &mut spent).unwrap();
        assert_eq!(recognized.name, name);
        assert_eq!(recognized.events, batch.maps);
        assert_eq!(recognized.bytes, batch.ndjson.len());
        assert_eq!(spent, batch.ndjson.len() as u64);
    }

    #[test]
    fn rejects_a_nested_object_or_array() {
        for nested in [json!({"a": 1}), json!([1, 2]), json!({}), json!([])] {
            let mut event = stamped(Event::new());
            event.insert("nested".into(), nested);
            assert_rejected(&with_bad_line_inside(&line(&event)), "a nested value");
        }
    }

    #[test]
    fn rejects_a_duplicate_key() {
        let stamps = stamped_prefix();
        assert!(recognize(format!("{stamps}\"a\":1}}\n").as_bytes()).is_some());
        for duplicated in ["\"a\":1,\"a\":1", "\"a\":1,\"a\":2"] {
            assert_rejected(
                &with_bad_line_inside(format!("{stamps}{duplicated}}}\n").as_bytes()),
                "a duplicate key",
            );
        }
    }

    #[test]
    fn rejects_whitespace_the_writer_does_not_write() {
        let event = awkward_events().swap_remove(0);
        let good = line(&event);
        let text = String::from_utf8(good.clone()).unwrap();
        for spaced in [
            text.replacen(':', ": ", 1),
            text.replacen(',', " ,", 1),
            format!(" {text}"),
            text.replacen('\n', " \n", 1),
            text.replacen('\n', "\r\n", 1),
            text.replacen('{', "{\t", 1),
        ] {
            assert_ne!(spaced.as_bytes(), good.as_slice());
            assert_rejected(&with_bad_line_inside(spaced.as_bytes()), "extra whitespace");
        }
    }

    #[test]
    fn rejects_a_nul_byte() {
        let mut event = stamped(Event::new());
        event.insert("msg".into(), json!("ab"));
        let good = line(&event);
        assert!(recognize(&good).is_some());
        let nul_inside: Vec<u8> = good
            .iter()
            .flat_map(|&b| if b == b'a' { vec![b'a', 0] } else { vec![b] })
            .collect();
        assert_rejected(&with_bad_line_inside(&nul_inside), "a NUL inside a string");
        let mut trailing = written(&awkward_events()).ndjson;
        trailing.push(0);
        assert_rejected(&trailing, "a NUL after the last line");
        // A torn write: the length reached disk before the data.
        assert_rejected(&vec![0; good.len()], "a file of NULs");
    }

    #[test]
    fn rejects_an_empty_file() {
        assert_rejected(b"", "an empty file");
        assert_rejected(b"\n", "a lone newline");
    }

    #[test]
    fn rejects_a_missing_trailing_newline() {
        let mut bytes = written(&awkward_events()).ndjson;
        assert_eq!(bytes.pop(), Some(b'\n'));
        assert_rejected(&bytes, "a missing trailing newline");
    }

    #[test]
    fn rejects_invalid_utf8() {
        let mut event = stamped(Event::new());
        event.insert("msg".into(), json!("é"));
        let good = line(&event);
        assert!(recognize(&good).is_some());
        // Cut the two-byte `é` in half.
        let cut: Vec<u8> = good.iter().copied().filter(|&b| b != 0xa9).collect();
        assert_ne!(cut, good);
        assert_rejected(&with_bad_line_inside(&cut), "invalid UTF-8");
        assert_rejected(&with_bad_line_inside(b"\xff\xfe\n"), "invalid UTF-8");
    }

    #[test]
    fn rejects_a_blank_line() {
        assert_rejected(&with_bad_line_inside(b"\n"), "a blank line");
        let mut leading = b"\n".to_vec();
        leading.extend(written(&awkward_events()).ndjson);
        assert_rejected(&leading, "a leading blank line");
        let mut trailing = written(&awkward_events()).ndjson;
        trailing.push(b'\n');
        assert_rejected(&trailing, "a trailing blank line");
    }

    #[test]
    fn rejects_a_line_that_is_not_an_object() {
        for value in [
            json!(1),
            json!("text"),
            json!(null),
            json!(true),
            json!([{"a": 1}]),
        ] {
            let mut bad = serde_json::to_vec(&value).unwrap();
            bad.push(b'\n');
            assert_rejected(&with_bad_line_inside(&bad), "a non-object line");
        }
        assert_rejected(&with_bad_line_inside(b"{\"a\":1\n"), "a truncated object");
        assert_rejected(
            &with_bad_line_inside(b"{\"a\":1}{\"b\":2}\n"),
            "two objects",
        );
    }

    #[test]
    fn rejects_an_uppercase_key() {
        for key in ["Service", "_Time", "mSg", "A"] {
            let mut event = stamped(Event::new());
            event.insert(key.into(), json!("v"));
            assert!(!is_folded_name(key));
            assert_rejected(&with_bad_line_inside(&line(&event)), "an uppercase key");
        }
        // Non-ASCII case is not folded, so it is not refused.
        let mut event = stamped(Event::new());
        event.insert("cafÉ".into(), json!("v"));
        assert!(recognize(&line(&event)).is_some());
    }

    #[test]
    fn rejects_spellings_the_writer_does_not_produce() {
        let stamps = stamped_prefix();
        assert!(recognize(format!("{stamps}\"a\":2,\"b\":1}}\n").as_bytes()).is_some());
        for spelled in [
            "\"b\":1,\"a\":2",
            "\"a\":1.0e2",
            "\"a\":1E2",
            "\"a\":0.10",
            "\"a\":-0",
            "\"a\":\"\\u0041\"",
            "\"a\":\"\\/\"",
        ] {
            assert_rejected(
                &with_bad_line_inside(format!("{stamps}{spelled}}}\n").as_bytes()),
                "a non-writer spelling",
            );
        }
    }

    /// The row from the final review: `_time` is not an instant, so
    /// compaction's repair would replace it with the file name's instant
    /// while the hot read casts it to NULL.
    #[test]
    fn rejects_a_row_compaction_would_repair() {
        assert_rejected(
            &with_bad_line_inside(
                b"{\"_ingested\":\"2026-09-27T12:00:00Z\",\"_time\":\"not-a-date\",\"env\":\"prod\",\"service\":\"api\"}\n",
            ),
            "a row compaction would repair",
        );
    }

    /// Every envelope instant compaction repairs is checked, and each value
    /// its repair would replace, or that is not spelled as ingest spells
    /// it, rejects the file.
    #[test]
    fn rejects_each_instant_compaction_would_repair() {
        let unrepaired = [
            json!(null),
            json!(true),
            json!(1_790_000_000_000_i64),
            json!(""),
            json!("not-a-date"),
            json!("2026-02-30T00:00:00.000000Z"),
            // Instants ingest never spells this way.
            json!("2026-09-27T12:00:00Z"),
            json!("2026-09-27T12:00:00.123456789Z"),
            json!("2026-09-27T14:00:00.123456+02:00"),
            json!("2026-09-27 12:00:00.123456Z"),
            json!("2026-09-27t12:00:00.123456z"),
            json!("2026-06-30T23:59:60.000000Z"),
            json!("0000-01-01T00:00:00.000000Z"),
        ];
        for column in trawl_core::schema::TIMESTAMP_COLUMNS {
            let mut missing = stamped(Event::new());
            missing.remove(*column);
            assert_rejected(&with_bad_line_inside(&line(&missing)), column);
            for value in &unrepaired {
                let mut event = stamped(Event::new());
                event.insert((*column).to_owned(), value.clone());
                assert_rejected(
                    &with_bad_line_inside(&line(&event)),
                    &format!("{column} = {value}"),
                );
            }
        }
        assert!(recognize(&line(&stamped(Event::new()))).is_some());
    }

    /// Real writer output: client events through the canonicalizer, then
    /// the writer's `ServiceBatch::push`.
    #[test]
    fn canonicalized_events_are_recognized() {
        use crate::ingest::envelope::{EnvelopeContext, canonicalize};
        use crate::ingest::producer::{Derivation, Producer};

        let arrival_instant = chrono::DateTime::parse_from_rfc3339(&instant())
            .unwrap()
            .to_utc();
        let arrival = arrival_instant.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let envs = ["prod".to_owned()];
        let derivation = Derivation::defaults();
        let ctx = EnvelopeContext {
            arrival: &arrival,
            arrival_instant,
            envs: &envs,
            default_env: "prod",
            producer: Producer::Http {
                peer_host: "10.0.4.55",
                peer_is_trusted_relay: false,
            },
            derivation: &derivation,
        };
        let clients = [
            json!({"service": "api", "message": "no time at all"}),
            json!({"service": "api", "_time": "2026-09-27T14:00:00.5+02:00", "level": "warn"}),
            json!({"service": "api", "timestamp": "2026/09/27 12:00:00", "n": 1.5}),
            json!({"service": "api", "_time": "not-a-date", "Mixed_Case": true}),
            json!({"service": "api", "time": 1_790_000_000, "nested": {"a": [1, 2]}}),
            json!({"Service": "api", "_env": "prod", "_host": "web-1", "@timestamp": "2026-09-27"}),
        ];
        let mut batch = ServiceBatch::default();
        for client in clients {
            let canonical = canonicalize(client.as_object().unwrap(), &ctx).unwrap();
            batch.push(canonical.obj);
        }
        assert_eq!(recognize(&batch.ndjson), Some(batch.maps));
    }

    fn examine_path(path: &Path) -> Result<Recognized, Rejection> {
        let mut spent = 0;
        let result = examine(path, limits(u64::MAX), &mut spent);
        if result.is_err() {
            assert_eq!(spent, 0, "a rejection before the read reads nothing");
        }
        result
    }

    #[test]
    fn rejects_a_name_the_writer_does_not_produce() {
        let tmp = tempfile::tempdir().unwrap();
        let bytes = written(&awkward_events()).ndjson;
        for name in [
            "svc.ndjson",
            "svc_1_bad.ndjson",
            "svc_1_ABCD.ndjson",
            "svc_01_abcd.ndjson",
            "svc_+1_abcd.ndjson",
            "svc_1_abcde.ndjson",
            "_1_abcd.ndjson",
            ".svc_1_abcd.ndjson",
            "s v_1_abcd.ndjson",
            "svc_1_abcd.ndjson.merged",
            "svc_1_abcd.tmp",
        ] {
            let path = tmp.path().join(name);
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(examine_path(&path).unwrap_err(), Rejection::Name, "{name}");
        }
    }

    fn writer_name() -> String {
        WalName {
            service: "svc".into(),
            millis: 1_790_000_000_000,
            nonce: 0xbeef,
        }
        .file_name()
    }

    #[test]
    fn rejects_a_symlink_even_to_a_writer_file() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let real = target.join(writer_name());
        std::fs::write(&real, written(&awkward_events()).ndjson).unwrap();
        let link = tmp.path().join(writer_name());
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(
            examine_path(&real).is_ok(),
            "the target itself is writer output"
        );
        assert_eq!(examine_path(&link).unwrap_err(), Rejection::Unreadable);
    }

    #[test]
    fn rejects_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(writer_name());
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(examine_path(&dir).unwrap_err(), Rejection::Unreadable);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rejects_a_fifo_without_waiting_for_a_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let fifo = tmp.path().join(writer_name());
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        assert_eq!(examine_path(&fifo).unwrap_err(), Rejection::Unreadable);
    }

    #[test]
    fn a_missing_file_is_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join(writer_name());
        assert_eq!(examine_path(&gone).unwrap_err(), Rejection::Unreadable);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn off_linux_even_writer_output_is_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(writer_name());
        std::fs::write(&path, written(&awkward_events()).ndjson).unwrap();
        assert_eq!(examine_path(&path).unwrap_err(), Rejection::Unreadable);
    }

    #[test]
    fn a_file_over_the_limit_is_not_read() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(writer_name());
        let bytes = written(&awkward_events()).ndjson;
        std::fs::write(&path, &bytes).unwrap();
        let len = bytes.len() as u64;
        let mut spent = 0;
        assert_eq!(
            examine(&path, limits(len - 1), &mut spent).unwrap_err(),
            Rejection::TooLong { len }
        );
        assert_eq!(spent, 0);
        assert!(examine(&path, limits(len), &mut spent).is_ok());
        assert_eq!(spent, len);
    }

    #[test]
    fn a_file_with_more_lines_than_allowed_is_read_but_not_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(writer_name());
        let bytes = written(&awkward_events()).ndjson;
        std::fs::write(&path, &bytes).unwrap();
        let lines = awkward_events().len();
        let len = bytes.len() as u64;
        let mut spent = 0;
        let parsed = parsed_lines_for_test();
        let fewer = Limits {
            bytes: len,
            events: lines - 1,
        };
        assert_eq!(
            examine(&path, fewer, &mut spent).unwrap_err(),
            Rejection::TooMany { lines }
        );
        assert_eq!(parsed_lines_for_test(), parsed, "no line parsed");
        assert_eq!(spent, len, "the read counts");
        let exact = Limits {
            bytes: len,
            events: lines,
        };
        assert_eq!(
            examine(&path, exact, &mut spent).unwrap().events.len(),
            lines
        );
    }

    #[test]
    fn a_rejected_file_still_spends_what_was_read() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(writer_name());
        let bytes = with_bad_line_inside(b"\n");
        std::fs::write(&path, &bytes).unwrap();
        let mut spent = 7;
        assert_eq!(
            examine(&path, limits(u64::MAX), &mut spent).unwrap_err(),
            Rejection::Unrecognized
        );
        assert_eq!(spent, 7 + bytes.len() as u64);
    }
}
