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

        // Cancel any existing stream.
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
                    Ok(StreamEvent::Event(mut map)) => {
                        apply_tz_to_event(&mut map, utc_offset_secs);
                        buffer.push_event(&map);
                        event_count += 1;
                        tracing::debug!("received stream event, total: {event_count}");
                    }
                    Ok(StreamEvent::Snapshot {
                        ref columns,
                        ref rows,
                    }) => {
                        let rows: Vec<_> = rows
                            .iter()
                            .cloned()
                            .map(|mut row| {
                                apply_tz_to_event(&mut row, utc_offset_secs);
                                row
                            })
                            .collect();
                        buffer.replace_with_snapshot(columns, &rows);
                        // Snapshots are complete results — send immediately.
                        let response = buffer.to_query_response();
                        let _ = tx.send(QueryResult {
                            result: Ok(response),
                            duration: Duration::from_secs(0),
                        });
                    }
                    Ok(StreamEvent::Row(_)) => {
                        // Legacy variant — shouldn't appear from SSE stream.
                        tracing::debug!("ignoring unexpected Row variant in live stream");
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
                    });
                }
            }

            // Flush remaining events.
            if !buffer.is_empty() {
                let response = buffer.to_query_response();
                let _ = tx.send(QueryResult {
                    result: Ok(response),
                    duration: Duration::from_secs(0),
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
/// Returns the mapped column names in user-specified order, or empty vec if
/// no table stage is present (or the query fails to parse).
fn extract_field_order(query: &str) -> Vec<String> {
    let Ok(ast) = trawl_core::parser::parse(query) else {
        return Vec::new();
    };
    for stage in &ast.pipeline {
        if let trawl_core::ast::PipeStage::Table(t) = &stage.node {
            return t
                .fields
                .iter()
                .map(|f| trawl_core::emitter::map_field_name(f).to_string())
                .collect();
        }
    }
    Vec::new()
}

/// Apply timezone offset to timestamp-valued fields in a streaming event.
///
/// Converts RFC 3339 UTC strings to the display format used by the query
/// executor, with the configured UTC offset applied. Handles both the
/// standard `timestamp` field and timechart's `_time` bucket field.
fn apply_tz_to_event(event: &mut serde_json::Map<String, serde_json::Value>, utc_offset_secs: i32) {
    if utc_offset_secs == 0 {
        return;
    }
    for key in &["timestamp", "_time"] {
        if let Some(serde_json::Value::String(ts)) = event.get(*key) {
            let converted = trawl_engine::timezone::reformat_rfc3339(ts, utc_offset_secs);
            event.insert((*key).to_owned(), serde_json::Value::String(converted));
        }
    }
}
