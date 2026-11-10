// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! History detail pane (right side of horizontal split).
//!
//! Shows metadata and syntax-highlighted query for the selected history entry.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use trawl_api::zone::{DisplayZone, present_human};
use trawl_core::sanitize::sanitize_display_text;

use crate::tui::App;
use crate::tui::highlight::Highlighter;

/// Render the detail pane for the selected history entry.
pub fn render_detail_pane(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let theme = &app.theme;
    let selected = app.panel.history_selected;
    let Some(ref history) = app.history_cache else {
        return;
    };
    let Some(entry) = history.entries.get(selected) else {
        return;
    };

    let highlighter = Highlighter::new(app.schema_cache.as_ref(), &app.theme.syntax);

    let mut lines: Vec<Line<'static>> = Vec::new();

    // -- status badge --
    let (status_label, status_color) = match entry.status {
        trawl_client::QueryStatus::Success => ("success", theme.status_success),
        trawl_client::QueryStatus::Error => ("error", theme.status_error),
        trawl_client::QueryStatus::Timeout => ("timeout", theme.status_warning),
    };
    lines.push(Line::from(Span::styled(
        status_label,
        Style::default()
            .fg(status_color)
            .add_modifier(Modifier::BOLD),
    )));

    // -- separator --
    #[allow(clippy::cast_possible_truncation)]
    let sep_width = area.width.min(35) as usize;
    lines.push(Line::from(Span::styled(
        "\u{2500}".repeat(sep_width),
        Style::default().fg(theme.text_muted),
    )));

    // -- metadata --
    let label_style = Style::default().fg(theme.text_muted);
    let value_style = Style::default().fg(theme.text_primary);
    lines.push(Line::from(vec![
        Span::styled("duration   ", label_style),
        Span::styled(format_duration_ms(entry.duration_ms), value_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled("rows       ", label_style),
        Span::styled(entry.row_count.to_string(), value_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled("executed   ", label_style),
        Span::styled(
            format_full_timestamp(&entry.executed_at, app.display_zone),
            value_style,
        ),
    ]));

    lines.push(Line::default());

    // -- syntax-highlighted query (line by line) --
    for query_line in entry.query.lines() {
        lines.push(highlighter.highlight_line(query_line));
    }

    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, area);
}

/// Format milliseconds into a human-readable duration.
fn format_duration_ms(ms: u64) -> String {
    if ms >= 60_000 {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    } else if ms >= 1000 {
        format!("{}.{}s", ms / 1000, (ms % 1000) / 100)
    } else {
        format!("{ms}ms")
    }
}

/// A wire timestamp in `zone`, with its offset and any fraction. Text that is
/// not a wire timestamp shows as sent, sanitized, rather than guessed at.
fn format_full_timestamp(wire: &str, zone: DisplayZone) -> String {
    present_human(wire, zone).unwrap_or_else(|| sanitize_display_text(wire))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_ms_under_second() {
        assert_eq!(format_duration_ms(0), "0ms");
        assert_eq!(format_duration_ms(42), "42ms");
        assert_eq!(format_duration_ms(999), "999ms");
    }

    #[test]
    fn duration_ms_seconds() {
        assert_eq!(format_duration_ms(1000), "1.0s");
        assert_eq!(format_duration_ms(1200), "1.2s");
        assert_eq!(format_duration_ms(59_999), "59.9s");
    }

    #[test]
    fn duration_ms_minutes() {
        assert_eq!(format_duration_ms(60_000), "1m 0s");
        assert_eq!(format_duration_ms(90_000), "1m 30s");
        assert_eq!(format_duration_ms(150_000), "2m 30s");
    }

    fn zone(text: &str) -> DisplayZone {
        text.parse().unwrap()
    }

    #[test]
    fn full_timestamp_valid() {
        assert_eq!(
            format_full_timestamp("2026-03-18T14:30:00Z", zone("UTC")),
            "2026-03-18 14:30:00 +00:00"
        );
        assert_eq!(
            format_full_timestamp("2026-03-18T14:30:00Z", zone("+05:30")),
            "2026-03-18 20:00:00 +05:30"
        );
    }

    #[test]
    fn full_timestamp_with_fractional() {
        let wire = "2026-03-18T14:30:00.123456Z";
        assert_eq!(
            format_full_timestamp(wire, zone("+05:30")),
            "2026-03-18 20:00:00.123456 +05:30"
        );
        assert_eq!(
            format_full_timestamp(wire, zone("UTC")),
            "2026-03-18 14:30:00.123456 +00:00"
        );
    }

    /// Text that is not a wire timestamp shows as sent, with no guessed
    /// instant, and cannot drive the terminal.
    #[test]
    fn full_timestamp_malformed_fallback() {
        assert_eq!(format_full_timestamp("garbage", zone("+05:30")), "garbage");
        assert_eq!(
            format_full_timestamp("2026-03-18\u{202E}Z", zone("UTC")),
            "2026-03-18\u{FFFD}Z"
        );
    }
}
