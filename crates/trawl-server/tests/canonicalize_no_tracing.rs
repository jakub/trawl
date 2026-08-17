// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The telemetry loop guard: `canonicalize` never calls `tracing`
//! (ADR-0013 slice 2, ruling 4).
//!
//! Internal telemetry turns trawld's own tracing events into ordinary
//! ingest records, which means they pass through the canonicalizer on
//! their way to the WAL. A `tracing` call from inside that path emits an
//! event that is canonicalized, which emits an event — an ingestion loop
//! that no rate limiter fixes, because the amplification is per event.
//! Producer failure paths are therefore metrics only (plus rate-limited
//! stderr at the producer, outside these modules).
//!
//! This is a source-level assertion rather than a runtime one because the
//! failure mode is unbounded recursion: a test that PROVOKES it would be
//! the outage. Reviewing a diff is exactly when the invariant is at risk,
//! so the guard belongs where a diff trips it.

/// The two modules the invariant covers: the door and the profile types
/// it reaches on every event.
const GUARDED: [(&str, &str); 2] = [
    (
        "ingest/envelope.rs",
        include_str!("../src/ingest/envelope.rs"),
    ),
    (
        "ingest/producer.rs",
        include_str!("../src/ingest/producer.rs"),
    ),
];

/// Everything from the `#[cfg(test)]` module to the end of file. Unit
/// tests may log freely — they run under no telemetry layer.
const TEST_MODULE_MARKER: &str = "#[cfg(test)]";

/// Strip whole-line comments, so the invariant is about CALLS and not
/// about prose (both modules DOCUMENT the rule, naming `tracing` to do
/// so). Deliberately strict about the rest: a trailing `// tracing`
/// comment would fail this test, which is a cheap false positive next to
/// the alternative of parsing Rust.
fn code_only(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn canonicalize_never_calls_tracing() {
    for (name, source) in GUARDED {
        let production = match source.find(TEST_MODULE_MARKER) {
            Some(at) => &source[..at],
            None => source,
        };
        let code = code_only(production);
        assert!(
            !code.contains("tracing"),
            "{name} names `tracing` outside its test module — telemetry's own \
             events pass through here, so a log line from inside is an \
             ingestion loop (ADR-0013 slice 2, ruling 4). Count it in \
             `crate::metrics` instead."
        );
        // The same reason rules out the macros' short spellings, which
        // reach `tracing` without naming it.
        for macro_name in ["info!", "warn!", "error!", "debug!", "trace!", "event!"] {
            assert!(
                !code.contains(macro_name),
                "{name} calls {macro_name} outside its test module — see above"
            );
        }
    }
}

#[test]
fn the_guard_reads_real_sources() {
    // A guard that silently matched nothing would pass forever. Pin that
    // each module was actually found and that the test-module split is
    // real, so a rename cannot turn this into a no-op.
    for (name, source) in GUARDED {
        assert!(
            source.contains("pub fn canonicalize") || source.contains("pub enum ProducerKind"),
            "{name} is not the module this guard means to cover"
        );
        assert!(
            source.contains(TEST_MODULE_MARKER),
            "{name} lost its unit tests, or the marker changed"
        );
    }
}
