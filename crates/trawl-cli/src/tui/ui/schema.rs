// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Schema browser detail pane (right side of horizontal split).
//!
//! Renders contextual statistics for the currently selected tree node:
//! service overview with sparklines, or field stats with sample values.

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Sparkline, Wrap};

use crate::tui::state::SchemaBrowser;

/// Determine what is selected in the tree and render the appropriate detail.
pub fn render_detail_pane(schema: &SchemaBrowser, frame: &mut Frame<'_>, area: Rect) {
    let selection = resolve_selection(schema);
    match selection {
        Selection::None => {
            let p = Paragraph::new("select a service or field")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(p, area);
        }
        Selection::CommonHeader => render_common_header_detail(schema, frame, area),
        Selection::CommonField { name } => render_field_detail(schema, name, None, frame, area),
        Selection::Service { name } => render_service_detail(schema, name, frame, area),
        Selection::ServiceField { service, field } => {
            render_field_detail(schema, field, Some(service), frame, area);
        }
    }
}

enum Selection<'a> {
    None,
    CommonHeader,
    CommonField { name: &'a str },
    Service { name: &'a str },
    ServiceField { service: &'a str, field: &'a str },
}

/// Walk the flattened tree to find what the cursor points at.
fn resolve_selection(schema: &SchemaBrowser) -> Selection<'_> {
    let filter = schema.filter.to_lowercase();
    let common_names: HashSet<&str> = schema
        .common_fields
        .iter()
        .map(|f| f.name.as_str())
        .collect();

    let mut idx = 0usize;
    let target = schema.selected;

    // Common header + fields.
    if !schema.common_fields.is_empty() {
        let visible_common: Vec<_> = schema
            .common_fields
            .iter()
            .filter(|f| filter.is_empty() || f.name.to_lowercase().contains(&filter))
            .collect();

        if !visible_common.is_empty() || filter.is_empty() {
            if idx == target {
                return Selection::CommonHeader;
            }
            idx += 1;
            for f in &visible_common {
                if idx == target {
                    return Selection::CommonField { name: &f.name };
                }
                idx += 1;
            }
        }
    }

    // Services.
    for svc in &schema.services {
        let unique_fields: Vec<_> = svc
            .columns
            .iter()
            .filter(|c| !common_names.contains(c.name.as_str()))
            .collect();

        if !filter.is_empty() {
            let svc_matches = svc.name.to_lowercase().contains(&filter);
            let fields_match = unique_fields
                .iter()
                .any(|c| c.name.to_lowercase().contains(&filter));
            if !svc_matches && !fields_match {
                continue;
            }
        }

        if idx == target {
            return Selection::Service { name: &svc.name };
        }
        idx += 1;

        if schema.expanded.contains(&svc.name) {
            for col in &unique_fields {
                if idx == target {
                    return Selection::ServiceField {
                        service: &svc.name,
                        field: &col.name,
                    };
                }
                idx += 1;
            }
        }
    }

    Selection::None
}

