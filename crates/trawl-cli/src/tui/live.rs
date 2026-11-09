// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live streaming (SSE tail) — toggle, start, stop, and helper functions.

use std::time::Duration;

use futures::StreamExt;
use trawl_client::StreamEvent;

use super::state::{Execution, LiveBuffer, RunSource, TabStatus};
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

        // End the earlier run. An earlier stream's last snapshot's rows stay
        // drawn until a replacement result arrives (a quiet restarted stream
        // may send none), so the cap report stays with them; every result
        // replaces it. A batch's Running status goes: nothing is left to
        // cancel.
        let was_batch = self.active_tab().execution.is_batch();
        self.end_run("replaced by live stream");
        if was_batch {
            self.active_tab_mut().status = TabStatus::Idle;
        }
        let generation = self.tab.next_generation();

        // Extract field order from the DSL so the live buffer preserves it.
        let field_order = extract_field_order(&query);

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
                        generation,
                        source: RunSource::Live,
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
                    Ok(StreamEvent::Event(frame)) => {
                        buffer.push_frame(&frame);
                        event_count += 1;
                        tracing::debug!("received stream event, total: {event_count}");
                    }
                    Ok(StreamEvent::Snapshot { result, capped }) => {
                        buffer.replace_with_snapshot(&result, &capped);
                        // Each snapshot replaces the whole result — send immediately.
                        let response = buffer.to_query_response();
                        let _ = tx.send(QueryResult {
                            result: Ok(response),
                            duration: Duration::from_secs(0),
                            capped: buffer.capped().to_vec(),
                            generation,
                            source: RunSource::Live,
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
                        generation,
                        source: RunSource::Live,
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
                    generation,
                    source: RunSource::Live,
                });
            }

            tracing::info!("live stream task ended");
        });

        self.active_tab_mut().execution = Execution::Live {
            generation,
            handle: task,
        };
        self.live_mode = true;
    }

    /// Stop live streaming. A running batch is not a stream and keeps going.
    pub(crate) fn stop_live_stream(&mut self) {
        tracing::info!("stopping live stream");

        if matches!(self.active_tab().execution, Execution::Live { .. }) {
            self.end_run("stopped");
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

#[cfg(test)]
mod tests {
    use super::extract_field_order;

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
