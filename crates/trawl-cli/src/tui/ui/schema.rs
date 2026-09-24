// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Schema browser detail pane (right side of horizontal split).
//!
//! Renders statistics for the selected tree node: a service overview with a
//! daily-event chart, or a field's type, coverage and value range.

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Chart, Dataset, GraphType, Paragraph, Wrap};

use crate::tui::state::SchemaBrowser;
use crate::tui::theme::Theme;

/// Determine what is selected in the tree and render the appropriate detail.
pub fn render_detail_pane(
    schema: &SchemaBrowser,
    frame: &mut Frame<'_>,
    area: Rect,
    theme: &Theme,
) {
    let selection = resolve_selection(schema);
    match selection {
        Selection::None => {
            let p = Paragraph::new("select a service or field")
                .style(Style::default().fg(theme.text_muted));
            frame.render_widget(p, area);
        }
        Selection::CommonHeader => render_common_header_detail(schema, frame, area, theme),
        Selection::CommonField { name } => {
            render_field_detail(schema, name, None, frame, area, theme);
        }
        Selection::Service { name } => render_service_detail(schema, name, frame, area, theme),
        Selection::ServiceField { service, field } => {
            render_field_detail(schema, field, Some(service), frame, area, theme);
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
fn render_common_header_detail(
    schema: &SchemaBrowser,
    frame: &mut Frame<'_>,
    area: Rect,
    theme: &Theme,
) {
    let mut lines = vec![
        Line::from(Span::styled(
            "common fields",
            Style::default()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "\u{2500}".repeat(area.width.min(35) as usize),
            Style::default().fg(theme.text_muted),
        )),
        Line::from(format!("fields:     {}", schema.common_fields.len())),
        Line::from(format!("services:   {}", schema.services.len())),
        Line::default(),
    ];

    for f in &schema.common_fields {
        lines.push(Line::from(vec![
            Span::styled(&f.name, Style::default().fg(theme.text_accent)),
            Span::styled(
                format!("  ({})", f.data_type),
                Style::default().fg(theme.text_muted),
            ),
        ]));
    }

    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, area);
}

/// Render service overview with braille chart.
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
fn render_service_detail(
    schema: &SchemaBrowser,
    service_name: &str,
    frame: &mut Frame<'_>,
    area: Rect,
    theme: &Theme,
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
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "\u{2500}".repeat(area.width.min(35) as usize),
            Style::default().fg(theme.text_muted),
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

    // Labels the chart at the bottom of the pane; the field breakdown
    // renders between the label and the chart.
    if !svc.daily_event_counts.is_empty() {
        lines.push(Line::from(Span::styled(
            "daily events:",
            Style::default().fg(theme.text_muted),
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
            Span::styled("  common: ", Style::default().fg(theme.text_muted)),
            Span::styled(common_list, Style::default().fg(theme.text_primary)),
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
            Span::styled("  unique: ", Style::default().fg(theme.text_muted)),
            Span::styled(unique_list, Style::default().fg(theme.text_primary)),
        ]));
    }

    // Split area: text above, braille chart below (if data).
    if svc.daily_event_counts.is_empty() {
        let p = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(p, area);
    } else {
        let [text_area, chart_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(5)]).areas(area);

        let p = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(p, text_area);

        let data: Vec<u64> = svc.daily_event_counts.iter().map(|d| d.count).collect();
        let max_val = data.iter().copied().max().unwrap_or(0);
        let min_val = data.iter().copied().min().unwrap_or(0);
        let first_date = &svc.daily_event_counts[0].date;
        let last_date = &svc.daily_event_counts[svc.daily_event_counts.len() - 1].date;
        let max_label = format_count(max_val);
        let min_label = format_count(min_val);

        // Resample to braille resolution (2 points per column)
        let chart_width = chart_area.width.saturating_sub(8) as usize; // axis labels
        let target = chart_width.saturating_mul(2).max(1);
        let display_data = resample(&data, target);

        #[allow(clippy::cast_precision_loss)]
        let points: Vec<(f64, f64)> = display_data
            .iter()
            .enumerate()
            .map(|(i, &v)| (i as f64, v as f64))
            .collect();

        let x_max = (display_data.len().saturating_sub(1)) as f64;
        let y_min_f = min_val as f64;
        let y_max_f = if max_val == min_val {
            max_val as f64 + 1.0
        } else {
            max_val as f64
        };

        let dataset = Dataset::default()
            .data(&points)
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(theme.text_accent));

        let chart = Chart::new(vec![dataset])
            .x_axis(
                Axis::default()
                    .style(Style::default().fg(theme.text_muted))
                    .bounds([0.0, x_max.max(1.0)])
                    .labels(vec![
                        Span::raw(first_date.clone()),
                        Span::raw(last_date.clone()),
                    ]),
            )
            .y_axis(
                Axis::default()
                    .style(Style::default().fg(theme.text_muted))
                    .bounds([y_min_f, y_max_f])
                    .labels(vec![Span::raw(min_label), Span::raw(max_label)]),
            );

        frame.render_widget(chart, chart_area);
    }
}

