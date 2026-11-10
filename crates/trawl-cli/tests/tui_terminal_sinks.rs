// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What sender-chosen text does on its way through the TUI to an
//! operator's terminal.
//!
//! A real trawld stores one event whose message holds a bidi override
//! (U+202E), a zero-width space (U+200B) and a soft hyphen (U+00AD), each
//! between plain markers. The real `trawl` TUI runs on a PTY and runs a
//! batch query whose own text holds none of them, then opens the event
//! detail popup on the event's row. The popup draws U+FFFD in place of
//! each, one for one. Neither the driver's capture nor the bytes the TUI
//! wrote to its terminal hold any of the three. Every step waits on a
//! driver reply; nothing sleeps.

mod common;

use serde_json::json;

use common::Server;
use common::tui::{Home, READER_PERMISSIONS, Tui, pinned};

/// The service the event arrives under, and a query that finds it. The
/// query projects no `_time`, so the result opens in the table view.
const SERVICE: &str = "tui_sink_probe";
const QUERY: &str = "service=tui_sink_probe | table message";

/// The message a sender chose, and how the TUI draws it.
const HOSTILE: &str = "zqx\u{202e}1\u{200b}2\u{00ad}3";
const SHOWN: &str = "zqx\u{fffd}1\u{fffd}2\u{fffd}3";

/// The characters no terminal may receive from the event.
const FORBIDDEN: [char; 3] = ['\u{202e}', '\u{200b}', '\u{00ad}'];

/// The rows inside the event detail popup's border, each cut to the
/// columns between its side edges. The popup's top edge is the row that
/// holds its title. A capture line is the grid row in quotes, one
/// character per cell, so a character's index is its column plus one.
fn popup_rows(screen: &str) -> Vec<String> {
    let grid: Vec<Vec<char>> = screen.lines().map(|l| l.chars().collect()).collect();
    let (top, title) = grid
        .iter()
        .enumerate()
        .find_map(|(y, row)| {
            let text: String = row.iter().collect();
            let at = text.find(" Event Detail (")?;
            Some((y, text[..at].chars().count()))
        })
        .unwrap_or_else(|| panic!("no Event Detail popup:\n{}", screen.escape_debug()));
    let left = title - 1;
    assert_eq!(
        grid[top][left],
        '┌',
        "the popup title does not start its top edge:\n{}",
        screen.escape_debug()
    );
    let right = (title..grid[top].len())
        .find(|&x| grid[top][x] == '┐')
        .unwrap_or_else(|| panic!("no top-right corner:\n{}", screen.escape_debug()));
    let bottom = (top + 1..grid.len())
        .find(|&y| grid[y].get(left) == Some(&'└'))
        .unwrap_or_else(|| panic!("no bottom edge:\n{}", screen.escape_debug()));
    grid[top + 1..bottom]
        .iter()
        .map(|row| {
            row.get(left + 1..right)
                .unwrap_or_else(|| panic!("a short popup row:\n{}", screen.escape_debug()))
                .iter()
                .collect()
        })
        .collect()
}

/// Each forbidden character `text` holds, with the line it is on.
fn forbidden_in(text: &str) -> Vec<String> {
    text.lines()
        .enumerate()
        .flat_map(|(n, line)| {
            line.chars()
                .filter(|c| FORBIDDEN.contains(c))
                .map(move |c| format!("{c:?} on line {}: {}", n + 1, line.escape_debug()))
        })
        .collect()
}

/// The event detail popup draws a sender's bidi override, zero-width space
/// and soft hyphen as U+FFFD, and the TUI writes none of them to its
/// terminal, while the result it holds keeps the value as sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_terminal_sinks_reach_no_terminal_control() {
    let server = Server::start(true).await;
    server
        .ingest(&[json!({ "service": SERVICE, "message": HOSTILE })])
        .await;
    server.compact().await;
    let token = server.mint("tui-sink-reader", &READER_PERMISSIONS).await;

    let home = Home::new();
    let config = home.config(&pinned(&server, &token), "UTC");
    let mut tui = Tui::start(&home, "UTC", &config).await;
    tui.batch(QUERY).await;

    // The result keeps the value as sent: only the drawing replaces it.
    let results = tui.send(json!({ "cmd": "get_results" })).await;
    let column = results["columns"]
        .as_array()
        .and_then(|columns| columns.iter().position(|c| c == "message"))
        .unwrap_or_else(|| panic!("no message column: {results}"));
    let rows = results["rows"].as_array().expect("result rows");
    assert_eq!(rows.len(), 1, "one row expected: {results}");
    assert_eq!(
        rows[0][column], HOSTILE,
        "the result's message is not the value as sent: {results}"
    );

    let status = tui.send(json!({ "cmd": "status" })).await;
    if status["focus"] != "results" {
        tui.send(json!({ "cmd": "key", "key": "tab" })).await;
    }
    tui.send(json!({ "cmd": "key", "key": "down" })).await;
    tui.send(json!({ "cmd": "key", "key": "enter" })).await;
    let screen = tui.capture().await;

    let popup = popup_rows(&screen);
    assert!(
        popup.iter().any(|row| row.contains(SHOWN)),
        "the popup does not draw {SHOWN:?}:\n{}",
        popup.join("\n").escape_debug()
    );
    let found = forbidden_in(&screen);
    assert!(
        found.is_empty(),
        "the capture holds sender controls:\n{}",
        found.join("\n")
    );

    tui.send(json!({ "cmd": "quit" })).await;
    let (status, terminal) = tui.wait().await;
    assert!(
        status.success(),
        "the TUI exited {status}\n{}",
        terminal.escape_debug()
    );
    // The terminal got the message, drawn: the check after this one reads
    // a stream that carried it.
    assert!(
        terminal.contains(SHOWN),
        "the TUI never wrote {SHOWN:?} to its terminal:\n{}",
        terminal.escape_debug()
    );
    let found = forbidden_in(&terminal);
    assert!(
        found.is_empty(),
        "the TUI wrote sender controls to its terminal:\n{}",
        found.join("\n")
    );

    server.stop().await;
}
