//! Saved queries sidebar (F4).

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::app::App;

/// Render the saved queries sidebar.
pub fn render(app: &App, frame: &mut Frame<'_>) {
    let area = centered_rect(80, 80, frame.area());

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Saved Queries (F4) ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));

    if let Some(saved) = &app.saved_cache {
        if saved.queries.is_empty() {
            let text = Line::from("No saved queries yet");
            let paragraph = Paragraph::new(text)
                .block(block)
                .alignment(Alignment::Center);
            frame.render_widget(paragraph, area);
            return;
        }

        // Build list of saved queries
        let items: Vec<ListItem<'_>> = saved
            .queries
            .iter()
            .map(|query| {
                // Format: "name - query (truncated)"
                let line = Line::from(vec![
                    Span::styled(
                        format!("{:25}", query.name),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" - "),
                    Span::styled(
                        truncate_query(&query.query, 80),
                        Style::default().fg(Color::Cyan),
                    ),
                ]);
                ListItem::new(line)
            })
            .collect();

        let footer = Line::from(vec![
            Span::raw("Total: "),
            Span::styled(
                saved.queries.len().to_string(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" saved queries"),
        ]);

        let list = List::new(items)
            .block(block.title_bottom(footer).borders(Borders::ALL))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("► ");

        let mut list_state = ListState::default().with_selected(Some(app.saved_selected_index));

        frame.render_stateful_widget(list, area, &mut list_state);
    } else {
        // Saved queries not loaded
        let text = Line::from("Saved queries not available");
        let paragraph = Paragraph::new(text)
            .block(block)
            .alignment(Alignment::Center);

        frame.render_widget(paragraph, area);
    }
}

/// Truncate query string to max length with ellipsis.
fn truncate_query(query: &str, max_len: usize) -> String {
    if query.len() <= max_len {
        query.to_owned()
    } else {
        format!("{}...", &query[..max_len.saturating_sub(3)])
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    use ratatui::layout::{Constraint, Direction, Layout};

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
