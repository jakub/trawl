//! Popup overlays (confirmations, text input).

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;
use crate::state::Popup;

/// Render the active popup (if any).
pub fn render(app: &App, frame: &mut Frame<'_>) {
    if let Some(popup) = &app.popup {
        match popup {
            Popup::ConfirmDelete { name, .. } => {
                render_confirm_delete(frame, name);
            }
            Popup::SaveQuery { input } => {
                render_save_query(frame, input);
            }
        }
    }
}

/// Render confirmation dialog for deleting a saved query.
fn render_confirm_delete(frame: &mut Frame<'_>, name: &str) {
    let area = centered_rect(60, 30, frame.area());

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Confirm Delete ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::Red));

    let text = vec![
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
            "Press Y or Enter to confirm, any other key to cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let paragraph = Paragraph::new(text)
        .block(block)
        .alignment(Alignment::Center);

    frame.render_widget(paragraph, area);
}

/// Render text input dialog for saving a query.
fn render_save_query(frame: &mut Frame<'_>, input: &str) {
    let area = centered_rect(60, 30, frame.area());

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Save Query ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::Cyan));

    let text = vec![
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
            "Press Enter to save, Esc to cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let paragraph = Paragraph::new(text)
        .block(block)
        .alignment(Alignment::Center);

    frame.render_widget(paragraph, area);
}

/// Helper to create a centered rect.
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
