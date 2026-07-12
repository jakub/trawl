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
#[derive(Debug)]
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

/// Store-backed coverage for the run-selector resolution path (ADR-0004
/// slice 3 ported it to the async pg stores). These exercise the branch
/// logic against a real `#[sqlx::test]` database and, for `run=all`, run the
/// emitted UNION subquery through `DuckDB` so the `TIMESTAMP '<offset>'`
/// synthetic-column literal is proven to parse (guarding the offset-literal
/// contract the pg `started_at` round-trip depends on).
#[cfg(test)]
mod pg_tests {
    use sqlx::PgPool;
    use trawl_core::ast::{FromSavedStage, SavedRunSelector};

    use super::{
        ResolvedFromSaved, parquet_source, resolve, resolve_all, resolve_latest, resolve_specific,
    };
    use crate::error::ServerError;
    use crate::store::{RunClaim, RunStatus, SavedQueryStore, ScheduleStore};

    /// Create a saved query and its schedule, returning both ids.
    async fn seed(pool: &PgPool, key_id: i64, name: &str) -> (i64, ScheduleStore, SavedQueryStore) {
        let saved_store = SavedQueryStore::new(pool.clone());
        let sched_store = ScheduleStore::new(pool.clone());
        let saved = saved_store
            .create(key_id, name, "level=error")
            .await
            .unwrap();
        sched_store
            .create_schedule(saved.id, key_id, 300, None)
            .await
            .unwrap();
        (saved.id, sched_store, saved_store)
    }

    /// Start then finish a run, returning its id. `path`/`status` let callers
    /// build success-with-parquet, success-without-parquet, and error rows.
    async fn run(
        store: &ScheduleStore,
        schedule_id: i64,
        saved_id: i64,
        status: RunStatus,
        path: Option<&str>,
    ) -> i64 {
        let rid = match store
            .claim_run(schedule_id, saved_id, "q", None)
            .await
            .unwrap()
        {
            RunClaim::Started(id) => id,
            other => panic!("expected a started run, got {other:?}"),
        };
        store
            .finish_run(rid, status, 10, Some(1), None, None, path)
            .await
            .unwrap();
        rid
    }

    /// Schedule id for a saved query (seed always creates exactly one).
    async fn schedule_id(store: &ScheduleStore, saved_id: i64) -> i64 {
        store
            .get_schedule_for_saved_query(saved_id, 1)
            .await
            .unwrap()
            .unwrap()
            .id
    }