/// Render overview for the common fields header.
fn render_common_header_detail(schema: &SchemaBrowser, frame: &mut Frame<'_>, area: Rect) {
    let mut lines = vec![
        Line::from(Span::styled(
            "common fields",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "\u{2500}".repeat(area.width.min(35) as usize),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(format!("fields:     {}", schema.common_fields.len())),
        Line::from(format!("services:   {}", schema.services.len())),
        Line::default(),
    ];

    for f in &schema.common_fields {
        lines.push(Line::from(vec![
            Span::styled(&f.name, Style::default().fg(Color::Cyan)),
            Span::styled(
                format!("  ({})", f.data_type),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, area);
}

/// Render service overview with sparkline.
#[allow(clippy::too_many_lines)]
fn render_service_detail(
    schema: &SchemaBrowser,
    service_name: &str,
    frame: &mut Frame<'_>,
    area: Rect,
) {
    let Some(svc) = schema.services.iter().find(|s| s.name == service_name) else {
        return;
    };

    let common_names: HashSet<&str> = schema
        .common_fields
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    let unique_count = svc
        .columns
        .iter()
        .filter(|c| !common_names.contains(c.name.as_str()))
        .count();
    let common_count = svc.columns.len() - unique_count;

    let events_str = format_count(svc.total_events);
    let bytes_str = format_bytes(svc.total_bytes);

    // Compute approximate rate.
    let days = svc.daily_event_counts.len().max(1);
    #[allow(clippy::cast_precision_loss)]
    let daily_rate = svc.total_events as f64 / days as f64;
    let hourly_rate = daily_rate / 24.0;

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let daily_rate_u = daily_rate.max(0.0) as u64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let hourly_rate_u = hourly_rate.max(0.0) as u64;

    let mut lines = vec![
        Line::from(Span::styled(
            service_name,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "\u{2500}".repeat(area.width.min(35) as usize),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(format!("events:    {events_str}")),
        Line::from(format!(
            "rate:      ~{}/day  ~{}/hr",
            format_count(daily_rate_u),
            format_count(hourly_rate_u)
        )),
    ];

    if let (Some(earliest), Some(latest)) = (&svc.earliest_date, &svc.latest_date) {
        lines.push(Line::from(format!(
            "data:      {earliest} \u{2192} {latest}"
        )));
    }

    lines.push(Line::from(format!(
        "files:     {} ({bytes_str})",
        svc.file_count
    )));
    lines.push(Line::default());

    // Sparkline for daily events.
    if !svc.daily_event_counts.is_empty() {
        lines.push(Line::from(Span::styled(
            "daily events:",
            Style::default().fg(Color::DarkGray),
        )));
    }

    // Field breakdown.
    lines.push(Line::default());
    lines.push(Line::from(format!(
        "fields: {} total ({unique_count} unique)",
        svc.columns.len()
    )));

    if common_count > 0 {
        let common_list: String = schema
            .common_fields
            .iter()
            .filter(|f| svc.columns.iter().any(|c| c.name == f.name))
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(Line::from(vec![
            Span::styled("  common: ", Style::default().fg(Color::DarkGray)),
            Span::styled(common_list, Style::default().fg(Color::White)),
        ]));
    }

    if unique_count > 0 {
        let unique_list: String = svc
            .columns
            .iter()
            .filter(|c| !common_names.contains(c.name.as_str()))
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(Line::from(vec![
            Span::styled("  unique: ", Style::default().fg(Color::DarkGray)),
            Span::styled(unique_list, Style::default().fg(Color::White)),
        ]));
    }

    // Split area: text above, sparkline with axis labels below (if data).
    if svc.daily_event_counts.is_empty() {
        let p = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(p, area);
    } else {
        // Layout: sparkline (2, y-labels in gutter) + x-axis (1)
        // y-max in gutter of top row, y-min in gutter of bottom row.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3)])
            .split(area);

        let p = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(p, chunks[0]);

        let data: Vec<u64> = svc.daily_event_counts.iter().map(|d| d.count).collect();
        let max_val = data.iter().copied().max().unwrap_or(0);
        let min_val = data.iter().copied().min().unwrap_or(0);
        let first_date = &svc.daily_event_counts[0].date;
        let last_date = &svc.daily_event_counts[svc.daily_event_counts.len() - 1].date;
        let max_label = format_count(max_val);
        let min_label = format_count(min_val);

        // Y-axis labels are right-justified in a small gutter, sparkline fills the rest.
        let gutter: u16 = 5; // enough for "1.2K " or "12.3K"

        let spark_area = chunks[1];
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // sparkline (y-max/y-min in gutter)
                Constraint::Length(1), // x-axis dates
            ])
            .split(spark_area);

        // Sparkline: 2 rows tall, offset past gutter.
        let spark_rect = Rect {
            x: rows[0].x + gutter + 1,
            width: rows[0].width.saturating_sub(gutter + 1),
            ..rows[0]
        };
        let target_width = spark_rect.width as usize;
        let display_data = resample(&data, target_width);
        let sparkline = Sparkline::default()
            .data(&display_data)
            .style(Style::default().fg(Color::Cyan));
        frame.render_widget(sparkline, spark_rect);

        // Y-max label in the gutter of the sparkline's top row.
        let ymax_rect = Rect {
            width: gutter,
            height: 1,
            ..rows[0]
        };
        let y_max = Line::from(Span::styled(
            format!("{max_label:>gutter$}", gutter = gutter as usize),
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(Paragraph::new(y_max), ymax_rect);

        // Y-min label in the gutter of the sparkline's bottom row.
        let ymin_rect = Rect {
            width: gutter,
            y: rows[0].y + 1,
            height: 1,
            ..rows[0]
        };
        let y_min = Line::from(Span::styled(
            format!("{min_label:>gutter$}", gutter = gutter as usize),
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(Paragraph::new(y_min), ymin_rect);

        // X-axis dates, indented past gutter.
        #[allow(clippy::cast_possible_truncation)]
        let x_width = rows[1].width.saturating_sub(gutter + 1) as usize;
        let date_pad = x_width.saturating_sub(first_date.len() + last_date.len());
        let x_line = format!(
            "{:>gutter$} {first_date}{}{last_date}",
            "",
            " ".repeat(date_pad),
            gutter = gutter as usize,
        );
        let x_axis = Line::from(Span::styled(x_line, Style::default().fg(Color::DarkGray)));
        frame.render_widget(Paragraph::new(x_axis), rows[1]);
    }
}

/// Render field detail (common or service-scoped).
fn render_field_detail(
    schema: &SchemaBrowser,
    field_name: &str,
    service: Option<&str>,
    frame: &mut Frame<'_>,
    area: Rect,
) {
    let mut lines = vec![
        Line::from(Span::styled(
            field_name,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "\u{2500}".repeat(area.width.min(35) as usize),
            Style::default().fg(Color::DarkGray),
        )),
    ];

    // Find the column stats.
    if let Some(svc_name) = service {
        // Service-scoped field.
        if let Some(svc) = schema.services.iter().find(|s| s.name == svc_name)
            && let Some(col) = svc.columns.iter().find(|c| c.name == field_name)
        {
            lines.push(Line::from(format!("type:       {}", col.data_type)));
            lines.push(Line::from(format!("in:         {svc_name}")));

            if col.total_count > 0 {
                let non_null = col.total_count - col.null_count;
                lines.push(Line::from(format!(
                    "non-null:   {}",
                    format_non_null_pct(non_null, col.total_count)
                )));
            }

            if let (Some(min_v), Some(max_v)) = (&col.min_value, &col.max_value) {
                push_range_lines(&mut lines, min_v, max_v);
            }

            if col.compressed_bytes > 0 {
                lines.push(Line::from(format!(
                    "storage:    {}",
                    format_bytes(col.compressed_bytes)
                )));
            }
        }
    } else if let Some(cf) = schema.common_fields.iter().find(|f| f.name == field_name) {
        // Common field — aggregate across services.
        lines.push(Line::from(format!("type:       {}", cf.data_type)));
        lines.push(Line::from(format!(
            "in:         {} services",
            cf.service_count
        )));

        if cf.total_count > 0 {
            let non_null = cf.total_count - cf.null_count;
            lines.push(Line::from(format!(
                "non-null:   {}",
                format_non_null_pct(non_null, cf.total_count)
            )));
        }

        if let (Some(min_v), Some(max_v)) = (&cf.min_value, &cf.max_value) {
            push_range_lines(&mut lines, min_v, max_v);
        }
    }

    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, area);
}

// -- format helpers ----------------------------------------------------------

/// Trim trailing fractional-zero microseconds from timestamp-like strings.
///
/// `"2026-03-01 00:00:00.000000"` → `"2026-03-01 00:00:00"`
/// `"2026-03-01 12:34:56.123000"` → `"2026-03-01 12:34:56.123"`
/// Non-timestamp strings pass through unchanged.
fn trim_timestamp(s: &str) -> &str {
    // Only trim if it looks like a timestamp (contains a dot after a time-like pattern).
    if let Some(dot_pos) = s.rfind('.') {
        let after_dot = &s[dot_pos + 1..];
        if !after_dot.is_empty() && after_dot.bytes().all(|b| b == b'0') {
            return &s[..dot_pos];
        }
        // Trim trailing zeros but keep at least one digit after dot.
        let trimmed = s.trim_end_matches('0');
        if trimmed.ends_with('.') {
            return &s[..dot_pos];
        }
        return trimmed;
    }
    s
}

/// Render a range value across one or two lines.
///
/// If `min – max` fits on one line after the label, render inline.
/// Otherwise split across two lines with the continuation indented.
fn push_range_lines(lines: &mut Vec<Line<'_>>, min_v: &str, max_v: &str) {
    let min_t = trim_timestamp(min_v);
    let max_t = trim_timestamp(max_v);
    // "range:      " is 12 chars
    let inline = format!("{min_t} \u{2013} {max_t}");
    if inline.len() <= 40 {
        lines.push(Line::from(format!("range:      {inline}")));
    } else {
        lines.push(Line::from(format!("range:      {min_t} \u{2013}")));
        lines.push(Line::from(format!("            {max_t}")));
    }
}

/// Format non-null percentage with "< 0.1%" for near-zero values.
fn format_non_null_pct(non_null: u64, total: u64) -> String {
    if non_null == 0 {
        return "0%".to_owned();
    }
    #[allow(clippy::cast_precision_loss)]
    let pct = (non_null as f64 / total as f64) * 100.0;
    if pct < 0.1 {
        format!("< 0.1% ({})", format_count(non_null))
    } else {
        format!("{pct:.1}% ({})", format_count(non_null))
    }
}

/// Human-readable byte size (KB, MB, GB).
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;

    if bytes >= GB {
        #[allow(clippy::cast_precision_loss)]
        let val = bytes as f64 / GB as f64;
        format!("{val:.1} GB")
    } else if bytes >= MB {
        #[allow(clippy::cast_precision_loss)]
        let val = bytes as f64 / MB as f64;
        format!("{val:.1} MB")
    } else if bytes >= KB {
        #[allow(clippy::cast_precision_loss)]
        let val = bytes as f64 / KB as f64;
        format!("{val:.1} KB")
    } else {
        format!("{bytes} B")
    }
}

/// Human-readable event count (K, M, B).
fn format_count(count: u64) -> String {
    if count >= 1_000_000_000 {
        #[allow(clippy::cast_precision_loss)]
        let val = count as f64 / 1_000_000_000.0;
        format!("{val:.1}B")
    } else if count >= 1_000_000 {
        #[allow(clippy::cast_precision_loss)]
        let val = count as f64 / 1_000_000.0;
        format!("{val:.1}M")
    } else if count >= 1_000 {
        #[allow(clippy::cast_precision_loss)]
        let val = count as f64 / 1_000.0;
        format!("{val:.1}K")
    } else {
        count.to_string()
    }
}

/// Resample a data series to exactly `target_len` points using linear interpolation.
///
/// When data has fewer points than terminal columns, stretches to fill;
/// when more, compresses. Returns the original data unchanged if lengths match.
fn resample(data: &[u64], target_len: usize) -> Vec<u64> {
    if data.is_empty() || target_len == 0 {
        return vec![0; target_len];
    }
    if data.len() == target_len {
        return data.to_vec();
    }

    let src_len = data.len();
    let mut out = Vec::with_capacity(target_len);

    for i in 0..target_len {
        // Map output index to a fractional position in the source.
        #[allow(clippy::cast_precision_loss)]
        let src_pos = if target_len == 1 {
            0.0
        } else {
            (i as f64) * ((src_len - 1) as f64) / ((target_len - 1) as f64)
        };

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let lo = (src_pos as usize).min(src_len - 1);
        let hi = (lo + 1).min(src_len - 1);
        #[allow(clippy::cast_precision_loss)]
        let frac = src_pos - lo as f64;

        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let val = (data[lo] as f64 * (1.0 - frac) + data[hi] as f64 * frac).round() as u64;
        out.push(val);
    }

    out
}
