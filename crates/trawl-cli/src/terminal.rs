// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Sender and server text on its way to the operator's terminal
//! (ADR-0039, amendment of 2026-11-09).
//!
//! Field names and event values are chosen by whoever can ingest, so every
//! sink that can reach a terminal shows them through
//! [`trawl_core::sanitize`]. A prose line sanitizes each value it
//! interpolates at its own site. A table draws each header and cell through
//! [`table_cell`].

use trawl_core::sanitize::sanitize_display_text;

/// A table header or cell as the table draws it, after value presentation.
///
/// A line break, `\n` or `\r\n`, stays a line inside the cell: comfy-table
/// splits a cell on `\n` and draws its borders on every line, so a break
/// cannot leave the cell. Every other unsafe character on a line becomes
/// U+FFFD, a tab and a lone `\r` included. Tables are a human format, so
/// this holds wherever the table goes, an `-o` file included.
pub(crate) fn table_cell(text: &str) -> String {
    text.replace("\r\n", "\n")
        .split('\n')
        .map(sanitize_display_text)
        .collect::<Vec<_>>()
        .join("\n")
}
