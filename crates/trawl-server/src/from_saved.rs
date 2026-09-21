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
use crate::store::{ReportRun, RunStatus, SavedQueryStore, ScheduleStore};

/// Result of resolving a `from saved` stage.
#[derive(Debug)]
pub(crate) struct ResolvedFromSaved {
    /// The `DuckDB` source expression (e.g. `read_parquet('...')` or a `UNION ALL` subquery).
    pub source: String,
    /// The DSL string with the `from saved` stage stripped, suitable for
    /// re-parse + execution against the resolved source.
    pub remaining_dsl: String,
    /// The saved query's own DSL — the text whose run produced the parquet
    /// this query reads.
    ///
    /// Carried for the incomplete-results notice (ADR-0011): the stored
    /// results were computed from fields the saved query bound, so a
    /// degraded pin among them is as much a completeness caveat as one in
    /// the stages the caller typed. Nothing stamps report runs at write
    /// time, so the walk happens here, over both texts.
    pub saved_dsl: String,
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
    let saved = saved_store
        .get_by_name(key_id, &stage.name)
        .await?
        .ok_or_else(|| ServerError::NotFound(format!("saved query '{}' not found", stage.name)))?;

    let source = match stage.run {
        SavedRunSelector::Latest => {
            resolve_latest(schedule_store, saved.id, key_id, data_dir).await?
        }
        SavedRunSelector::Specific(run_id) => {
            resolve_specific(schedule_store, run_id, key_id, data_dir).await?
        }
        SavedRunSelector::All => resolve_all(schedule_store, saved.id, data_dir).await?,
    };

    // Everything after the stage's span end is the remaining pipeline (e.g.
    // " | stats count() by host"). Prepend "*" to form a valid query with an
    // empty search stage so the remaining pipes are re-parsed.
    let remaining = &original_dsl[stage_span_end..];
    let remaining_dsl = if remaining.trim().is_empty() {
        "*".to_string()
    } else {
        format!("*{remaining}")
    };

    Ok(ResolvedFromSaved {
        source,
        remaining_dsl,
        saved_dsl: saved.query,
    })
}

/// The source ONE successful run reads from, whichever selector picked it.
///
/// `run=latest` and `run=N` differ in how they find the run, never in what
/// a run means, so both come here. A run id that happens to be the latest
/// has to answer the same as `run=latest`, and before this it did not: the
/// id path 404'd a run with no parquet file while `run=latest` resolved it.
///
/// Three shapes, all of them successful runs:
///
/// - a `result_path`: the ordinary run, read as parquet.
/// - no path, a blob with no rows: the run genuinely found nothing. Its
///   blob still carries the column names, so it resolves to the empty typed
///   source [`empty_typed_source`] builds, and `| stats count()` over it
///   answers 0.
/// - no path, a blob WITH rows: a run whose parquet write failed and fell
///   back to the blob. It exists and `GET /saved/{id}/runs/{run_id}` serves
///   it, but there is no file to point a query at, so this is a 409 naming
///   the run. For `run=latest` the next scheduled run clears it, which is
///   what makes 409 the right status rather than 404 or 500.
async fn source_for_run(
    run: &ReportRun,
    schedule_store: &ScheduleStore,
    key_id: i64,
    data_dir: &str,
) -> Result<String, ServerError> {
    if let Some(ref result_path) = run.result_path {
        return Ok(parquet_source(data_dir, result_path));
    }

    // `get_run_result` scopes the blob by the schedule's owning key, the
    // same ownership join `get_run` reads a run through.
    let blob = schedule_store.get_run_result(run.id, key_id).await?;
    let Some(result) = crate::scheduler::decode_result_blob(blob) else {
        // A success with neither a file nor a readable blob is corrupt
        // state, not an empty window. Say so about this run rather than
        // resolving something else. The detail is logged, not returned.
        return Err(ServerError::Internal(format!(
            "report run {} succeeded with no parquet result and no readable result blob",
            run.id
        )));
    };

    if result.rows.is_empty() {
        return Ok(empty_typed_source(&result.columns));
    }

    Err(ServerError::Conflict(format!(
        "report run {run} produced {rows} rows but no parquet result, so it cannot be read \
         through `from saved`; fetch it at /api/v1/saved/{saved}/runs/{run} instead, or wait \
         for the next scheduled run",
        run = run.id,
        rows = result.rows.len(),
        saved = run.saved_query_id,
    )))
}

