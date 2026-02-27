//! Popup overlays (confirmations, text input).

use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::tui::App;
use crate::tui::state::Popup;

/// Render the active popup (if any).
pub fn render(app: &App, frame: &mut Frame<'_>) {
    if let Some(popup) = &app.popup {
        match popup {
            Popup::Help { scroll } => {
                crate::tui::ui::help::render(app, frame, *scroll);
            }
            Popup::ConfirmDelete { name, .. } => {
                render_confirm_delete(frame, name);
            }
            Popup::SaveQuery { editor } => {
                render_save_query(frame, editor);
            }
            Popup::EventDetail { row_index, scroll } => {
                render_event_detail(app, frame, *row_index, *scroll);
            }
            Popup::Error { message } => {
                render_error(frame, message);
            }
        }
    }
}

/// Render confirmation dialog for deleting a saved query.
fn render_confirm_delete(frame: &mut Frame<'_>, name: &str) {
    let area = centered_rect(60, 35, frame.area());

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Confirm Delete ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::Red));

    let text = vec![
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "Delete saved query?",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            name,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "Press Y or Enter to confirm",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "Any other key to cancel",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
    ];

    let paragraph = Paragraph::new(text)
        .block(block)
        .alignment(Alignment::Center)
        .wrap(ratatui::widgets::Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

/// Render text input dialog for saving a query.
fn render_save_query(frame: &mut Frame<'_>, editor: &crate::tui::state::SimpleEditor) {
    let area = centered_rect(60, 35, frame.area());
    let input = editor.text();

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Save Query ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::Cyan));

    let text = vec![
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "Enter a name for this query:",
            Style::default().fg(Color::White),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("> {input}█"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "Press Enter to save",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "Esc to cancel",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
    ];

    let paragraph = Paragraph::new(text)
        .block(block)
        .alignment(Alignment::Center)
        .wrap(ratatui::widgets::Wrap { trim: false });

    frame.render_widget(paragraph, area);

    // Position the cursor inside the input field.
    let cursor_col = editor.cursor.1;
    #[allow(clippy::cast_possible_truncation)]
    let cursor_x =
        area.x + (area.width / 2).saturating_sub((input.len() as u16) / 2) + 2 + cursor_col as u16;
    let cursor_y = area.y + 5; // Row of the input line within the popup
    frame.set_cursor_position((cursor_x, cursor_y));
}

/// Render the event detail popup (key-value view of a single row).
fn render_event_detail(app: &App, frame: &mut Frame<'_>, row_index: usize, scroll: usize) {
    let area = centered_rect(70, 80, frame.area());
    frame.render_widget(Clear, area);

    let tab = app.active_tab();
    let Some(response) = &tab.result else {
        return;
    };

    let result = &response.result;
    let Some(row_data) = result.rows.get(row_index) else {
        return;
    };

    let title = format!(" Event Detail (row {}) ", row_index + 1);
    let block = Block::default()
        .title(title)
        .title_bottom(" ↑↓: scroll | []: prev/next row | Esc: close ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));

    // Build key-value lines
    let mut lines: Vec<Line<'_>> = Vec::new();
    lines.push(Line::from(""));

    // Find max field name width for alignment
    let max_name_len = result
        .columns
        .iter()
        .map(|c| c.name.len())
        .max()
        .unwrap_or(0);

    for (i, col) in result.columns.iter().enumerate() {
        let value = row_data
            .get(i)
            .map(super::results::value_to_string)
            .unwrap_or_default();

        lines.push(Line::from(vec![
            Span::styled(
                format!("{:>width$}  ", col.name, width = max_name_len),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(value),
        ]));
    }
    lines.push(Line::from(""));

    let max_scroll = lines.len().saturating_sub(1);
    let clamped_scroll = scroll.min(max_scroll);

    #[allow(clippy::cast_possible_truncation)]
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(ratatui::widgets::Wrap { trim: false })
        .scroll((clamped_scroll as u16, 0));

    frame.render_widget(paragraph, area);
}

/// Render an error message popup.
fn render_error(frame: &mut Frame<'_>, message: &str) {
    let area = centered_rect(60, 30, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Error ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::Red));

    let text = vec![
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(message, Style::default().fg(Color::Red))),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "Press Esc to dismiss",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
    ];

    let paragraph = Paragraph::new(text)
        .block(block)
        .alignment(Alignment::Center)
        .wrap(ratatui::widgets::Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

use super::common::centered_rect;
