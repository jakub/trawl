// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Key audit polling task.
//!
//! Periodically polls the fleet-auth Postgres keystore for key changes made
//! out-of-process by fleet-admin. Emits `key_created` / `key_revoked`
//! tracing events that flow through the [`WalLayer`] into parquet,
//! providing an audit trail even though fleet-admin has no WAL subscriber.
//!
//! The task snapshots all key states on startup and only emits events for
//! *changes* detected on subsequent polls (no replay of historical keys).

use std::collections::HashMap;
use std::time::Duration;

use fleet_auth::KeyStore;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Spawn the key audit polling task.
///
/// Polls the `KeyStore` every `interval` for new or revoked keys and emits
/// tracing events. Returns a `JoinHandle` for shutdown coordination.
pub fn spawn_audit_task(
    key_store: KeyStore,
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

/// Build the initial snapshot from the current keystore state.
async fn initial_snapshot(key_store: &KeyStore) -> Result<HashMap<i64, KeySnapshot>, String> {
    let keys = key_store
        .list_keys(false)
        .await
        .map_err(|e| format!("list_keys failed: {e}"))?;
    Ok(keys
        .into_iter()
        .map(|k| (k.id, KeySnapshot { active: k.active }))
        .collect())
}

/// Poll the keystore and emit events for any changes since the last snapshot.
async fn poll_changes(
    key_store: &KeyStore,
    snapshot: &mut HashMap<i64, KeySnapshot>,
) -> Result<(), String> {
    let current = key_store
        .list_keys(false)
        .await
        .map_err(|e| format!("list_keys failed: {e}"))?;

    for key in &current {
        match snapshot.get(&key.id) {
            None => {
                // New key — not in our snapshot.
                tracing::info!(
                    event_type = "key_created",
                    key_id = key.id,
                    prefix = %key.prefix,
                    name = %key.name,
                    kind = %key.kind,
                    roles = ?key.roles,
                    occurred_at = %key.created_at.to_rfc3339(),
                    "API key created (detected by audit)"
                );
            }
            Some(prev) if prev.active && !key.active => {
                // Key was revoked since last poll. fleet-auth timestamps are
                // typed DateTime<Utc>; render rfc3339 for the audit trail.
                let occurred_at = key
                    .revoked_at
                    .map_or_else(|| "unknown".to_owned(), |t| t.to_rfc3339());
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
