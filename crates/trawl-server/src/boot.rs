// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The boot steps that make the corpus ready to serve (ADR-0041 slice 2),
//! and the captured results with it (ADR-0057).
//!
//! They run after publication recovery and state construction, and before
//! self-telemetry, compaction, the other workers, the scheduler and the
//! listener start.

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use crate::catalog::conform::{self, ArchiveIdentity};
use crate::config::Config;
use crate::error::join_failure_text;
use crate::state::AppState;

/// Prepare the corpus and the captured results for serving.
///
/// Self-telemetry must not be active yet. A WAL file it wrote before step 3
/// would be inserted once and hydrated a second time.
///
/// # Errors
///
/// Each stops the boot: a failed reconciliation of the results root, a
/// failed interruption of the steps left running, a failed marking of the
/// report runs left running, a failed conformance pass (a per-path failure
/// is skipped inside the pass, not an error), an archive that names another
/// catalog, a hydration error, or a gate that was not born starting. The
/// last two are boot bugs, never properties of the WAL.
pub async fn prepare_corpus(state: &AppState, config: &Config) -> Result<(), String> {
    crate::results::sweep::reconcile_at_boot(&state.results).await?;
    interrupt_running_steps(state).await?;
    mark_stale_report_runs(state).await?;
    if !config.ingest.enabled {
        return prepare_query_only(state, config).await;
    }
    let hot = state
        .query
        .hot_buffer
        .clone()
        .ok_or("ingest is enabled but the app state has no hot buffer")?;
    let gate = hot.publication();

    {
        let gate = Arc::clone(&gate);
        tokio::task::spawn_blocking(move || {
            crate::ingest::compaction::recover_rollups_at_boot(&gate);
        })
        .await
        .map_err(|e| join_failure_text("boot rollup recovery", e))?;
    }

    // Fatal on failure, like the epoch gate. A bad or claimed path is not a
    // failure: the pass skips and counts it, so one bad file cannot keep the
    // daemon down.
    let summary = conform::ensure_conformance(
        &state.storage.catalog,
        &state.query.field_catalog,
        &config.data.base_dir(),
        &config.wal_dir(),
        gate.removal_sync_owed_days(),
        &config.ingest.compaction_memory_limit,
    )
    .await?;
    tracing::info!(
        event_type = "catalog_conform",
        ran = summary.ran,
        scanned = summary.scanned,
        rewritten = summary.rewritten,
        skipped = summary.skipped,
        observed = summary.observed,
        "boot conformance pass finished"
    );

    let wal_dir = config.wal_dir();
    let report = tokio::task::spawn_blocking(move || {
        crate::ingest::hydration::hydrate_at_boot(&wal_dir, &hot, SystemTime::now())
    })
    .await
    .map_err(|e| join_failure_text("boot hydration", e))?
    .map_err(|e| format!("boot hydration failed: {e}"))?;
    gate.finish_hydration(report.overhang)
        .map_err(|e| format!("boot hydration: {e}"))
}

/// Record `interrupted` on every step a process that ended left running,
/// at the results clock's now: the process that ran its work is gone
/// (ADR-0059). The evidence clock does not move.
async fn interrupt_running_steps(state: &AppState) -> Result<(), String> {
    let interrupted = state
        .storage
        .investigations
        .interrupt_running_steps(state.results.clock.now())
        .await
        .map_err(|e| format!("interrupting the investigation steps left running failed: {e}"))?;
    tracing::info!(
        event_type = "investigations_boot",
        interrupted,
        "investigation steps left running by an ended process interrupted"
    );
    Ok(())
}

/// Mark `error` every report run a process that ended left `running`. The
/// advisory lock is held, so no live sibling owns one, and the listener has
/// not served, so no claim this process admitted can be among them.
async fn mark_stale_report_runs(state: &AppState) -> Result<(), String> {
    state
        .storage
        .schedule
        .cleanup_stale_runs()
        .await
        .map(drop)
        .map_err(|e| format!("boot stale report-run recovery failed: {e}"))
}

async fn prepare_query_only(state: &AppState, config: &Config) -> Result<(), String> {
    // A query-only node skips the conformance pass, but `/api/v1/schema`
    // still answers from this catalog's pins, which describe the archive
    // only if this catalog wrote it.
    let identity =
        conform::verify_archive_identity(&state.storage.catalog, &config.data.base_dir()).await?;
    match identity {
        ArchiveIdentity::Unproven => tracing::warn!(
            event_type = "catalog_identity_unproven",
            "query-only node: the archive carries no conformance marker, so \
             the pins /api/v1/schema advertises are not proven to describe \
             it — boot once with [ingest] enabled = true to run the \
             conformance pass, and check for skipped paths if it has"
        ),
        identity => tracing::info!(
            event_type = "catalog_identity",
            identity = ?identity,
            "query-only node: archive belongs to the connected catalog"
        ),
    }

    let wal_dir = config.wal_dir();
    let files = tokio::task::spawn_blocking(move || count_wal_files(&wal_dir))
        .await
        .map_err(|e| join_failure_text("WAL presence check", e))?;
    if files > 0 {
        tracing::warn!(
            event_type = "wal_present_on_query_node",
            files,
            "query-only node found WAL files it never reads; their rows are \
             missing from this node's answers until an ingest node compacts them"
        );
    }
    Ok(())
}

fn count_wal_files(wal_dir: &Path) -> u64 {
    let envs = crate::env_dirs::try_list_env_dirs(wal_dir).unwrap_or_default();
    envs.iter()
        .filter_map(|(_, dir)| std::fs::read_dir(dir).ok())
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter(|entry| {
            Path::new(&entry.file_name())
                .extension()
                .is_some_and(|ext| ext == "ndjson")
        })
        .count() as u64
}

#[cfg(test)]
mod tests {
    use super::count_wal_files;

    #[test]
    fn counts_ndjson_in_env_directories_only() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path();
        assert_eq!(count_wal_files(&wal.join("missing")), 0);
        for (rel, body) in [
            ("prod/api_1_0001.ndjson", "{}\n"),
            ("prod/web_2_0002.ndjson", "{}\n"),
            ("lab/api_3_0003.ndjson", "{}\n"),
            // Not WAL files: siblings the writer and compaction leave.
            ("prod/api_4_0004.ndjson.tmp", ""),
            ("prod/api_5_0005.ndjson.merged", ""),
            ("prod/.publish-api.json", ""),
            // Outside the env layout.
            ("loose.ndjson", "{}\n"),
            ("prod/nested/api_6_0006.ndjson", "{}\n"),
            ("Not An Env/api_7_0007.ndjson", "{}\n"),
        ] {
            let path = wal.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        assert_eq!(count_wal_files(wal), 3);
    }
}
