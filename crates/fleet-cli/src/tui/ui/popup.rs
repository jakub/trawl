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
fn render_save_query(frame: &mut Frame<'_>, input: &str) {
    let area = centered_rect(60, 35, frame.area());

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
}

use super::common::centered_rect;
