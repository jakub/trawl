// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Resolve `| from saved <name>` pipe stages to parquet sources.
//!
//! Called from the query handler before pool execution. Looks up the
//! named saved query in the app-state store, resolves run selectors
//! (`latest`, `all`, or a specific run ID) to parquet file paths,
//! and rewrites the DSL string with the `from saved` stage stripped.

use trawl_core::ast::{FromSavedStage, SavedRunSelector};

use crate::error::ServerError;
use crate::store::{SavedQueryStore, ScheduleStore};

/// Result of resolving a `from saved` stage.
pub(crate) struct ResolvedFromSaved {
    /// The `DuckDB` source expression (e.g. `read_parquet('...')` or a `UNION ALL` subquery).
    pub source: String,
    /// The DSL string with the `from saved` stage stripped, suitable for
    /// re-parse + execution against the resolved source.
    pub remaining_dsl: String,
}

/// Resolve a `FromSavedStage` to a parquet source and remaining DSL.
///
/// `stage_span_end` is the byte offset in `original_dsl` where the
/// `from saved` stage ends (from the `Spanned` wrapper). Everything
/// after that offset becomes the remaining pipeline.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve(
    stage: &FromSavedStage,
    original_dsl: &str,
    stage_span_end: usize,
    saved_store: &SavedQueryStore,
    schedule_store: &ScheduleStore,
    key_id: i64,
    data_dir: &str,
) -> Result<ResolvedFromSaved, ServerError> {
    // Look up the saved query by name (user-scoped).
    let saved = saved_store
        .get_by_name(key_id, &stage.name)
        .await?
        .ok_or_else(|| ServerError::NotFound(format!("saved query '{}' not found", stage.name)))?;

    // Resolve the run selector to a DuckDB source expression.
    let source = match stage.run {
        SavedRunSelector::Latest => resolve_latest(schedule_store, saved.id, data_dir).await?,
        SavedRunSelector::Specific(run_id) => {
            resolve_specific(schedule_store, run_id, key_id, data_dir).await?
        }
        SavedRunSelector::All => resolve_all(schedule_store, saved.id, data_dir).await?,
    };

    // Strip the `from saved` stage from the original DSL. Everything
    // after the stage's span end is the remaining pipeline (e.g.
    // " | stats count() by host"). Prepend "* " to form a valid query
    // with an empty search stage so the remaining pipes are re-parsed.
    let remaining = &original_dsl[stage_span_end..];
    let remaining_dsl = if remaining.trim().is_empty() {
        "*".to_string()
    } else {
        format!("*{remaining}")
    };

    Ok(ResolvedFromSaved {
        source,
        remaining_dsl,
    })
}

/// Resolve `run=latest` — most recent successful run with a parquet result.
async fn resolve_latest(
    schedule_store: &ScheduleStore,
    saved_query_id: i64,
    data_dir: &str,
) -> Result<String, ServerError> {
    let run = schedule_store
        .latest_successful_run(saved_query_id)
        .await?
        .ok_or_else(|| {
            ServerError::NotFound("no successful runs with parquet results".to_string())
        })?;

    let result_path = run.result_path.ok_or_else(|| {
        ServerError::Internal("latest successful run has no result_path".to_string())
    })?;

    Ok(parquet_source(data_dir, &result_path))
}

/// Resolve `run=N` — a specific run by ID.
async fn resolve_specific(
    schedule_store: &ScheduleStore,
    run_id: i64,
    key_id: i64,
    data_dir: &str,
) -> Result<String, ServerError> {
    let run = schedule_store
        .get_run(run_id, key_id)
        .await?
        .ok_or_else(|| ServerError::NotFound(format!("report run {run_id} not found")))?;

    let result_path = run.result_path.ok_or_else(|| {
        ServerError::NotFound(format!(
            "report run {run_id} has no parquet result (may be a legacy zstd-only run)"
        ))
    })?;

    Ok(parquet_source(data_dir, &result_path))
}

/// Resolve `run=all` — all successful runs, unioned with `_run_id` and `_run_time` metadata.
async fn resolve_all(
    schedule_store: &ScheduleStore,
    saved_query_id: i64,
    data_dir: &str,
) -> Result<String, ServerError> {
    let runs = schedule_store.list_successful_runs(saved_query_id).await?;

    if runs.is_empty() {
        return Err(ServerError::NotFound(
            "no successful runs with parquet results".to_string(),
        ));
    }

    // Build a UNION ALL BY NAME subquery that injects _run_id and _run_time.
    let mut parts = Vec::with_capacity(runs.len());
    for run in &runs {
        let Some(ref result_path) = run.result_path else {
            // Skip legacy runs without parquet files.
            continue;
        };
        let source = parquet_source(data_dir, result_path);
        let safe_time = run.started_at.to_rfc3339().replace('\'', "''");
        parts.push(format!(
            "SELECT *, {run_id} AS _run_id, TIMESTAMP '{safe_time}' AS _run_time FROM {source}",
            run_id = run.id,
        ));
    }

    if parts.is_empty() {
        return Err(ServerError::NotFound(
            "no successful runs with parquet results".to_string(),
        ));
    }

    // Wrap in parentheses so it can be used as a DuckDB subquery source.
    Ok(format!("({})", parts.join(" UNION ALL BY NAME ")))
}

/// Build a `read_parquet()` expression for a single result file.
///
/// `result_path` is relative to `data_dir` (e.g. `scheduled/my_query/run_42.parquet`).
fn parquet_source(data_dir: &str, result_path: &str) -> String {
    let base = data_dir.trim_end_matches('/');
    let safe_path = format!("{base}/{result_path}").replace('\'', "''");
    format!("read_parquet('{safe_path}', union_by_name=true)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parquet_source_basic() {
        let src = parquet_source("/var/lib/trawl/data", "scheduled/daily/run_1.parquet");
        assert_eq!(
            src,
            "read_parquet('/var/lib/trawl/data/scheduled/daily/run_1.parquet', union_by_name=true)"
        );
    }

    #[test]
    fn parquet_source_strips_trailing_slash() {
        let src = parquet_source("/data/", "scheduled/run.parquet");
        assert_eq!(
            src,
            "read_parquet('/data/scheduled/run.parquet', union_by_name=true)"
        );
    }

    #[test]
    fn parquet_source_escapes_single_quotes() {
        let src = parquet_source("/data", "scheduled/it's/run.parquet");
        assert_eq!(
            src,
            "read_parquet('/data/scheduled/it''s/run.parquet', union_by_name=true)"
        );
    }
}
