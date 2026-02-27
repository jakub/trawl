//! Help overlay (F1).

use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::tui::App;

/// Render the help overlay.
pub fn render(_app: &App, frame: &mut Frame<'_>, scroll: usize) {
    let area = centered_rect(60, 70, frame.area());

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));

    let help_text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "Ctrl — App Actions",
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
        Line::from("  Ctrl+Q          - quit"),
        Line::from("  Ctrl+T          - new tab"),
        Line::from("  Ctrl+W          - close tab / delete word (editor)"),
        Line::from("  Ctrl+S          - save current query"),
        Line::from("  Ctrl+L          - clear editor"),
        Line::from("  Ctrl+Enter      - execute query"),
        Line::from("  Ctrl+Z/Y        - undo / redo"),
        Line::from("  Ctrl+C/X/V      - copy / cut / paste"),
        Line::from("  Ctrl+A/E        - line start / end"),
        Line::from("  Ctrl+U/K        - kill to line start / end"),
        Line::from(""),
        Line::from(Span::styled(
            "Alt — Navigation",
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
        Line::from("  Alt+[ / Alt+]   - prev / next tab"),
        Line::from("  Alt+1-9         - jump to tab N"),
        Line::from("  Alt+Left/Right   - word movement (editor)"),
        Line::from("  Alt+B / Alt+F   - word movement (readline)"),
        Line::from("  Alt+D           - delete word after cursor"),
        Line::from(""),
        Line::from(Span::styled(
            "F-Keys — Mode Triggers",
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
        Line::from("  F1              - toggle help"),
        Line::from("  F2              - toggle schema browser"),
        Line::from("  F3              - toggle query history"),
        Line::from("  F4              - toggle saved queries"),
        Line::from("  F5              - execute query"),
        Line::from("  F9              - toggle live tail mode"),
        Line::from(""),
        Line::from(Span::styled(
            "Bare Keys — Context Actions",
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
        Line::from("  Tab             - cycle focus (editor → results → sidebar)"),
        Line::from("  Esc             - close popup / cancel / deselect"),
        Line::from("  Enter           - execute (editor) / detail (results) / load (sidebar)"),
        Line::from("  Shift+Enter     - insert newline (editor)"),
        Line::from("  [ / ]           - prev / next sidebar section"),
        Line::from("  /               - search results / filter schema"),
        Line::from("  n / N           - next / prev search match"),
        Line::from("  v               - cycle chart view (timechart results)"),
        Line::from("  ↑/↓             - navigate rows / items"),
        Line::from("  ←/→             - scroll columns / expand-collapse tree"),
        Line::from("  PgUp/PgDn       - page up / down"),
        Line::from("  Home/End        - jump to top / bottom"),
        Line::from(""),
        Line::from(Span::styled(
            "Shift — Selection",
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
        Line::from("  Shift+Arrow     - select text (editor)"),
        Line::from("  Shift+Ctrl+←/→  - select word"),
        Line::from("  Shift+Alt+←/→   - select word (macOS)"),
        Line::from("  Shift+Home/End  - select to line start/end"),
        Line::from(""),
        Line::from(Span::styled(
            "Esc to close · Tab to cycle focus",
            Style::default().fg(Color::DarkGray),
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
