// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ndjson query debug log.
//!
//! One JSON object per query execution, capturing DSL, generated SQL,
//! source paths, hot buffer state, result sample, and timing. Designed
//! for `tail -f /tmp/trawl-query.log | jq` debugging workflows.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write as _};
use std::path::Path;
use std::sync::Mutex;

use serde::Serialize;
use serde_json::Value as JsonValue;

/// Append-only ndjson query debug log.
pub struct QueryLog {
    writer: Mutex<BufWriter<File>>,
}

impl QueryLog {
    /// Open (or create) a query log file in append mode.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    /// Serialize and append an entry. Never panics or propagates errors —
    /// serialization failure is a bug, I/O failure is logged via tracing.
    pub fn write(&self, entry: &QueryLogEntry) {
        let line = match serde_json::to_string(entry) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "failed to serialize query log entry");
                return;
            }
        };
        let Ok(mut writer) = self.writer.lock() else {
            tracing::warn!("query log mutex poisoned, skipping entry");
            return;
        };
        if let Err(e) = writeln!(writer, "{line}") {
            tracing::warn!(error = %e, "failed to write query log entry");
            return;
        }
        if let Err(e) = writer.flush() {
            tracing::warn!(error = %e, "failed to flush query log");
        }
    }
}

impl std::fmt::Debug for QueryLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryLog").finish_non_exhaustive()
    }
}

/// A single query execution log entry.
#[derive(Debug, Serialize)]
pub struct QueryLogEntry {
    /// ISO 8601 timestamp of the log entry.
    pub ts: String,
    /// Authenticated user name.
    pub user: String,
    /// User's role.
    pub role: String,
    /// Raw DSL query string.
    pub dsl: String,
    /// Source file selection debug info.
    pub source: SourceDebug,
    /// Hot buffer state at query time.
    pub hot_buffer: HotBufferDebug,
    /// Generated SQL (parameterized).
    pub sql: String,
    /// SQL parameter values (Display form).
    pub params: Vec<String>,
    /// Query result summary.
    pub result: ResultDebug,
    /// Execution timing breakdown.
    pub timing_ms: TimingDebug,
    /// Error message if the query failed, null otherwise.
    pub error: Option<String>,
}

/// Debug info about parquet source file selection.
#[derive(Debug, Serialize)]
pub struct SourceDebug {
    /// The computed source argument passed to `read_parquet()`.
    pub computed: String,
    /// Number of glob patterns in the source list.
    pub globs: usize,
    /// Service name extracted from the DSL, if any.
    pub service_filter: Option<String>,
    /// Time filter duration in seconds, if any.
    pub time_filter_secs: Option<u64>,
    /// Whether the source fell back to recursive glob.
    pub is_fallback: bool,
}

/// Debug info about the hot buffer state at query time.
#[derive(Debug, Serialize)]
pub struct HotBufferDebug {
    /// "disabled", "empty", or "active".
    pub status: &'static str,
    /// Total events in the hot buffer.
    pub events: usize,
    /// Number of batches in the hot buffer.
    pub batches: usize,
    /// Estimated byte size of the hot buffer.
    pub bytes: usize,
}

/// Debug info about the query result.
#[derive(Debug, Serialize)]
pub struct ResultDebug {
    /// "success" or "error".
    pub status: &'static str,
    /// Column names in the result set.
    pub columns: Vec<String>,
    /// Total row count.
    pub row_count: usize,
    /// First N rows as self-describing maps.
    pub sample: Vec<BTreeMap<String, JsonValue>>,
}

/// Execution timing breakdown.
#[derive(Debug, Serialize)]
pub struct TimingDebug {
    /// Time spent waiting for a pool permit (ms).
    pub pool_wait: u64,
    /// Total query execution time (ms).
    pub total: u64,
}
