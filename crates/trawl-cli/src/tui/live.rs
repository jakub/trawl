// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live streaming (SSE tail) — toggle, start, stop, and helper functions.

use std::time::Duration;

use futures::StreamExt;
use trawl_client::StreamEvent;

use super::state::LiveBuffer;
use super::{App, QueryError, QueryResult};

impl App {
    /// Toggle live tail mode (F9).
    pub(crate) fn toggle_live_mode(&mut self) {
        if self.live_mode {
            self.stop_live_stream();
        } else {
            self.start_live_stream();
        }
    }

    /// Start live streaming with the current query.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn start_live_stream(&mut self) {
        let query = self.active_tab().editor.text().trim().to_owned();

        if query.is_empty() {
            tracing::warn!("cannot start live stream with empty query");
            return;
        }

        tracing::info!("starting live stream for query: {}", query);

        // Cancel any existing stream. Its last snapshot's rows stay drawn
        // until a replacement result arrives (a quiet restarted stream may
        // send none), so the cap report stays with them; every result
        // replaces it.
        if let Some(task) = self.live_task.take() {
            task.abort();
        }

        // Extract field order from the DSL so the live buffer preserves it.
        let field_order = extract_field_order(&query);

        // Resolve timezone offset for timestamp display in streaming events.
        let utc_offset_secs =
            trawl_engine::timezone::resolve_utc_offset(&self.timezone).unwrap_or(0);

        let client = self.client.clone();
        let tx = self.query_tx.clone();
        let max_events = self.max_live_events;

        let task = tokio::spawn(async move {
            tracing::info!("live stream task started");

            let mut stream = match client.stream_events(&query).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("failed to start stream: {e}");
                    let _ = tx.send(QueryResult {
                        result: Err(QueryError {
                            message: e.to_string(),
                            details: e.error_details().to_vec(),
                        }),
                        duration: Duration::from_secs(0),
                        capped: Vec::new(),
                    });
                    return;
                }
            };

            let mut buffer = LiveBuffer::new(max_events);
            if !field_order.is_empty() {
                buffer = buffer.with_column_order(field_order);
            }
            let mut event_count = 0usize;

            while let Some(event) = stream.next().await {
                match event {
                    Ok(StreamEvent::Event(mut frame)) => {
                        apply_tz_to_frame(&mut frame, utc_offset_secs);
                        buffer.push_frame(&frame);
                        event_count += 1;
                        tracing::debug!("received stream event, total: {event_count}");
                    }
                    Ok(StreamEvent::Snapshot { mut result, capped }) => {
                        apply_tz_to_frame(&mut result, utc_offset_secs);
                        buffer.replace_with_snapshot(&result, &capped);
                        // Each snapshot replaces the whole result — send immediately.
                        let response = buffer.to_query_response();
                        let _ = tx.send(QueryResult {
                            result: Ok(response),
                            duration: Duration::from_secs(0),
                            capped: buffer.capped().to_vec(),
                        });
                    }
                    Ok(StreamEvent::Error(msg)) => {
                        tracing::error!("stream error event: {msg}");
                    }
                    Ok(StreamEvent::Lagged(n)) => {
                        tracing::warn!(
                            event_type = "stream_lagged",
                            missed = n,
                            "stream subscriber fell behind"
                        );
                    }
                    Err(e) => {
                        tracing::error!("stream error: {e}");
                        break;
                    }
                }

                // Send snapshot every 10 events.
                if event_count.is_multiple_of(10) && event_count > 0 {
                    let response = buffer.to_query_response();
                    tracing::debug!(
                        "sending live buffer snapshot ({} rows)",
                        response.result.row_count()
                    );

                    let _ = tx.send(QueryResult {
                        result: Ok(response),
                        duration: Duration::from_secs(0),
                        capped: buffer.capped().to_vec(),
                    });
                }
            }

            // Flush remaining events.
            if !buffer.is_empty() {
                let response = buffer.to_query_response();
                let _ = tx.send(QueryResult {
                    result: Ok(response),
                    duration: Duration::from_secs(0),
                    capped: buffer.capped().to_vec(),
                });
            }

            tracing::info!("live stream task ended");
        });

        self.live_task = Some(task);
        self.live_mode = true;
    }

    /// Stop live streaming.
    pub(crate) fn stop_live_stream(&mut self) {
        tracing::info!("stopping live stream");

        if let Some(task) = self.live_task.take() {
            task.abort();
        }

        self.live_mode = false;
    }
}

/// Extract the user-specified field order from a `fields`/`table` pipe stage.
///
/// Returns the column names verbatim, in the order the user wrote them, or an empty
/// vec when the query has no such stage or fails to parse.
fn extract_field_order(query: &str) -> Vec<String> {
    let Ok(ast) = trawl_core::parser::parse(query) else {
        return Vec::new();
    };
    for stage in &ast.pipeline {
        if let trawl_core::ast::PipeStage::Table(t) = &stage.node {
            return t
                .fields
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
        }
    }
    Vec::new()
}

