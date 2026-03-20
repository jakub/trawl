// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Help overlay (F1).

use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::tui::App;

/// Render the help overlay.
pub fn render(app: &App, frame: &mut Frame<'_>, scroll: usize) {
    let theme = &app.theme;
    let area = centered_rect(60, 70, frame.area());

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .style(Style::default().bg(theme.surface).fg(theme.text_primary));

    let section_style = Style::default().fg(theme.table_header);
    let help_text = vec![
        Line::from(""),
        Line::from(Span::styled("Navigation", section_style)),
        Line::from(""),
        Line::from("  Alt+1           - Query tab"),
        Line::from("  Alt+2           - History tab"),
        Line::from("  Alt+3           - Schema tab"),
        Line::from("  Alt+4           - Saved tab"),
        Line::from("  Tab             - accept suggestion / cycle focus"),
        Line::from("  Esc             - close panel / cancel / deselect"),
        Line::from(""),
        Line::from(Span::styled("Query Editor", section_style)),
        Line::from(""),
        Line::from("  Shift+Enter     - execute query"),
        Line::from("  Ctrl+Enter      - execute query (alt)"),
        Line::from("  F5              - execute query"),
        Line::from("  Enter           - insert newline"),
        Line::from("  Ctrl+S          - save current query"),
        Line::from("  Ctrl+L          - clear editor"),
        Line::from("  Ctrl+Z/Y        - undo / redo"),
        Line::from("  Ctrl+C/X/V      - copy / cut / paste"),
        Line::from("  Ctrl+A/E        - line start / end"),
        Line::from("  Ctrl+U/K        - kill to line start / end"),
        Line::from("  Ctrl+W          - delete word before cursor"),
        Line::from("  Alt+Left/Right   - word movement"),
        Line::from("  Alt+B / Alt+F   - word movement (readline)"),
        Line::from("  Alt+D           - delete word after cursor"),
        Line::from(""),
        Line::from(Span::styled("Results", section_style)),
        Line::from(""),
        Line::from("  ↑/↓             - navigate rows"),
        Line::from("  ←/→             - scroll columns"),
        Line::from("  Enter           - event detail view"),
        Line::from("  /               - search results"),
        Line::from("  n / N           - next / prev search match"),
        Line::from("  v               - cycle chart view (timechart)"),
        Line::from("  PgUp/PgDn       - page up / down"),
        Line::from("  Home/End        - jump to top / bottom"),
        Line::from(""),
        Line::from(Span::styled("Schema / History / Saved", section_style)),
        Line::from(""),
        Line::from("  ↑/↓             - navigate items"),
        Line::from("  Enter           - expand / load query"),
        Line::from("  ←/→             - collapse / expand tree"),
        Line::from("  /               - filter schema"),
        Line::from("  s               - schedule (Saved tab)"),
        Line::from("  Del             - delete (Saved tab)"),
        Line::from(""),
        Line::from(Span::styled("Other", section_style)),
        Line::from(""),
        Line::from("  Ctrl+Q          - quit"),
        Line::from("  F1              - toggle help"),
        Line::from("  F9              - toggle live tail mode"),
        Line::from("  Shift+Arrow     - select text (editor)"),
        Line::from("  Shift+Ctrl+←/→  - select word"),
        Line::from(""),
        Line::from(Span::styled(
            "Esc to close",
            Style::default().fg(theme.text_muted),
        )),
    ];

    // Clamp scroll to content bounds
    let max_scroll = help_text.len().saturating_sub(1);
    let clamped_scroll = scroll.min(max_scroll);

    #[allow(clippy::cast_possible_truncation)] // scroll offset bounded by content length
    let paragraph = Paragraph::new(help_text)
        .block(block)
        .alignment(Alignment::Left)
        .scroll((clamped_scroll as u16, 0));

    frame.render_widget(paragraph, area);
}

use super::common::centered_rect;