/// Render field detail (common or service-scoped).
fn render_field_detail(
    schema: &SchemaBrowser,
    field_name: &str,
    service: Option<&str>,
    frame: &mut Frame<'_>,
    area: Rect,
    theme: &Theme,
) {
    let mut lines = vec![
        Line::from(Span::styled(
            field_name,
            Style::default()
                .fg(theme.text_accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "\u{2500}".repeat(area.width.min(35) as usize),
            Style::default().fg(theme.text_muted),
        )),
    ];

    // Find the column stats.
    if let Some(svc_name) = service {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::compute_common_fields;

    fn service(name: &str, bounds: &[(&str, &str, &str)]) -> trawl_api::ServiceSchema {
        trawl_api::ServiceSchema {
            name: name.to_owned(),
            columns: bounds
                .iter()
                .map(|(column, min, max)| trawl_api::ServiceColumnStats {
                    name: (*column).to_owned(),
                    data_type: "TIMESTAMP".to_owned(),
                    null_count: 0,
                    total_count: 10,
                    min_value: Some((*min).to_owned()),
                    max_value: Some((*max).to_owned()),
                    compressed_bytes: 0,
                })
                .collect(),
            earliest_date: None,
            latest_date: None,
            file_count: 1,
            total_bytes: 1024,
            total_events: 10,
            daily_event_counts: vec![],
            degraded_fields: Vec::new(),
        }
    }

    fn bounds(
        services: &[trawl_api::ServiceSchema],
        field: &str,
    ) -> (Option<String>, Option<String>) {
        let common = compute_common_fields(services);
        let f = common
            .iter()
            .find(|f| f.name == field)
            .unwrap_or_else(|| panic!("common field {field}"));
        (f.min_value.clone(), f.max_value.clone())
    }

    fn range_lines(min: &str, max: &str) -> Vec<String> {
        let mut lines = Vec::new();
        push_range_lines(&mut lines, min, max);
        lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn schema_time_bounds_merge_and_render() {
        // `_time` as the server now renders it for compacted (local)
        // TIMESTAMP columns: fixed-width, no `Z`. The pre-epoch bound
        // must win the min across services, in either service order.
        let old = service(
            "old",
            &[(
                "_time",
                "1969-12-31T23:59:59.500000",
                "2026-01-02T03:04:05.123000",
            )],
        );
        let new = service(
            "new",
            &[(
                "_time",
                "2026-01-01T00:00:00.000000",
                "2026-09-24T18:25:30.654321",
            )],
        );
        let want = (
            Some("1969-12-31T23:59:59.500000".to_owned()),
            Some("2026-09-24T18:25:30.654321".to_owned()),
        );
        assert_eq!(bounds(&[old.clone(), new.clone()], "_time"), want);
        assert_eq!(bounds(&[new.clone(), old.clone()], "_time"), want);

        // Mixed shapes have no common range, whatever the order and
        // whatever comes after: local beside UTC, and a timestamp beside a
        // service still holding epoch integers.
        let utc = service(
            "utc",
            &[(
                "_time",
                "2026-01-01T00:00:00.000000Z",
                "2026-01-01T00:00:01.000000Z",
            )],
        );
        let int = service("int", &[("_time", "1767225600000000", "1767225601000000")]);
        for order in [
            vec![old.clone(), utc.clone()],
            vec![utc.clone(), old.clone()],
            vec![old.clone(), int.clone()],
            vec![int.clone(), old.clone()],
            vec![old.clone(), int.clone(), new.clone()],
        ] {
            let names: Vec<_> = order.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(bounds(&order, "_time"), (None, None), "{names:?}");
        }

        // Non-timestamp text keeps the plain comparison.
        let a = service("a", &[("_time", "200", "404")]);
        let b = service("b", &[("_time", "1000", "503")]);
        assert_eq!(
            bounds(&[a, b], "_time"),
            (Some("1000".to_owned()), Some("503".to_owned()))
        );

        // Rendered: the fixed-width text reads as a time with trailing
        // fractional zeros trimmed; a `Z` sample renders as sent.
        let (min, max) = want;
        assert_eq!(
            range_lines(&min.unwrap(), &max.unwrap()),
            [
                "range:      1969-12-31T23:59:59.5 \u{2013}",
                "            2026-09-24T18:25:30.654321",
            ]
        );
        assert_eq!(
            range_lines("2026-01-01T00:00:00.000000Z", "2026-01-01T00:00:01.000000Z"),
            [
                "range:      2026-01-01T00:00:00.000000Z \u{2013}",
                "            2026-01-01T00:00:01.000000Z",
            ]
        );
    }
}