/// Resolve `run=latest` to the most recent successful run.
///
/// The newest success is the only run this selector may answer from:
/// [`source_for_run`] either resolves THAT run or fails naming it, and
/// nothing here falls through to an older one. An older run covers an older
/// window, and answering from it without saying so is a stale report
/// (ADR-0018 ruling 13).
async fn resolve_latest(
    schedule_store: &ScheduleStore,
    saved_query_id: i64,
    key_id: i64,
    data_dir: &str,
) -> Result<String, ServerError> {
    let run = schedule_store
        .latest_successful_run(saved_query_id)
        .await?
        .ok_or_else(|| ServerError::NotFound("no successful runs".to_string()))?;

    source_for_run(&run, schedule_store, key_id, data_dir).await
}

/// Resolve `run=N` — a specific run by ID.
///
/// Two refusals of its own, then the shared resolution. A run id nobody owns
/// (or that does not exist) is a 404 that says nothing more, and a run that
/// did not SUCCEED has no result to query: `finish_run` writes the result
/// only on the success path, so a `running`, `error` or `timeout` row has
/// neither a file nor a blob. Before this the status went unchecked and the
/// NULL `result_path` did the refusing by accident, which now reads as
/// corrupt state instead. The message names the status, so a caller can tell
/// "not yet" from "never".
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

    if run.status != RunStatus::Success {
        return Err(ServerError::NotFound(format!(
            "report run {run_id} has no result to query (status: {})",
            run.status.as_str()
        )));
    }

    source_for_run(&run, schedule_store, key_id, data_dir).await
}

