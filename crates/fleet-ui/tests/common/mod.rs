// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared CSS-text helpers for fleet-ui's stylesheet contract tests.
//!
//! Several tests read the shipped `fleet-ui.css` (and the pre-migration
//! fixture) as text and assert structural invariants over it. They all
//! need the same primitive — chop a flat stylesheet into whole rules —
//! so it lives here once rather than being re-derived (weakly) per test
//! binary.

#![allow(dead_code)] // each test binary uses a subset of these items

/// Split a flat stylesheet into its individual top-level rules, dropping
/// blank and comment lines between them. A rule runs from its selector
/// line to the line where brace depth returns to zero, so single-line
/// rules, multi-line rules, and `@keyframes` / `@supports` blocks each
/// come out whole (nested rules included in their wrapper — re-run this
/// on the wrapper's inner text to descend). The CSS is un-nested apart
/// from at-rules, so this stays simple.
pub fn rules(css: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut depth: i32 = 0;
    for line in css.lines() {
        if depth == 0 {
            let t = line.trim_start();
            if t.is_empty() || t.starts_with("/*") || t.starts_with('*') {
                continue;
            }
        }
        cur.push(line);
        for ch in line.chars() {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
        if depth == 0 && !cur.is_empty() {
            out.push(cur.join("\n"));
            cur.clear();
        }
    }
    out
}
