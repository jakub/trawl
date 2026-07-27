// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Key audit polling task.
//!
//! Periodically polls the fleet-auth Postgres keystore for capability changes
//! made out-of-process by fleet-admin. Emits `key_created` / `key_revoked` /
//! `key_roles_changed` / `role_created` / `role_changed` / `role_deleted`
//! tracing events that flow through the [`WalLayer`] into parquet, providing
//! an audit trail even though fleet-admin has no WAL subscriber.
//!
//! Under ADR-0006 a role name alone says nothing durable about capability —
//! `fleet-admin roles add-perm` mutates what a role grants without touching
//! any key. So the snapshot covers *both* sides of the grant: each key's role
//! set and each role's `(app, permission)` bundle plus `rate_rpm`. Every event
//! that names a key also carries the permissions those roles resolved to at
//! that instant, so the trail can answer "what could key X do at time T".
//!
//! The task snapshots all key and role states on startup and only emits events
//! for *changes* detected on subsequent polls (no replay of history).

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use fleet_auth::{ApiKeyInfo, KeyStore, Role};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Spawn the key audit polling task.
///
/// Polls the `KeyStore` every `interval` for key and role changes and emits
/// tracing events. Returns a `JoinHandle` for shutdown coordination.
pub fn spawn_audit_task(
    key_store: KeyStore,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Snapshot current state to avoid replaying history on startup.
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
            keys = snapshot.keys.len(),
            roles = snapshot.roles.len(),
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
    /// Role names held by the key, sorted (as `list_keys` returns them).
    roles: Vec<String>,
}

/// State tracked per role for change detection: what the role actually grants.
struct RoleSnapshot {
    rate_rpm: Option<u32>,
    /// `app:permission` strings, sorted and deduped.
    permissions: Vec<String>,
}

/// Both halves of the grant model, as of one poll.
struct Snapshot {
    keys: HashMap<i64, KeySnapshot>,
    roles: BTreeMap<String, RoleSnapshot>,
}

/// Render a role's bundle as sorted, deduped `app:permission` strings.
fn permission_strings(role: &Role) -> Vec<String> {
    let mut perms: Vec<String> = role
        .permissions
        .iter()
        .map(|p| format!("{}:{}", p.app, p.permission))
        .collect();
    perms.sort();
    perms.dedup();
    perms
}

/// Resolve a key's role names against the role catalog: the union of every
/// permission its roles grant, sorted and deduped.
fn effective_permissions(
    roles: &[String],
    catalog: &BTreeMap<String, RoleSnapshot>,
) -> Vec<String> {
    let mut perms: Vec<String> = roles
        .iter()
        .filter_map(|name| catalog.get(name))
        .flat_map(|role| role.permissions.iter().cloned())
        .collect();
    perms.sort();
    perms.dedup();
    perms
}

/// Fetch the current keystore state.
async fn fetch(key_store: &KeyStore) -> Result<(Vec<ApiKeyInfo>, Vec<Role>), String> {
    let keys = key_store
        .list_keys(false)
        .await
        .map_err(|e| format!("list_keys failed: {e}"))?;
    let roles = key_store
        .list_roles()
        .await
        .map_err(|e| format!("list_roles failed: {e}"))?;
    Ok((keys, roles))
}

/// Collapse fetched state into the diffable snapshot.
fn snapshot_of(keys: &[ApiKeyInfo], roles: &[Role]) -> Snapshot {
    Snapshot {
        keys: keys
            .iter()
            .map(|k| {
                (
                    k.id,
                    KeySnapshot {
                        active: k.active,
                        roles: k.roles.clone(),
                    },
                )
            })
            .collect(),
        roles: roles
            .iter()
            .map(|r| {
                (
                    r.name.clone(),
                    RoleSnapshot {
                        rate_rpm: r.rate_rpm,
                        permissions: permission_strings(r),
                    },
                )
            })
            .collect(),
    }
}

/// Build the initial snapshot from the current keystore state.
async fn initial_snapshot(key_store: &KeyStore) -> Result<Snapshot, String> {
    let (keys, roles) = fetch(key_store).await?;
    Ok(snapshot_of(&keys, &roles))
}

/// Emit events for role mutations since the last snapshot.
fn diff_roles(prev: &BTreeMap<String, RoleSnapshot>, current: &BTreeMap<String, RoleSnapshot>) {
    for (name, role) in current {
        let Some(before) = prev.get(name) else {
            tracing::info!(
                event_type = "role_created",
                role = %name,
                rate_rpm = ?role.rate_rpm,
                permissions = ?role.permissions,
                "role created (detected by audit)"
            );
            continue;
        };
        if before.permissions == role.permissions && before.rate_rpm == role.rate_rpm {
            continue;
        }
        let added: Vec<&str> = role
            .permissions
            .iter()
            .filter(|p| !before.permissions.contains(*p))
            .map(String::as_str)
            .collect();
        let removed: Vec<&str> = before
            .permissions
            .iter()
            .filter(|p| !role.permissions.contains(*p))
            .map(String::as_str)
            .collect();
        tracing::info!(
            event_type = "role_changed",
            role = %name,
            permissions_added = ?added,
            permissions_removed = ?removed,
            rate_rpm_before = ?before.rate_rpm,
            rate_rpm_after = ?role.rate_rpm,
            permissions = ?role.permissions,
            "role capability changed (detected by audit)"
        );
    }

    for name in prev.keys().filter(|n| !current.contains_key(*n)) {
        tracing::info!(
            event_type = "role_deleted",
            role = %name,
            "role deleted (detected by audit)"
        );
    }
}

/// Poll the keystore and emit events for any changes since the last snapshot.
async fn poll_changes(key_store: &KeyStore, snapshot: &mut Snapshot) -> Result<(), String> {
    let (keys, roles) = fetch(key_store).await?;
    let current = snapshot_of(&keys, &roles);

    // Roles first: key events below quote permissions resolved against the
    // current catalog, so the role mutation must precede them in the WAL.
    diff_roles(&snapshot.roles, &current.roles);

    for key in &keys {
        match snapshot.keys.get(&key.id) {
            None => {
                // New key — not in our snapshot.
                tracing::info!(
                    event_type = "key_created",
                    key_id = key.id,
                    prefix = %key.prefix,
                    name = %key.name,
                    kind = %key.kind,
                    roles = ?key.roles,
                    permissions = ?effective_permissions(&key.roles, &current.roles),
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
            Some(prev) if prev.roles != key.roles => {
                tracing::info!(
                    event_type = "key_roles_changed",
                    key_id = key.id,
                    prefix = %key.prefix,
                    name = %key.name,
                    roles_before = ?prev.roles,
                    roles = ?key.roles,
                    permissions = ?effective_permissions(&key.roles, &current.roles),
                    "API key roles changed (detected by audit)"
                );
            }
            _ => {} // No change.
        }
    }

    *snapshot = current;

    Ok(())
}
