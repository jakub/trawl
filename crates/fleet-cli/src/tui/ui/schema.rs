//! Schema browser sidebar (F2).
//!
//! Two-level hierarchy: service list → per-service column profile.

use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::tui::state::{CatalogSummary, SchemaView};

use super::common::centered_rect;

/// Render the schema browser sidebar (dispatches on `SchemaView` state).
pub fn render(_app: &crate::tui::App, frame: &mut Frame<'_>, view: &SchemaView) {
    let area = centered_rect(60, 80, frame.area());
    frame.render_widget(Clear, area);

    match view {
        SchemaView::ServiceList {
            services,
            selected,
            catalog,
        } => {
            render_service_list(frame, area, services, *selected, catalog.as_ref());
        }
        SchemaView::Loading { service } => render_loading(frame, area, service),
        SchemaView::ServiceDetail {
            service,
            columns,
            selected,
            scroll: _,
            total_rows,
            total_schema_columns,
            ..
        } => render_service_detail(
            frame,
            area,
            service,
            columns,
            *selected,
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
    catalog: Option<&CatalogSummary>,
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

    // Build header lines: catalog summary (if available) + service count.
    let mut header_lines: Vec<Line<'_>> = Vec::new();

    if let Some(cat) = catalog {
        // Date range line.
        if let (Some(earliest), Some(latest)) = (&cat.earliest_date, &cat.latest_date) {
            header_lines.push(Line::from(Span::styled(
                format!("  {earliest} → {latest}"),
                Style::default().fg(Color::DarkGray),
            )));
        }

        // Storage line.
        let size_str = format_bytes(cat.total_bytes);
        let mut storage = format!("  {size_str} across {} files", cat.file_count);
        if let Some(hot) = cat.hot_buffer_events {
            if hot > 0 {
                use std::fmt::Write;
                let _ = write!(storage, " · {hot} buffered");
            }
        }
        header_lines.push(Line::from(Span::styled(
            storage,
            Style::default().fg(Color::DarkGray),
        )));
    }

    header_lines.push(Line::from(vec![
        Span::styled("  Services", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" ({})", services.len()),
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    let header = ListItem::new(header_lines);

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

    // +1 for the header item
    let mut state = ListState::default().with_selected(Some(selected + 1));
    frame.render_stateful_widget(list, area, &mut state);
}

/// Format a byte count as a human-readable string (e.g. "1.2 GB").
#[allow(clippy::cast_precision_loss)] // display formatting, precision loss at >4 PB is fine
fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * KIB;
    const GIB: f64 = 1024.0 * MIB;

    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KB", b / KIB)
    } else {
        format!("{bytes} B")
    }
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
    total_rows: usize,
    total_schema_columns: usize,
) {
    let title = format!(" {service} ");
    let footer = Line::from(vec![
        Span::styled("⏎", Style::default().fg(Color::Cyan)),
        Span::raw(" insert  "),
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

    for col in columns {
        let pct = col.population_pct();
        let pct_color = if pct >= 80 {
            Color::Green
        } else if pct >= 50 {
            Color::Yellow
        } else {
            Color::Red
        };

        let line = Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:22}", col.name), Style::default().fg(Color::Cyan)),
            Span::styled(
                format!("{:12}", col.data_type),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(format!("{pct:>3}%"), Style::default().fg(pct_color)),
        ]);

        if col.sample_values.is_empty() {
            items.push(ListItem::new(line));
        } else {
            let values_str = col.sample_values.join("  ");
            let values_line = Line::from(Span::styled(
                format!("    {values_str}"),
                Style::default().fg(Color::DarkGray),
            ));
            items.push(ListItem::new(vec![line, values_line]));
        }
    }

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(Color::DarkGray));

    // +1 for the header line
    let mut state = ListState::default().with_selected(Some(selected + 1));
    frame.render_stateful_widget(list, area, &mut state);
}