/// Apply the display offset to a live frame's timestamp cells.
///
/// The stream sends UTC, where a batch query asks the server for the
/// configured offset, so the TUI applies it here, to every column the
/// frame types `timestamp` (a list of timestamps included) and to nothing
/// else: a sender's text that merely looks like a time stays as sent. A
/// cell chrono cannot read, such as an infinity, stays as sent too.
fn apply_tz_to_frame(frame: &mut trawl_engine::value::QueryResult, utc_offset_secs: i32) {
    if utc_offset_secs == 0 {
        return;
    }
    let trawl_engine::value::QueryResult { columns, rows } = frame;
    for row in rows {
        for (column, cell) in columns.iter().zip(row.iter_mut()) {
            shift_timestamps(&column.ty, cell, utc_offset_secs);
        }
    }
}

/// [`apply_tz_to_frame`] for one cell of a column typed `ty`.
fn shift_timestamps(
    ty: &trawl_engine::value::ColumnType,
    cell: &mut trawl_engine::value::Value,
    utc_offset_secs: i32,
) {
    use trawl_engine::value::{ColumnType, Value};
    match (ty, cell) {
        (ColumnType::Timestamp, Value::String(text)) => {
            if let Some(shifted) = trawl_engine::timezone::rfc3339_at_offset(text, utc_offset_secs)
            {
                *text = shifted;
            }
        }
        (ColumnType::List { items }, Value::Array(elements)) => {
            for element in elements {
                shift_timestamps(items, element, utc_offset_secs);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_tz_to_frame, extract_field_order};
    use trawl_engine::value::{Column, ColumnType, QueryResult, Value};

    /// The display offset moves every `timestamp` cell, a list's elements
    /// included, and leaves a `string` that looks like a time, an
    /// infinity, and a null alone. At offset zero nothing changes.
    #[test]
    fn the_display_offset_shifts_timestamp_columns_only() {
        let text = |s: &str| Value::String(s.to_owned());
        let frame = QueryResult {
            columns: vec![
                Column::new("_time", ColumnType::Timestamp),
                Column::new("seen", ColumnType::list(ColumnType::Timestamp)),
                Column::new("timestamp", ColumnType::String),
                Column::new("ends", ColumnType::Timestamp),
                Column::new("_ingested", ColumnType::Timestamp),
            ],
            rows: vec![vec![
                text("2026-01-15T09:00:00.000000Z"),
                Value::Array(vec![text("2026-01-15T23:30:00.500000Z")]),
                text("2026-01-15T09:00:00Z"),
                text("infinity"),
                Value::Null,
            ]],
        };

        let mut utc = frame.clone();
        apply_tz_to_frame(&mut utc, 0);
        assert_eq!(utc, frame);

        let mut shifted = frame;
        apply_tz_to_frame(&mut shifted, 19_800);
        assert_eq!(
            shifted.rows,
            vec![vec![
                text("2026-01-15T14:30:00.000000+05:30"),
                Value::Array(vec![text("2026-01-16T05:00:00.500000+05:30")]),
                text("2026-01-15T09:00:00Z"),
                text("infinity"),
                Value::Null,
            ]]
        );
        // The grid then shows the wall clock at that offset.
        assert_eq!(
            crate::tui::ui::results::cell_text(&ColumnType::Timestamp, &shifted.rows[0][0]),
            "2026-01-15 14:30:00"
        );
    }

    /// Zero-alias contract (ADR-0013): the live column order carries the user's
    /// own spellings verbatim. An alias mapping here would make `table
    /// timestamp` consume `_time` while the batch lane stayed green.
    #[test]
    fn table_field_names_are_verbatim_and_distinct() {
        assert_eq!(extract_field_order("* | table _time"), vec!["_time"]);
        assert_eq!(
            extract_field_order("* | table timestamp"),
            vec!["timestamp"]
        );
        assert_eq!(
            extract_field_order("* | table @timestamp"),
            vec!["@timestamp"]
        );
    }

    #[test]
    fn table_field_order_follows_the_user() {
        assert_eq!(
            extract_field_order("* | table timestamp, _time, host"),
            vec!["timestamp", "_time", "host"]
        );
        assert_eq!(
            extract_field_order("* | table host, _time, timestamp"),
            vec!["host", "_time", "timestamp"]
        );
    }

    #[test]
    fn no_table_stage_or_unparseable_query_yields_no_order() {
        assert!(extract_field_order("* | head 5").is_empty());
        assert!(extract_field_order("| | |").is_empty());
    }
}