/// Resolve `run=all` — all successful runs, unioned with `_run_id` and `_run_time` metadata.
///
/// Zero-row runs are deliberately NOT members. They have no parquet file
/// (see [`empty_typed_source`]), a union branch built from their column
/// names would contribute no rows anyway, and its all-NULL columns could
/// only clash with the typed ones the real files bring. The visible
/// consequence: `_run_id` and `_run_time` never name a run that found
/// nothing, so `run=all` describes the runs that produced data, not every
/// run that happened. The single-run selectors, `run=latest` and `run=N`,
/// both answer for a zero-row run.
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

    let mut parts = Vec::with_capacity(runs.len());
    for run in &runs {
        let Some(ref result_path) = run.result_path else {
            // The store already filters `result_path IS NOT NULL`; this arm
            // only unwraps the Option.
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

/// Build a zero-row source that still carries a run's column NAMES.
///
/// A successful report run with no rows has no parquet file: the ndjson
/// writer behind `write_query_result_to_parquet` has nothing to infer a
/// schema from, so there is no typed empty parquet to point at. The run's
/// zstd JSON blob is the one representation that kept the column names, and
/// this turns them back into a FROM source: `(SELECT NULL AS "a", NULL AS
/// "b" WHERE FALSE)`. Downstream stages bind their fields, aggregates
/// answer over an empty relation, and no row is invented.
///
/// Every column is NULL-typed. That is enough for `| where`, `stats` and
/// `table` (probed in `empty_typed_source_executes_downstream_shapes`), and
/// it is the only honest type available: the blob records names, not the
/// `DuckDB` types the run's rows would have had.
///
/// A result with ZERO columns is `QueryResult::empty()`, the answer a run
/// gets when its own source matched no file at all. There are no names to
/// carry, so the source is a one-column dummy nothing binds to:
/// `(SELECT 1 WHERE FALSE)`. A downstream field reference then fails to
/// bind, which is the truth about that run: it recorded no schema.
fn empty_typed_source(columns: &[trawl_api::value::Column]) -> String {
    if columns.is_empty() {
        return "(SELECT 1 WHERE FALSE)".to_string();
    }
    let projected = columns
        .iter()
        .map(|c| format!("NULL AS \"{}\"", c.name.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ");
    format!("(SELECT {projected} WHERE FALSE)")
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

    pub(super) fn cols(names: &[&str]) -> Vec<trawl_api::value::Column> {
        names
            .iter()
            .map(|n| trawl_api::value::Column {
                name: (*n).to_string(),
            })
            .collect()
    }

    #[test]
    fn empty_typed_source_projects_quoted_names() {
        assert_eq!(
            empty_typed_source(&cols(&["a", "b"])),
            r#"(SELECT NULL AS "a", NULL AS "b" WHERE FALSE)"#
        );
        assert_eq!(
            empty_typed_source(&cols(&[r#"we"ird"#])),
            r#"(SELECT NULL AS "we""ird" WHERE FALSE)"#
        );
        assert_eq!(empty_typed_source(&[]), "(SELECT 1 WHERE FALSE)");
    }

    /// Execution probe: the NULL-typed empty source has to survive every
    /// downstream shape a `from saved` pipeline can put after it, through the
    /// real emitter and a real `DuckDB`. A NULL column is an untyped literal
    /// in `DuckDB`, so "does `count()` over it work" is a question only
    /// execution answers.
    #[test]
    fn empty_typed_source_executes_downstream_shapes() {
        let executor = trawl_engine::executor::Executor::new().unwrap();
        let pins = trawl_core::schema::FieldTypes::new();
        let source = empty_typed_source(&cols(&["a", "b", "_time"]));

        let run = |dsl: &str| {
            executor
                .run_query(dsl, &source, &pins, 100, 0)
                .unwrap_or_else(|e| panic!("{dsl} over {source} failed: {e}"))
        };

        // Filtering an untyped NULL column: binds, matches nothing.
        assert_eq!(run("* | where a > 5").rows.len(), 0);
        // A row-count aggregate still answers, with zero.
        let counted = run("* | stats count()");
        assert_eq!(counted.rows.len(), 1);
        assert_eq!(counted.rows[0][0].to_string(), "0");
        // An aggregate that reads the NULL column itself.
        assert_eq!(run("* | stats avg(a)").rows.len(), 1);
        // Projection binds the name and returns no rows.
        let projected = run("* | table a");
        assert_eq!(projected.rows.len(), 0);
        assert_eq!(projected.columns.len(), 1);
        assert_eq!(projected.columns[0].name, "a");
        // `_time` survives the TIMESTAMP cast the time-aware stages emit.
        assert_eq!(run("* | sort -_time | head 5").rows.len(), 0);
        executor
            .run_query("* | table _time", &source, &pins, 100, 0)
            .expect("_time column must bind");
        // The timestamp cast the union and display paths put on `_time`,
        // asked of DuckDB directly so the assertion does not depend on which
        // stage happens to emit it today.
        duckdb::Connection::open_in_memory()
            .unwrap()
            .execute_batch(&format!(
                r#"SELECT TRY_CAST("_time" AS TIMESTAMP) AS t FROM {source}"#
            ))
            .expect("TRY_CAST over a NULL-typed _time column must plan");
        // The zero-column fallback is a valid source too (nothing binds).
        assert_eq!(
            executor
                .run_query("* | stats count()", &empty_typed_source(&[]), &pins, 100, 0)
                .unwrap()
                .rows[0][0]
                .to_string(),
            "0"
        );
    }

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

/// `run=all` feeds a relation whose only timestamp is `_run_time`, so
/// `timechart on _run_time` is the stage that makes it chartable: real
/// runs, real parquet, real `DuckDB`, one bucket per hour of run time.
///
/// Module-level rather than inside `pg_tests` so its path is
/// `from_saved::timechart_on_run_time`.
#[cfg(test)]
#[sqlx::test]
async fn timechart_on_run_time(pool: sqlx::PgPool) {
    use crate::store::RunStatus;
    use pg_tests::{run, schedule_id, seed};

    /// A one-row parquet result carrying the `count` column a report's
    /// `stats count()` would have written.
    fn write_count_parquet(data_dir: &str, rel: &str, count: i64) {
        let full = format!("{data_dir}/{rel}");
        std::fs::create_dir_all(std::path::Path::new(&full).parent().unwrap()).unwrap();
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT {count}::BIGINT AS \"count\") TO '{full}' (FORMAT PARQUET)"
        ))
        .unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().to_str().unwrap();

    let (saved_id, sched_store, saved_store) = seed(&pool, 1, "timechart_runs").await;
    let sid = schedule_id(&sched_store, saved_id).await;

    write_count_parquet(data_dir, "scheduled/timechart_runs/run_1.parquet", 4);
    write_count_parquet(data_dir, "scheduled/timechart_runs/run_2.parquet", 6);
    let r1 = run(
        &sched_store,
        sid,
        saved_id,
        RunStatus::Success,
        Some("scheduled/timechart_runs/run_1.parquet"),
    )
    .await;
    let r2 = run(
        &sched_store,
        sid,
        saved_id,
        RunStatus::Success,
        Some("scheduled/timechart_runs/run_2.parquet"),
    )
    .await;

    // Both runs happen inside this test, so their real `started_at`
    // values land in the same hour — and, once an hour, in two. Plant
    // the instants instead, so the bucketing is asserted, not sampled.
    for (id, at) in [(r1, "2026-01-01T00:17:00Z"), (r2, "2026-01-01T02:41:00Z")] {
        sqlx::query("UPDATE report_runs SET started_at = $1 WHERE id = $2")
            .bind(at.parse::<chrono::DateTime<chrono::Utc>>().unwrap())
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    }

    let dsl = "| from saved timechart_runs run=all | timechart on _run_time span=1h sum(count)";
    let ast = trawl_core::parser::parse(dsl).expect("dsl parses");
    let stage = ast
        .from_saved_stage()
        .expect("the first stage is from saved");
    let resolved = resolve(
        stage,
        dsl,
        ast.pipeline[0].span.end,
        &saved_store,
        &sched_store,
        1,
        data_dir,
    )
    .await
    .expect("run=all resolves");

    let remaining = trawl_core::parser::parse(&resolved.remaining_dsl).expect("the tail parses");
    let emitted = trawl_core::emitter::emit(
        &remaining,
        &resolved.source,
        trawl_core::context::EvalContext::capture(),
    )
    .expect("emit succeeds");

    let conn = duckdb::Connection::open_in_memory().unwrap();
    let mut stmt = conn
        .prepare(&format!(
            "SELECT strftime(\"_time\", '%Y-%m-%d %H:%M:%S') AS b, \
             CAST(\"sum_count\" AS BIGINT) FROM ({}) ORDER BY b",
            emitted.sql
        ))
        .expect("the emitted SQL binds over the run union");
    let rows: Vec<(String, i64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("the emitted SQL runs")
        .collect::<Result<_, _>>()
        .expect("rows read");

    assert_eq!(
        rows,
        vec![
            ("2026-01-01 00:00:00".to_string(), 4),
            ("2026-01-01 02:00:00".to_string(), 6),
        ],
        "one bucket per hour of run time, carrying that run's count"
    );
}

/// Store-backed coverage for the run-selector resolution path.
///
/// These exercise the branch logic against a real `#[sqlx::test]` database
/// and, for `run=all`, run the emitted UNION subquery through `DuckDB` so
/// the `TIMESTAMP '<offset>'` synthetic-column literal is proven to parse
/// (guarding the offset-literal contract the pg `started_at` round-trip
/// depends on).
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
    pub(super) async fn seed(
        pool: &PgPool,
        key_id: i64,
        name: &str,
    ) -> (i64, ScheduleStore, SavedQueryStore) {
        let saved_store = SavedQueryStore::new(pool.clone());
        let sched_store = ScheduleStore::new(pool.clone());
        let saved = saved_store
            .create(key_id, name, "_severity=error")
            .await
            .unwrap();
        sched_store
            .create_schedule(saved.id, key_id, 300, None, None, 0, chrono::Utc::now())
            .await
            .unwrap();
        (saved.id, sched_store, saved_store)
    }

    /// Start then finish a run, returning its id. `path`/`status` let callers
    /// build success-with-parquet, success-without-parquet, and error rows.
    pub(super) async fn run(
        store: &ScheduleStore,
        schedule_id: i64,
        saved_id: i64,
        status: RunStatus,
        path: Option<&str>,
    ) -> i64 {
        let rid = match store
            .claim_run(schedule_id, saved_id, "q", None, None)
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

    /// Start then finish a run whose result is a zstd JSON blob instead of a
    /// parquet file, the way `write_result_parquet` persists a zero-row (or
    /// parquet-write-failed) result. Returns its id.
    async fn run_with_blob(
        store: &ScheduleStore,
        schedule_id: i64,
        saved_id: i64,
        result: &trawl_api::value::QueryResult,
    ) -> i64 {
        let rid = match store
            .claim_run(schedule_id, saved_id, "q", None, None)
            .await
            .unwrap()
        {
            RunClaim::Started(id) => id,
            other => panic!("expected a started run, got {other:?}"),
        };
        let json = serde_json::to_vec(result).unwrap();
        let blob = zstd::encode_all(json.as_slice(), 3).unwrap();
        store
            .finish_run(
                rid,
                RunStatus::Success,
                10,
                Some(result.rows.len()),
                None,
                Some(&blob),
                None,
            )
            .await
            .unwrap();
        rid
    }

    /// A `QueryResult` with the given column names and no rows: what a run
    /// whose window held nothing records.
    fn zero_row_result(names: &[&str]) -> trawl_api::value::QueryResult {
        trawl_api::value::QueryResult {
            columns: super::tests::cols(names),
            rows: Vec::new(),
        }
    }

    /// Schedule id for a saved query (seed always creates exactly one).
    pub(super) async fn schedule_id(store: &ScheduleStore, saved_id: i64) -> i64 {
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

        let source = resolve_latest(&sched_store, saved_id, 1, "/data")
            .await
            .unwrap();
        // Newest wins (started_at DESC, id DESC).
        assert_eq!(source, parquet_source("/data", "p/run_2.parquet"));
    }

    #[sqlx::test]
    async fn resolve_latest_not_found_without_successful_runs(pool: PgPool) {
        let (saved_id, sched_store, _saved) = seed(&pool, 1, "latest_none").await;
        let sid = schedule_id(&sched_store, saved_id).await;
        // A failed run is not a run this selector can answer from.
        run(&sched_store, sid, saved_id, RunStatus::Error, None).await;

        let err = resolve_latest(&sched_store, saved_id, 1, "/data")
            .await
            .expect_err("no successful run must be NotFound");
        assert!(matches!(err, ServerError::NotFound(_)), "got: {err:?}");
    }

    /// A success carrying neither a parquet path nor a result blob is
    /// corrupt state. The selector must say so about that run, not quietly
    /// resolve the older run behind it.
    #[sqlx::test]
    async fn resolve_latest_errors_on_a_success_with_neither_file_nor_blob(pool: PgPool) {
        let (saved_id, sched_store, _saved) = seed(&pool, 1, "latest_corrupt").await;
        let sid = schedule_id(&sched_store, saved_id).await;
        run(
            &sched_store,
            sid,
            saved_id,
            RunStatus::Success,
            Some("p/run_1.parquet"),
        )
        .await;
        let orphan = run(&sched_store, sid, saved_id, RunStatus::Success, None).await;

        let err = resolve_latest(&sched_store, saved_id, 1, "/data")
            .await
            .expect_err("a success with no result at all must not resolve");
        match err {
            ServerError::Internal(msg) => assert!(
                msg.contains(&orphan.to_string()),
                "the message must name the run: {msg}"
            ),
            other => panic!("expected Internal, got: {other:?}"),
        }
    }

    /// The point of ADR-0018 ruling 13: a run that found nothing is still the
    /// newest run, and `run=latest` has to answer from IT. Resolving the
    /// older run behind it would report yesterday's rows as today's.
    #[sqlx::test]
    async fn resolve_latest_answers_a_zero_row_success_not_an_older_run(pool: PgPool) {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();

        let (saved_id, sched_store, _saved) = seed(&pool, 1, "latest_zero").await;
        let sid = schedule_id(&sched_store, saved_id).await;

        write_parquet(data_dir, "scheduled/latest_zero/run_1.parquet", 1);
        run(
            &sched_store,
            sid,
            saved_id,
            RunStatus::Success,
            Some("scheduled/latest_zero/run_1.parquet"),
        )
        .await;
        run_with_blob(&sched_store, sid, saved_id, &zero_row_result(&["n", "msg"])).await;

        let source = resolve_latest(&sched_store, saved_id, 1, data_dir)
            .await
            .expect("a zero-row success resolves to its own empty source");
        assert_eq!(source, r#"(SELECT NULL AS "n", NULL AS "msg" WHERE FALSE)"#);

        // Executed, because the assertion that matters is the ANSWER: the
        // older run holds one row, and reading it here would count 1.
        let executor = trawl_engine::executor::Executor::new().unwrap();
        let pins = trawl_core::schema::FieldTypes::new();
        let counted = executor
            .run_query("* | stats count()", &source, &pins, 100, 0)
            .unwrap();
        assert_eq!(counted.rows[0][0].to_string(), "0");
        assert_eq!(
            executor
                .run_query("* | table n", &source, &pins, 100, 0)
                .unwrap()
                .rows
                .len(),
            0
        );
    }

    /// A run whose parquet write failed kept its rows in the blob. It exists
    /// and the run endpoint serves it, but there is no file to query, so the
    /// selector refuses NAMING that run instead of silently reading the
    /// older one behind it.
    #[sqlx::test]
    async fn resolve_latest_over_a_blob_backed_nonempty_run_errors(pool: PgPool) {
        let (saved_id, sched_store, _saved) = seed(&pool, 1, "latest_blob").await;
        let sid = schedule_id(&sched_store, saved_id).await;
        run(
            &sched_store,
            sid,
            saved_id,
            RunStatus::Success,
            Some("scheduled/latest_blob/run_1.parquet"),
        )
        .await;
        let blob_run = run_with_blob(
            &sched_store,
            sid,
            saved_id,
            &trawl_api::value::QueryResult {
                columns: super::tests::cols(&["n"]),
                rows: vec![vec![trawl_api::value::Value::Integer(7)]],
            },
        )
        .await;

        let err = resolve_latest(&sched_store, saved_id, 1, "/data")
            .await
            .expect_err("a blob-backed run with rows is not queryable here");
        match err {
            ServerError::Conflict(msg) => {
                assert!(
                    msg.contains(&blob_run.to_string()),
                    "the refusal must name the run: {msg}"
                );
                assert!(
                    !msg.contains("run_1.parquet"),
                    "and must not point at the older run: {msg}"
                );
            }
            other => panic!("expected Conflict, got: {other:?}"),
        }
    }

    /// `run=all` unions files. A zero-row run has none, so it contributes
    /// nothing and `_run_id` never names it (see `resolve_all`).
    #[sqlx::test]
    async fn resolve_all_skips_zero_row_members(pool: PgPool) {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();

        let (saved_id, sched_store, _saved) = seed(&pool, 1, "all_zero").await;
        let sid = schedule_id(&sched_store, saved_id).await;

        write_parquet(data_dir, "scheduled/all_zero/run_1.parquet", 1);
        let with_rows = run(
            &sched_store,
            sid,
            saved_id,
            RunStatus::Success,
            Some("scheduled/all_zero/run_1.parquet"),
        )
        .await;
        let empty = run_with_blob(&sched_store, sid, saved_id, &zero_row_result(&["n"])).await;

        let source = resolve_all(&sched_store, saved_id, data_dir).await.unwrap();
        assert!(
            !source.contains(&format!("{empty} AS _run_id")),
            "the zero-row run is not a member: {source}"
        );

        let executor = trawl_engine::executor::Executor::new().unwrap();
        let rows = executor
            .run_query(
                "* | table _run_id",
                &source,
                &trawl_core::schema::FieldTypes::new(),
                100,
                0,
            )
            .unwrap()
            .rows;
        assert_eq!(rows.len(), 1, "one member, one row");
        assert_eq!(rows[0][0].to_string(), with_rows.to_string());
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

    /// A success with neither a file nor a blob is corrupt state, and `run=N`
    /// says so in the same words `run=latest` does.
    #[sqlx::test]
    async fn resolve_specific_errors_on_a_success_with_neither_file_nor_blob(pool: PgPool) {
        let (saved_id, sched_store, _saved) = seed(&pool, 1, "specific_noresult").await;
        let sid = schedule_id(&sched_store, saved_id).await;
        let rid = run(&sched_store, sid, saved_id, RunStatus::Success, None).await;

        let err = resolve_specific(&sched_store, rid, 1, "/data")
            .await
            .expect_err("a success with no result at all must not resolve");
        match err {
            ServerError::Internal(msg) => assert!(
                msg.contains(&rid.to_string()),
                "the message must name the run: {msg}"
            ),
            other => panic!("expected Internal, got: {other:?}"),
        }
    }

    /// The point of the shared [`source_for_run`]: a run id answers exactly
    /// as it would if it happened to be the latest. A zero-row run resolves
    /// to its own empty source instead of 404ing for want of a file.
    #[sqlx::test]
    async fn resolve_specific_answers_a_zero_row_run(pool: PgPool) {
        let (saved_id, sched_store, _saved) = seed(&pool, 1, "specific_zero").await;
        let sid = schedule_id(&sched_store, saved_id).await;
        let rid = run_with_blob(&sched_store, sid, saved_id, &zero_row_result(&["n", "msg"])).await;

        let source = resolve_specific(&sched_store, rid, 1, "/data")
            .await
            .expect("a zero-row run resolves by id");
        assert_eq!(source, r#"(SELECT NULL AS "n", NULL AS "msg" WHERE FALSE)"#);

        let counted = trawl_engine::executor::Executor::new()
            .unwrap()
            .run_query(
                "* | stats count()",
                &source,
                &trawl_core::schema::FieldTypes::new(),
                100,
                0,
            )
            .unwrap();
        assert_eq!(counted.rows[0][0].to_string(), "0");
    }

    /// And a run whose rows live in the blob is the same 409 by id as it is
    /// by `run=latest`: it exists, the run endpoint serves it, but there is
    /// no file for a query to read.
    #[sqlx::test]
    async fn resolve_specific_refuses_a_blob_backed_nonempty_run(pool: PgPool) {
        let (saved_id, sched_store, _saved) = seed(&pool, 1, "specific_blob").await;
        let sid = schedule_id(&sched_store, saved_id).await;
        let rid = run_with_blob(
            &sched_store,
            sid,
            saved_id,
            &trawl_api::value::QueryResult {
                columns: super::tests::cols(&["n"]),
                rows: vec![vec![trawl_api::value::Value::Integer(7)]],
            },
        )
        .await;

        let err = resolve_specific(&sched_store, rid, 1, "/data")
            .await
            .expect_err("a blob-backed run with rows is not queryable here");
        match err {
            ServerError::Conflict(msg) => assert!(
                msg.contains(&rid.to_string()),
                "the refusal must name the run: {msg}"
            ),
            other => panic!("expected Conflict, got: {other:?}"),
        }
    }

    /// A run that did not succeed has no result to serve. The status is the
    /// check now, not an incidentally NULL `result_path`, and the message
    /// carries it so a caller can tell "not yet" from "never".
    #[sqlx::test]
    async fn resolve_specific_refuses_a_run_that_did_not_succeed(pool: PgPool) {
        let (saved_id, sched_store, _saved) = seed(&pool, 1, "specific_status").await;
        let sid = schedule_id(&sched_store, saved_id).await;

        // In flight: claimed, never finished.
        let running = match sched_store
            .claim_run(sid, saved_id, "q", None, None)
            .await
            .unwrap()
        {
            RunClaim::Started(id) => id,
            other => panic!("expected a started run, got {other:?}"),
        };
        let err = resolve_specific(&sched_store, running, 1, "/data")
            .await
            .expect_err("a running run has no result");
        match err {
            ServerError::NotFound(msg) => assert!(msg.contains("running"), "{msg}"),
            other => panic!("expected NotFound, got: {other:?}"),
        }
        sched_store
            .finish_run(
                running,
                RunStatus::Error,
                10,
                None,
                Some("boom"),
                None,
                None,
            )
            .await
            .unwrap();

        let err = resolve_specific(&sched_store, running, 1, "/data")
            .await
            .expect_err("a failed run has no result either");
        match err {
            ServerError::NotFound(msg) => assert!(msg.contains("error"), "{msg}"),
            other => panic!("expected NotFound, got: {other:?}"),
        }
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
    async fn readable_name_resolves_recorded_path_after_rename(pool: PgPool) {
        let (saved_id, schedules, saved) = seed(&pool, 1, "legacy").await;
        let sid = schedule_id(&schedules, saved_id).await;
        run(
            &schedules,
            sid,
            saved_id,
            RunStatus::Success,
            Some("scheduled/legacy/run_1.parquet"),
        )
        .await;
        let name = "雪 / \"report\" \\ path";
        let query = saved.get(saved_id, 1).await.unwrap().unwrap().query;
        saved
            .update_checked(saved_id, 1, &query, Some(name))
            .await
            .unwrap();
        let dsl = format!(
            "| from saved {} | head 1",
            trawl_core::format::format_saved_name(name)
        );
        let ast = trawl_core::parser::parse(&dsl).unwrap();
        let resolved = resolve(
            ast.from_saved_stage().unwrap(),
            &dsl,
            ast.pipeline[0].span.end,
            &saved,
            &schedules,
            1,
            "/data",
        )
        .await
        .unwrap();
        assert_eq!(
            resolved.source,
            parquet_source("/data", "scheduled/legacy/run_1.parquet")
        );
        assert_eq!(resolved.remaining_dsl, "* | head 1");
        assert!(
            resolve(
                ast.from_saved_stage().unwrap(),
                &dsl,
                ast.pipeline[0].span.end,
                &saved,
                &schedules,
                2,
                "/data"
            )
            .await
            .is_err()
        );
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
            ..
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

    /// The saved query's own text rides out of the resolve, because the
    /// incomplete-results notice is stamped over both halves of what the
    /// caller reads: the stages they typed, and the query whose recorded run
    /// produced the rows underneath them. Nothing stamps a report run at
    /// write time, so this is the only place those fields are still known.
    #[sqlx::test]
    async fn resolve_carries_the_saved_querys_own_dsl(pool: PgPool) {
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

        let dsl = "| from saved rollup | stats count() by host";
        let span_end = dsl.find(" | stats").unwrap();
        let resolved = resolve(
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
        assert_eq!(
            resolved.saved_dsl, "_severity=error",
            "the saved query's own fields are walkable by the notice"
        );
    }

    fn stage_latest(name: &str) -> FromSavedStage {
        FromSavedStage {
            name: name.to_string(),
            run: SavedRunSelector::Latest,
        }
    }
}
