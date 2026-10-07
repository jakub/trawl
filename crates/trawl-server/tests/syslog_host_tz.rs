// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A zone-less syslog timestamp never reads in the host's zone (ADR-0050).
//!
//! The host zone is per process, so each reading happens in a CHILD
//! PROCESS: this test binary re-executes itself with the ignored
//! [`syslog_host_tz_child`] test selected and `TZ` set. The child resolves
//! the packaged syslog settings, where `default_timezone` is unset, parses
//! fixed frames at fixed arrivals, and writes the instants to a file the
//! parent owns. The parent runs one child under `America/Chicago` and one
//! under `Asia/Kolkata` and asserts both read every frame as UTC.
//!
//! Each child also reports `chrono::Local`'s offset at each arrival, and the
//! parent asserts it is the zone `TZ` names. Without that control a runner
//! with no zone database would read both children as UTC and the test would
//! pass without proving anything.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use chrono::{DateTime, Offset, TimeZone, Utc};
use trawl_server::config::SyslogConfig;
use trawl_server::syslog::SyslogPeers;
use trawl_server::syslog::parse::parse_syslog;

/// Set only in the child: the file the child writes its readings to.
const CHILD_OUT: &str = "SYSLOG_HOST_TZ_CHILD_OUT";

/// The dynamic loader's search path, the one part of the parent's
/// environment the child keeps.
const LOADER_PATH: [&str; 3] = [
    "LD_LIBRARY_PATH",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
];

/// The peer every frame comes from. No setting names it.
const PEER: &str = "192.0.2.10";

/// (frame, arrival, the instant its wall clock denotes in UTC). Winter and
/// summer, year-less and with-year, so DST in either host zone would show.
const FRAMES: [(&str, &str, &str); 4] = [
    (
        "<13>Jan 15 12:00:00 host app: x",
        "2026-01-15T18:00:00Z",
        "2026-01-15T12:00:00Z",
    ),
    (
        "<13>Jan 15 2026 12:00:00 host app: x",
        "2026-01-15T18:00:00Z",
        "2026-01-15T12:00:00Z",
    ),
    (
        "<13>Jul 15 12:00:00 host app: x",
        "2026-07-15T18:00:00Z",
        "2026-07-15T12:00:00Z",
    ),
    (
        "<13>Jul 15 2026 12:00:00 host app: x",
        "2026-07-15T18:00:00Z",
        "2026-07-15T12:00:00Z",
    ),
];

fn instant(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

/// The subprocess entry point. Without [`CHILD_OUT`] it does nothing, so a
/// run with `--run-ignored` passes it untouched.
///
/// Writes one line per frame: the host's `chrono::Local` offset in seconds
/// at the arrival, the instant the frame stored, and the zone it was read
/// in.
#[test]
#[ignore = "entry point for the host-TZ child process; the test below runs it"]
fn syslog_host_tz_child() {
    let Some(out) = std::env::var_os(CHILD_OUT) else {
        return;
    };
    let peers = SyslogPeers::resolve(&SyslogConfig::default()).expect("the defaults resolve");
    let zone = peers.zones.for_peer(PEER.parse().unwrap());
    let mut lines = String::new();
    for (raw, arrival, _) in FRAMES {
        let arrival = instant(arrival);
        let local = chrono::Local
            .offset_from_utc_datetime(&arrival.naive_utc())
            .fix()
            .local_minus_utc();
        let parsed = parse_syslog(raw, arrival, zone);
        let stored = parsed.msg.timestamp.map_or_else(
            || "none".to_owned(),
            |ts| ts.with_timezone(&Utc).to_rfc3339(),
        );
        let read_in = parsed
            .read_in
            .map_or_else(|| "none".to_owned(), |zone| zone.to_string());
        writeln!(lines, "{local} {stored} {read_in}").expect("write to a String");
    }
    std::fs::write(out, lines).expect("write the child's readings");
}

/// Run the child under `tz` and return its readings, one per frame.
fn readings_under(tz: &str, dir: &Path) -> Vec<(i32, String, String)> {
    let out = dir.join(tz.replace('/', "_"));
    let mut command = Command::new(std::env::current_exe().expect("locate this test binary"));
    command
        .args([
            "syslog_host_tz_child",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env_clear()
        .env("TZ", tz)
        .env(CHILD_OUT, &out);
    // The test binary links libduckdb dynamically, and the runner points the
    // loader at it. Only the loader's search path crosses over.
    for name in LOADER_PATH {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let output = command.output().expect("run the child");
    let written = std::fs::read_to_string(&out).unwrap_or_else(|e| {
        panic!(
            "the {tz} child wrote no readings ({e}); status {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    });
    assert!(output.status.success(), "the {tz} child failed");
    written
        .lines()
        .map(|line| {
            let mut fields = line.split(' ');
            let local = fields.next().unwrap().parse().unwrap();
            let stored = fields.next().unwrap().to_owned();
            let read_in = fields.next().unwrap().to_owned();
            (local, stored, read_in)
        })
        .collect()
}

#[test]
fn syslog_zone_is_independent_of_host_tz() {
    let dir = tempfile::tempdir().unwrap();
    // (TZ, the host offset a working zone database gives at each arrival).
    let hosts = [
        ("America/Chicago", [-21_600, -21_600, -18_000, -18_000]),
        ("Asia/Kolkata", [19_800, 19_800, 19_800, 19_800]),
    ];
    let mut seen = Vec::new();
    for (tz, offsets) in hosts {
        let readings = readings_under(tz, dir.path());
        assert_eq!(readings.len(), FRAMES.len(), "{tz}");
        for ((local, stored, read_in), ((raw, _, expected), offset)) in
            readings.iter().zip(FRAMES.iter().zip(offsets))
        {
            assert_eq!(
                *local, offset,
                "{tz}: the child's host zone is not the one TZ names, so this \
                 run proves nothing about host independence"
            );
            assert_eq!(
                instant(stored),
                instant(expected),
                "{tz}: {raw} must read as UTC"
            );
            assert_eq!(read_in, "UTC", "{tz}: {raw}");
        }
        seen.push(
            readings
                .into_iter()
                .map(|(_, stored, _)| stored)
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(seen[0], seen[1], "the two hosts stored different instants");
}