    /// Write a tiny single-row parquet file at `data_dir/rel` via `DuckDB`.
    fn write_parquet(data_dir: &str, rel: &str, tag: i64) {
        let full = format!("{data_dir}/{rel}");
        std::fs::create_dir_all(std::path::Path::new(&full).parent().unwrap()).unwrap();
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT {tag}::BIGINT AS n, 'msg-{tag}' AS msg) \
             TO '{full}' (FORMAT PARQUET)"
        ))
        .unwrap();
    }

    // --- resolve_latest -----------------------------------------------------

    #[sqlx::test]
    async fn resolve_latest_picks_newest_successful_run(pool: PgPool) {
        let (saved_id, sched_store, _saved) = seed(&pool, 1, "latest").await;
        let sid = schedule_id(&sched_store, saved_id).await;
        run(
            &sched_store,
            sid,
            saved_id,
            RunStatus::Success,
            Some("p/run_1.parquet"),
        )
        .await;
        run(
            &sched_store,
            sid,
            saved_id,
            RunStatus::Success,
            Some("p/run_2.parquet"),
        )
        .await;

        let source = resolve_latest(&sched_store, saved_id, "/data")
            .await
            .unwrap();
        // Newest wins (started_at DESC, id DESC).
        assert_eq!(source, parquet_source("/data", "p/run_2.parquet"));
    }

    #[sqlx::test]
    async fn resolve_latest_not_found_without_successful_runs(pool: PgPool) {
        let (saved_id, sched_store, _saved) = seed(&pool, 1, "latest_none").await;
        let sid = schedule_id(&sched_store, saved_id).await;
        // An error run and a success-without-parquet run are both ineligible.
        run(&sched_store, sid, saved_id, RunStatus::Error, None).await;
        run(&sched_store, sid, saved_id, RunStatus::Success, None).await;

        let err = resolve_latest(&sched_store, saved_id, "/data")
            .await
            .expect_err("no successful parquet run must be NotFound");
        assert!(matches!(err, ServerError::NotFound(_)), "got: {err:?}");
    }

    // --- resolve_specific ---------------------------------------------------

    #[sqlx::test]
    async fn resolve_specific_returns_source_for_run(pool: PgPool) {
        let (saved_id, sched_store, _saved) = seed(&pool, 1, "specific").await;
        let sid = schedule_id(&sched_store, saved_id).await;
        let rid = run(
            &sched_store,
            sid,
            saved_id,
            RunStatus::Success,
            Some("p/run_7.parquet"),
        )
        .await;

        let source = resolve_specific(&sched_store, rid, 1, "/data")
            .await
            .unwrap();
        assert_eq!(source, parquet_source("/data", "p/run_7.parquet"));
    }

    #[sqlx::test]
    async fn resolve_specific_not_found_for_missing_run(pool: PgPool) {
        let (saved_id, sched_store, _saved) = seed(&pool, 1, "specific_missing").await;
        let _ = saved_id;
        let err = resolve_specific(&sched_store, 999_999, 1, "/data")
            .await
            .expect_err("unknown run id must be NotFound");
        assert!(matches!(err, ServerError::NotFound(_)), "got: {err:?}");
    }

    #[sqlx::test]
    async fn resolve_specific_not_found_for_other_users_run(pool: PgPool) {
        let (saved_id, sched_store, _saved) = seed(&pool, 1, "specific_owned").await;
        let sid = schedule_id(&sched_store, saved_id).await;
        let rid = run(
            &sched_store,
            sid,
            saved_id,
            RunStatus::Success,
            Some("p/run_1.parquet"),
        )
        .await;

        // key_id 2 does not own the run — ownership join yields NotFound.
        let err = resolve_specific(&sched_store, rid, 2, "/data")
            .await
            .expect_err("cross-user run access must be NotFound");
        assert!(matches!(err, ServerError::NotFound(_)), "got: {err:?}");
    }

    #[sqlx::test]
    async fn resolve_specific_not_found_when_run_has_no_parquet(pool: PgPool) {
        let (saved_id, sched_store, _saved) = seed(&pool, 1, "specific_noparquet").await;
        let sid = schedule_id(&sched_store, saved_id).await;
        // A legacy blob-only success run: found by id, but no result_path.
        let rid = run(&sched_store, sid, saved_id, RunStatus::Success, None).await;

        let err = resolve_specific(&sched_store, rid, 1, "/data")
            .await
            .expect_err("run without result_path must be NotFound");
        assert!(matches!(err, ServerError::NotFound(_)), "got: {err:?}");
    }

    // --- resolve_all --------------------------------------------------------

    #[sqlx::test]
    async fn resolve_all_not_found_without_successful_runs(pool: PgPool) {
        let (saved_id, sched_store, _saved) = seed(&pool, 1, "all_none").await;
        let sid = schedule_id(&sched_store, saved_id).await;
        run(&sched_store, sid, saved_id, RunStatus::Error, None).await;

        let err = resolve_all(&sched_store, saved_id, "/data")
            .await
            .expect_err("no successful parquet runs must be NotFound");
        assert!(matches!(err, ServerError::NotFound(_)), "got: {err:?}");
    }

    /// The `run=all` UNION subquery must be SQL `DuckDB` executes end-to-end:
    /// every eligible run contributes a branch, the injected `_run_id` /
    /// `_run_time` columns materialise, and the `TIMESTAMP '<offset>'` literal
    /// built from the pg `started_at` parses (`DuckDB` accepts and ignores the
    /// UTC offset). Ineligible runs (error, or success without parquet) are
    /// excluded.
    #[sqlx::test]
    async fn resolve_all_builds_union_duckdb_executes(pool: PgPool) {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();

        let (saved_id, sched_store, _saved) = seed(&pool, 1, "all_exec").await;
        let sid = schedule_id(&sched_store, saved_id).await;

        write_parquet(data_dir, "scheduled/all_exec/run_1.parquet", 1);
        write_parquet(data_dir, "scheduled/all_exec/run_2.parquet", 2);
        let r1 = run(
            &sched_store,
            sid,
            saved_id,
            RunStatus::Success,
            Some("scheduled/all_exec/run_1.parquet"),
        )
        .await;
        let r2 = run(
            &sched_store,
            sid,
            saved_id,
            RunStatus::Success,
            Some("scheduled/all_exec/run_2.parquet"),
        )
        .await;
        // Ineligible rows that must not appear in the union.
        run(&sched_store, sid, saved_id, RunStatus::Error, None).await;
        run(&sched_store, sid, saved_id, RunStatus::Success, None).await;

        let source = resolve_all(&sched_store, saved_id, data_dir).await.unwrap();
        assert!(source.contains("UNION ALL BY NAME"), "source: {source}");
        assert!(source.contains("TIMESTAMP '"), "source: {source}");

        // Execute the subquery exactly as the emitter would (`FROM (subquery)`).
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT _run_id, CAST(_run_time AS VARCHAR) AS t, n FROM {source} ORDER BY _run_id"
            ))
            .unwrap();
        let rows: Vec<(i64, String, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(rows.len(), 2, "only the two parquet successes union in");
        assert_eq!(rows[0].0, r1);
        assert_eq!(rows[1].0, r2);
        assert_eq!(rows[0].2, 1, "run_1 parquet payload");
        assert_eq!(rows[1].2, 2, "run_2 parquet payload");
        for (_, ts, _) in &rows {
            assert!(!ts.is_empty(), "_run_time literal must materialise");
        }
    }

    // --- resolve (entry point) ---------------------------------------------

    #[sqlx::test]
    async fn resolve_unknown_saved_query_is_not_found(pool: PgPool) {
        let saved_store = SavedQueryStore::new(pool.clone());
        let sched_store = ScheduleStore::new(pool.clone());
        let stage = FromSavedStage {
            name: "ghost".to_string(),
            run: SavedRunSelector::Latest,
        };
        let dsl = "| from saved ghost";
        let err = resolve(
            &stage,
            dsl,
            dsl.len(),
            &saved_store,
            &sched_store,
            1,
            "/data",
        )
        .await
        .expect_err("missing saved query must be NotFound");
        assert!(matches!(err, ServerError::NotFound(_)), "got: {err:?}");
    }

    #[sqlx::test]
    async fn resolve_strips_stage_and_prepends_wildcard(pool: PgPool) {
        let (saved_id, sched_store, saved_store) = seed(&pool, 1, "rollup").await;
        let sid = schedule_id(&sched_store, saved_id).await;
        run(
            &sched_store,
            sid,
            saved_id,
            RunStatus::Success,
            Some("p/run_1.parquet"),
        )
        .await;

        // Trailing pipeline after the stage becomes `* <remaining>`.
        let dsl = "| from saved rollup | stats count() by host";
        let span_end = dsl.find(" | stats").unwrap();
        let ResolvedFromSaved {
            source,
            remaining_dsl,
        } = resolve(
            &stage_latest("rollup"),
            dsl,
            span_end,
            &saved_store,
            &sched_store,
            1,
            "/data",
        )
        .await
        .unwrap();
        assert_eq!(source, parquet_source("/data", "p/run_1.parquet"));
        assert_eq!(remaining_dsl, "* | stats count() by host");

        // No trailing pipeline collapses to a bare `*`.
        let bare = "| from saved rollup";
        let ResolvedFromSaved { remaining_dsl, .. } = resolve(
            &stage_latest("rollup"),
            bare,
            bare.len(),
            &saved_store,
            &sched_store,
            1,
            "/data",
        )
        .await
        .unwrap();
        assert_eq!(remaining_dsl, "*");
    }

    fn stage_latest(name: &str) -> FromSavedStage {
        FromSavedStage {
            name: name.to_string(),
            run: SavedRunSelector::Latest,
        }
    }
}
