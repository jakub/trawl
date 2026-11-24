// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]

/// Split a flat stylesheet into its individual top-level rules, dropping
/// blank and comment lines between them. The CSS is un-nested apart
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
