// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The boot steps that make the corpus ready to serve (ADR-0041 slice 2).
//!
//! They run after publication recovery and state construction, and before
//! self-telemetry, compaction, the other workers, the scheduler and the
//! listener start. `main` and the integration-test fixture both call
//! [`prepare_corpus`], so the tests boot the sequence trawld runs.

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use crate::catalog::conform::{self, ArchiveIdentity};
use crate::config::Config;
use crate::error::join_failure_text;
use crate::state::AppState;

/// Prepare the corpus for serving.
///
/// An ingest node runs these steps in order:
///
/// 1. Recover the rollup markers the gate's boot scan registered. A failure
///    does not stop the boot: reads stay refused as `rollup_pending` until
///    a compaction pass recovers the marker.
/// 2. Run the ADR-0009 boot conformance pass. It leaves alone every file
///    that a pending publication marker claims or that sits in a day
///    directory holding a rollup marker.
/// 3. Hydrate the surviving WAL into the hot buffer.
/// 4. Finish hydration on the publication gate: settled when every file
///    became resident, overhang otherwise, until a compaction pass proves
///    coverage.
///
/// Self-telemetry must not be active yet. A WAL file it wrote before step 3
/// would be inserted once and hydrated a second time.
///
/// A query-only node reads parquet only. It checks that the archive belongs
/// to the connected catalog, hydrates nothing, and logs one warning when it
/// finds WAL files, because it never reads them.
///
/// # Errors
///
/// Each stops the boot: a failed conformance pass (a per-path failure is
/// skipped inside the pass, not an error), an archive that names another
/// catalog, a hydration error, or a gate that was not born starting. The
/// last two are boot bugs, never properties of the WAL.
pub async fn prepare_corpus(state: &AppState, config: &Config) -> Result<(), String> {
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

    // Fatal on failure, like the epoch gate: a data root not proven
    // conformant must not serve queries. A per-path failure is not that: an
    // unreadable or foreign parquet file, or a subdirectory the walk cannot
    // enumerate, is skipped and counted inside the pass, so one bad path
    // cannot keep the daemon down. So is every file a publication marker
    // still claims after boot recovery, and every file in a day an
    // unresolved rollup marker holds: the pass leaves it untouched and
    // withholds completion, so the next boot conforms it once the marker
    // resolves.
    let summary = conform::ensure_conformance(
        &state.storage.catalog,
        &state.query.field_catalog,
        &config.data.base_dir(),
        &config.wal_dir(),
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

/// The query-only half of [`prepare_corpus`].
async fn prepare_query_only(state: &AppState, config: &Config) -> Result<(), String> {
    // A query-only node skips the conformance pass, but `/api/v1/schema`
    // still answers from this catalog's pins, which describe the archive
    // only if this catalog wrote it. Check the same dual-sided marker as a
    // gate, and refuse the boot rather than advertise a schema about
    // someone else's data. Only a marker naming another catalog does that:
    // an archive with no marker (what an incomplete conformance pass
    // leaves) warns and serves, exactly as the ingest node does for the
    // same corpus.
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

    // The restart guarantee covers ingest nodes only (ADR-0041): this node
    // never reads the WAL, so rows still there are missing from its answers.
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

/// Count the `*.ndjson` entries in the WAL's env directories, the layout
/// the writer produces and compaction takes. Bounded to that one level;
/// anything that cannot be listed counts nothing.
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
