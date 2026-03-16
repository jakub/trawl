// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Key audit polling task.
//!
//! Periodically polls the `SQLite` auth database for key changes made by
//! trawl-admin (which writes directly to the same `auth.db`). Emits
//! `key_created` / `key_revoked` tracing events that flow through the
//! [`WalLayer`] into parquet, providing an audit trail even though
//! trawl-admin has no WAL subscriber.
//!
//! The task snapshots all key states on startup and only emits events for
//! *changes* detected on subsequent polls (no replay of historical keys).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use trawl_auth::KeyStore;

/// Spawn the key audit polling task.
///
/// Polls the `KeyStore` every `interval` for new or revoked keys and emits
/// tracing events. Returns a `JoinHandle` for shutdown coordination.
pub fn spawn_audit_task(
    key_store: Arc<Mutex<KeyStore>>,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Snapshot current key states to avoid replaying history on startup.
        let mut snapshot = match initial_snapshot(&key_store).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    event_type = "audit_error",
                    error = %e,
                    "failed to snapshot initial key state, audit task disabled"
                );
                return;
            }
        };

        tracing::info!(
            event_type = "lifecycle",
            action = "audit_start",
            interval_secs = interval.as_secs(),
            keys = snapshot.len(),
            "key audit task started"
        );

        loop {
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                _ = shutdown.changed() => {
                    tracing::info!(
                        event_type = "lifecycle",
                        action = "audit_stop",
                        "key audit task shutting down"
                    );
                    return;
                }
            }

            match poll_changes(&key_store, &mut snapshot).await {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!(
                        event_type = "audit_error",
                        error = %e,
                        "failed to poll key changes"
                    );
                }
            }
        }
    })
}

/// State tracked per key for change detection.
struct KeySnapshot {
    active: bool,
}

/// Build the initial snapshot from the current database state.
async fn initial_snapshot(
    key_store: &Arc<Mutex<KeyStore>>,
) -> Result<HashMap<i64, KeySnapshot>, String> {
    let store = Arc::clone(key_store);
    tokio::task::spawn_blocking(move || {
        let guard = store.lock();
        let keys = guard
            .list_keys(false)
            .map_err(|e| format!("list_keys failed: {e}"))?;
        Ok(keys
            .into_iter()
            .map(|k| (k.id, KeySnapshot { active: k.active }))
            .collect())
    })
    .await
    .map_err(|e| format!("spawn_blocking panicked: {e}"))?
}

/// Poll the database and emit events for any changes since the last snapshot.
async fn poll_changes(
    key_store: &Arc<Mutex<KeyStore>>,
    snapshot: &mut HashMap<i64, KeySnapshot>,
) -> Result<(), String> {
    let store = Arc::clone(key_store);
    let current = tokio::task::spawn_blocking(move || {
        let guard = store.lock();
        guard
            .list_keys(false)
            .map_err(|e| format!("list_keys failed: {e}"))
    })
    .await
    .map_err(|e| format!("spawn_blocking panicked: {e}"))??;

    for key in &current {
        match snapshot.get(&key.id) {
            None => {
                // New key — not in our snapshot.
                tracing::info!(
                    event_type = "key_created",
                    key_id = key.id,
                    prefix = %key.prefix,
                    name = %key.name,
                    role = %key.role,
                    occurred_at = %key.created_at,
                    "API key created (detected by audit)"
                );
            }
            Some(prev) if prev.active && !key.active => {
                // Key was revoked since last poll.
                let occurred_at = key.revoked_at.as_deref().unwrap_or("unknown");
                tracing::info!(
                    event_type = "key_revoked",
                    key_id = key.id,
                    prefix = %key.prefix,
                    name = %key.name,
                    occurred_at = %occurred_at,
                    "API key revoked (detected by audit)"
                );
            }
            _ => {} // No change.
        }
    }

    // Rebuild snapshot from current state.
    *snapshot = current
        .into_iter()
        .map(|k| (k.id, KeySnapshot { active: k.active }))
        .collect();

    Ok(())
}
