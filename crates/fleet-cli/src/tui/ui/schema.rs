//! Schema browser sidebar (F2).
//!
//! Two-level hierarchy: service list → per-service column profile.

use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::tui::state::SchemaView;

use super::common::centered_rect;

/// Render the schema browser sidebar (dispatches on `SchemaView` state).
pub fn render(_app: &crate::tui::App, frame: &mut Frame<'_>, view: &SchemaView) {
    let area = centered_rect(60, 80, frame.area());
    frame.render_widget(Clear, area);

    match view {
        SchemaView::ServiceList { services, selected } => {
            render_service_list(frame, area, services, *selected);
        }
        SchemaView::Loading { service } => render_loading(frame, area, service),
        SchemaView::ServiceDetail {
            service,
            columns,
            selected,
            scroll: _,
            expanded,
            total_rows,
            total_schema_columns,
        } => render_service_detail(
            frame,
            area,
            service,
            columns,
            *selected,
            *expanded,
            *total_rows,
            *total_schema_columns,
        ),
    }
}

/// Render the top-level service list.
fn render_service_list(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    services: &[String],
    selected: usize,
) {
    let title = " Schema Browser (F2) ";
    let footer = Line::from(vec![
        Span::raw("↑↓ select  "),
        Span::styled("⏎", Style::default().fg(Color::Cyan)),
        Span::raw(" drill  "),
        Span::styled("Esc", Style::default().fg(Color::Cyan)),
        Span::raw(" close"),
    ]);

    let block = Block::default()
        .title(title)
        .title_bottom(footer)
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));

    if services.is_empty() {
        let text = Line::from("No services found");
        let paragraph = Paragraph::new(text)
            .block(block)
            .alignment(Alignment::Center);
        frame.render_widget(paragraph, area);
        return;
    }

    let header = ListItem::new(Line::from(vec![
        Span::styled("Services", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" ({})", services.len()),
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    let mut items: Vec<ListItem<'_>> = vec![header];
    for svc in services {
        items.push(ListItem::new(Line::from(Span::styled(
            format!("  {svc}"),
            Style::default().fg(Color::Cyan),
        ))));
    }

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(Color::DarkGray));

    // +1 for the header line
    let mut state = ListState::default().with_selected(Some(selected + 1));
    frame.render_stateful_widget(list, area, &mut state);
}

/// Render the loading spinner.
fn render_loading(frame: &mut Frame<'_>, area: ratatui::layout::Rect, service: &str) {
    let block = Block::default()
        .title(" Schema Browser (F2) ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));

    let text = Line::from(vec![
        Span::raw("Loading "),
        Span::styled(service, Style::default().fg(Color::Cyan)),
        Span::raw("..."),
    ]);

    let paragraph = Paragraph::new(text)
        .block(block)
        .alignment(Alignment::Center);

    frame.render_widget(paragraph, area);
}

/// Render the per-service column profile.
#[allow(clippy::too_many_arguments)]
fn render_service_detail(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    service: &str,
    columns: &[crate::tui::state::ProfiledColumn],
    selected: usize,
    expanded: Option<usize>,
    total_rows: usize,
    total_schema_columns: usize,
) {
    let title = format!(" {service} ");
    let footer = Line::from(vec![
        Span::styled("⏎", Style::default().fg(Color::Cyan)),
        Span::raw(" insert  "),
        Span::styled("␣", Style::default().fg(Color::Cyan)),
        Span::raw(" values  "),
        Span::styled("Esc", Style::default().fg(Color::Cyan)),
        Span::raw(" ← back"),
    ]);

    let block = Block::default()
        .title(title)
        .title_bottom(footer)
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));

    if columns.is_empty() {
        let text = Line::from("No populated columns");
        let paragraph = Paragraph::new(text)
            .block(block)
            .alignment(Alignment::Center);
        frame.render_widget(paragraph, area);
        return;
    }

    // Header line.
    let header = ListItem::new(Line::from(vec![
        Span::styled(
            format!("{total_rows} sampled"),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!(" · {}/{total_schema_columns} fields", columns.len()),
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    let mut items: Vec<ListItem<'_>> = vec![header];

    for (idx, col) in columns.iter().enumerate() {
        let pct = col.population_pct();
        let pct_color = if pct >= 80 {
            Color::Green
        } else if pct >= 50 {
            Color::Yellow
        } else {
            Color::Red
        };

        let expand_marker = if expanded == Some(idx) { "▼ " } else { "  " };

        let line = Line::from(vec![
            Span::raw(expand_marker),
            Span::styled(format!("{:22}", col.name), Style::default().fg(Color::Cyan)),
            Span::styled(
                format!("{:12}", col.data_type),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(format!("{pct:>3}%"), Style::default().fg(pct_color)),
        ]);

        if expanded == Some(idx) && !col.sample_values.is_empty() {
            // Multi-line item: column info + sample values.
            let values_str = col.sample_values.join("  ");
            let values_line = Line::from(Span::styled(
                format!("    {values_str}"),
                Style::default().fg(Color::DarkGray),
            ));
            items.push(ListItem::new(vec![line, values_line]));
        } else {
            items.push(ListItem::new(line));
        }
    }

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(Color::DarkGray));

    // +1 for the header line
    let mut state = ListState::default().with_selected(Some(selected + 1));
    frame.render_stateful_widget(list, area, &mut state);
}
