// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Structural guard: fleet-auth reads no host or forwarding header
//! (ADR-0016).
//!
//! Every other test in this crate asserts a verdict. This one asserts an
//! absence, because that is the only way to state the rule that matters:
//! the CSRF decision must not depend on anything a client can type or a
//! proxy must remember to strip. A behavioural test cannot prove it —
//! reading `X-Forwarded-Proto` as a *tiebreak*, or `Host` as a fallback
//! "when `public_origins` looks like a loopback bind", would pass every
//! truth-table row in `session.rs` and quietly hand the verdict back to
//! the network. So this test reads the source instead.
//!
//! Scope: the non-test half of every file in `crates/fleet-auth/src`. The
//! guard stops at the file's `#[cfg(test)]` line on purpose — the unit
//! tests deliberately plant `Host`, `Forwarded` and `X-Forwarded-*`
//! headers to prove they change no verdict, and that is evidence for the
//! same rule, not a violation of it. Comment lines are skipped too: the
//! doc comments explaining why these headers are ignored have to be able
//! to name them.
//!
//! This file lives in `tests/`, so it never scans itself.

use std::fs;
use std::path::Path;

/// Header reads that would put the verdict back in the network's hands.
///
/// Spelled as source fragments rather than header names because that is
/// what a reintroduction would look like: `header::HOST` covers the typed
/// constant under any import path, and the quoted spellings cover
/// `HeaderName::from_static` and raw map lookups. `forwarded` catches
/// `Forwarded`, `X-Forwarded-For`, `X-Forwarded-Host`, `X-Forwarded-Proto`
/// and `X-Forwarded-Port` in one pattern.
const FORBIDDEN_FRAGMENTS: &[&str] = &[
    "header::HOST",
    "\"host\"",
    "\"Host\"",
    "forwarded",
    "Forwarded",
    "FORWARDED",
];

/// Read one source file's production half: everything above the unit-test
/// module, with comment lines dropped.
fn production_lines(path: &Path) -> Vec<(usize, String)> {
    let source =
        fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    source
        .lines()
        .enumerate()
        .take_while(|(_, line)| line.trim() != "#[cfg(test)]")
        .filter(|(_, line)| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("//")
        })
        .map(|(index, line)| (index + 1, line.to_owned()))
        .collect()
}

#[test]
fn no_source_file_reads_a_host_or_forwarding_header() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut scanned = 0_usize;
    let mut findings: Vec<String> = Vec::new();

    for entry in fs::read_dir(&src).expect("read crates/fleet-auth/src") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        scanned += 1;
        for (number, line) in production_lines(&path) {
            for fragment in FORBIDDEN_FRAGMENTS {
                if line.contains(fragment) {
                    findings.push(format!(
                        "{}:{number}: reads {fragment}: {}",
                        path.display(),
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(
        scanned >= 5,
        "expected to scan the crate's sources, found {scanned} files — did the layout move?"
    );
    assert!(
        findings.is_empty(),
        "ADR-0016: the origin verdict is the configured allowlist and the browser's \
         Origin, nothing else. These lines read a header a client or proxy controls:\n{}",
        findings.join("\n")
    );
}
