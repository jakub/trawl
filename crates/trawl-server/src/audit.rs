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
//! Under ADR-0006 a role name alone says nothing durable about capability:
//! `fleet-admin roles add-perm` mutates what a role grants without touching
//! any key. So the snapshot covers *both* sides of the grant: each key's role
//! set and each role's `(app, permission)` bundle plus `rate_rpm`. Every event
//! that names a key also carries its roles and the permissions they resolve
//! to in the role catalog of the same poll. That is the catalog the poll
//! observed, not proof of what the key could do at the instant of a change:
//! a role edited and a key revoked between two polls both land in one poll.
//!
//! The trail reports observed changes, not reconstructed history (ADR-0004,
//! 2026-11-06 amendment). Each poll diffs the fetched state against the last
//! whole snapshot and reports the differences in a fixed reporting order:
//! role-catalog events first, then keys in ascending id order (see [`diff`]).
//! That order is not the order the mutations happened in. Several mutations
//! between two polls collapse into one difference, and the poll cannot tell
//! which came first.
//!
//! The first snapshot is the baseline: the task emits nothing for the state
//! it finds there and does not replay history. A failed first snapshot is
//! retried every interval until one succeeds. If any attempt failed, the
//! task logs `audit_gap` with the unaudited window before `audit_start`.
//! Changes made before the baseline, inside that window included, are not
//! backfilled. A failed later poll keeps the last whole snapshot, so the
//! next successful poll reports what changed since it.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use chrono::Utc;
use fleet_auth::{ApiKeyInfo, KeyStore, Role};
use tokio::sync::watch;
use tokio::task::JoinHandle;

#[cfg(any(test, feature = "test-support"))]
mod test_hooks;
#[cfg(any(test, feature = "test-support"))]
pub use test_hooks::{AuditHold, AuditHooks, INJECTED_ERROR_TEXT, Point, Step};

/// Spawn the key audit polling task.
///
/// Polls the `KeyStore` every `interval` for key and role changes and emits
/// tracing events. Returns a `JoinHandle` for shutdown coordination: the task
/// ends with `audit_stop` once `shutdown` holds `true` or its sender is gone,
/// including while the first snapshot is still being retried.
pub fn spawn_audit_task(
    key_store: KeyStore,
    interval: Duration,
    shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let audit = Audit {
        key_store,
        interval,
        #[cfg(any(test, feature = "test-support"))]
        hooks: AuditHooks::default(),
    };
    tokio::spawn(audit.run(shutdown))
}

/// [`spawn_audit_task`] with fault and barrier hooks the caller keeps a
/// clone of.
#[cfg(any(test, feature = "test-support"))]
pub fn spawn_audit_task_with_hooks(
    key_store: KeyStore,
    interval: Duration,
    shutdown: watch::Receiver<bool>,
    hooks: AuditHooks,
) -> JoinHandle<()> {
    let audit = Audit {
        key_store,
        interval,
        hooks,
    };
    tokio::spawn(audit.run(shutdown))
}

/// Which loop of the task a fetch or a wait belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Taking the first snapshot, the baseline.
    Init,
    /// Diffing a later snapshot against the baseline.
    Poll,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Poll => "poll",
        }
    }
}

/// A keystore read failed. It names only the operation: the `AuthError`
/// can carry database text, which has no place in the audit trail.
struct FetchError {
    operation: &'static str,
}

struct Audit {
    key_store: KeyStore,
    interval: Duration,
    #[cfg(any(test, feature = "test-support"))]
    hooks: AuditHooks,
}

impl Audit {
    async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let Some(mut snapshot) = self.baseline(&mut shutdown).await else {
            log_stop();
            return;
        };

        tracing::info!(
            event_type = "lifecycle",
            action = "audit_start",
            interval_secs = self.interval.as_secs(),
            keys = snapshot.keys.len(),
            roles = snapshot.roles.len(),
            "key audit task started"
        );

        loop {
            if unless_stopped(&mut shutdown, self.wait(Phase::Poll))
                .await
                .is_none()
            {
                break;
            }
            let Some(fetched) = unless_stopped(&mut shutdown, self.fetch(Phase::Poll)).await else {
                break;
            };
            match fetched {
                // No await from the diff to the replaced baseline, so a
                // shutdown cannot land between the events and the snapshot
                // they were diffed against.
                Ok((keys, roles)) => {
                    let (events, next) = diff(&snapshot, &keys, &roles);
                    for event in &events {
                        emit(event);
                    }
                    snapshot = next;
                }
                Err(e) => tracing::warn!(
                    event_type = "audit_error",
                    phase = Phase::Poll.as_str(),
                    operation = e.operation,
                    "failed to poll key changes; keeping the last snapshot"
                ),
            }
        }
        log_stop();
    }

    /// Take the first snapshot, retrying every `interval` until it succeeds.
    /// `None` means shutdown came first.
    ///
    /// A recovery after failed attempts logs `audit_gap`: the window from
    /// the first attempt to the accepted baseline is unaudited, and nothing
    /// reconstructs it.
    async fn baseline(&self, shutdown: &mut watch::Receiver<bool>) -> Option<Snapshot> {
        // Before the first await: a change made from here on, before a fetch
        // succeeds, is in no snapshot the diff ever compares.
        let unaudited_from = Utc::now();
        let mut failures: u32 = 0;
        loop {
            match unless_stopped(shutdown, self.fetch(Phase::Init)).await? {
                Ok((keys, roles)) => {
                    if failures > 0 {
                        tracing::warn!(
                            event_type = "audit_gap",
                            unaudited_from = %unaudited_from.to_rfc3339(),
                            unaudited_until = %Utc::now().to_rfc3339(),
                            attempts = failures,
                            reconstruction = "none",
                            "key audit baseline established without reconstruction; \
                             key and role changes between unaudited_from and \
                             unaudited_until are unaudited"
                        );
                    }
                    return Some(snapshot_of(&keys, &roles));
                }
                Err(e) => {
                    failures += 1;
                    tracing::error!(
                        event_type = "audit_error",
                        phase = Phase::Init.as_str(),
                        operation = e.operation,
                        attempt = failures,
                        "failed to snapshot initial key state; retrying after the audit interval"
                    );
                }
            }
            unless_stopped(shutdown, self.wait(Phase::Init)).await?;
        }
    }

    /// Fetch the current keystore state. Both reads must succeed for the
    /// caller to get anything, so a poll never diffs half a snapshot.
    #[cfg_attr(
        not(any(test, feature = "test-support")),
        expect(unused_variables, reason = "phase selects test hooks only")
    )]
    async fn fetch(&self, phase: Phase) -> Result<(Vec<ApiKeyInfo>, Vec<Role>), FetchError> {
        #[cfg(any(test, feature = "test-support"))]
        self.hooks.reach(phase, Point::BeforeListKeys).await;
        let keys = async {
            #[cfg(any(test, feature = "test-support"))]
            if let Some(injected) = self.hooks.take_failure(phase, Step::ListKeys) {
                return Err(injected);
            }
            self.key_store.list_keys(false).await
        }
        .await
        .map_err(|_| FetchError {
            operation: "list_keys",
        })?;

        #[cfg(any(test, feature = "test-support"))]
        self.hooks.reach(phase, Point::BeforeListRoles).await;
        let roles = async {
            #[cfg(any(test, feature = "test-support"))]
            if let Some(injected) = self.hooks.take_failure(phase, Step::ListRoles) {
                return Err(injected);
            }
            self.key_store.list_roles().await
        }
        .await
        .map_err(|_| FetchError {
            operation: "list_roles",
        })?;

        Ok((keys, roles))
    }

    /// Sleep one interval.
    #[cfg_attr(
        not(any(test, feature = "test-support")),
        expect(unused_variables, reason = "phase selects test hooks only")
    )]
    async fn wait(&self, phase: Phase) {
        #[cfg(any(test, feature = "test-support"))]
        self.hooks.reach(phase, Point::BeforeWait).await;
        tokio::time::sleep(self.interval).await;
    }
}

/// Resolves once shutdown is requested: the value is `true`, now or later,
/// or the sender is gone. `wait_for` checks the current value first, so a
/// request sent before the task looks is not missed.
async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    // Err: the sender was dropped, and nothing else can stop the task.
    let _ = shutdown.wait_for(|stop| *stop).await;
}

/// Run `work` unless shutdown comes first, in which case `None`.
async fn unless_stopped<T>(
    shutdown: &mut watch::Receiver<bool>,
    work: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        () = stopped(shutdown) => None,
        out = work => Some(out),
    }
}

fn log_stop() {
    tracing::info!(
        event_type = "lifecycle",
        action = "audit_stop",
        "key audit task shutting down"
    );
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

/// One audit event, as the diff of two snapshots reports it.
///
/// Each variant is one `event_type`. [`emit`] turns it into the tracing event
/// the WAL records.
#[derive(Debug, Clone, PartialEq)]
enum AuditEvent {
    RoleCreated {
        role: String,
        rate_rpm: Option<u32>,
        permissions: Vec<String>,
    },
    RoleChanged {
        role: String,
        permissions_added: Vec<String>,
        permissions_removed: Vec<String>,
        rate_rpm_before: Option<u32>,
        rate_rpm_after: Option<u32>,
        permissions: Vec<String>,
    },
    RoleDeleted {
        role: String,
    },
    KeyCreated {
        key_id: i64,
        prefix: String,
        name: String,
        kind: String,
        roles: Vec<String>,
        permissions: Vec<String>,
        /// `created_at`, rfc3339.
        occurred_at: String,
    },
    KeyRolesChanged {
        key_id: i64,
        prefix: String,
        name: String,
        roles_before: Vec<String>,
        roles: Vec<String>,
        permissions: Vec<String>,
    },
    KeyRevoked {
        key_id: i64,
        prefix: String,
        name: String,
        roles: Vec<String>,
        permissions: Vec<String>,
        /// `revoked_at`, rfc3339, or `"unknown"` when the row has none.
        occurred_at: String,
    },
}

/// The events one poll observes between `prev` and the fetched state, and
/// the snapshot that replaces `prev` once they are emitted.
///
/// Role-catalog events come first, then each key in ascending id order:
/// `key_created` when the key is new, `key_roles_changed` when `prev` holds
/// it with other roles, and `key_revoked` when it is inactive and was new or
/// active in `prev`. The three checks are independent, so a key first seen
/// inactive reports created then revoked, and a revoke never hides a role
/// change. A key missing from the fetch reports nothing, and so does a key
/// that turns active again.
fn diff(prev: &Snapshot, keys: &[ApiKeyInfo], roles: &[Role]) -> (Vec<AuditEvent>, Snapshot) {
    let current = snapshot_of(keys, roles);
    let mut events = diff_roles(&prev.roles, &current.roles);

    let mut keys: Vec<&ApiKeyInfo> = keys.iter().collect();
    keys.sort_by_key(|k| k.id);
    for key in keys {
        let before = prev.keys.get(&key.id);
        let permissions = || effective_permissions(&key.roles, &current.roles);
        if before.is_none() {
            events.push(AuditEvent::KeyCreated {
                key_id: key.id,
                prefix: key.prefix.clone(),
                name: key.name.clone(),
                kind: key.kind.to_string(),
                roles: key.roles.clone(),
                permissions: permissions(),
                occurred_at: key.created_at.to_rfc3339(),
            });
        }
        if let Some(before) = before
            && before.roles != key.roles
        {
            events.push(AuditEvent::KeyRolesChanged {
                key_id: key.id,
                prefix: key.prefix.clone(),
                name: key.name.clone(),
                roles_before: before.roles.clone(),
                roles: key.roles.clone(),
                permissions: permissions(),
            });
        }
        if !key.active && before.is_none_or(|b| b.active) {
            events.push(AuditEvent::KeyRevoked {
                key_id: key.id,
                prefix: key.prefix.clone(),
                name: key.name.clone(),
                roles: key.roles.clone(),
                permissions: permissions(),
                occurred_at: key
                    .revoked_at
                    .map_or_else(|| "unknown".to_owned(), |t| t.to_rfc3339()),
            });
        }
    }

    (events, current)
}

/// Role-catalog events between two catalogs: created and changed roles in
/// name order, then deleted roles in name order.
fn diff_roles(
    prev: &BTreeMap<String, RoleSnapshot>,
    current: &BTreeMap<String, RoleSnapshot>,
) -> Vec<AuditEvent> {
    let mut events = Vec::new();
    for (name, role) in current {
        let Some(before) = prev.get(name) else {
            events.push(AuditEvent::RoleCreated {
                role: name.clone(),
                rate_rpm: role.rate_rpm,
                permissions: role.permissions.clone(),
            });
            continue;
        };
        if before.permissions == role.permissions && before.rate_rpm == role.rate_rpm {
            continue;
        }
        let added = role
            .permissions
            .iter()
            .filter(|p| !before.permissions.contains(*p))
            .cloned()
            .collect();
        let removed = before
            .permissions
            .iter()
            .filter(|p| !role.permissions.contains(*p))
            .cloned()
            .collect();
        events.push(AuditEvent::RoleChanged {
            role: name.clone(),
            permissions_added: added,
            permissions_removed: removed,
            rate_rpm_before: before.rate_rpm,
            rate_rpm_after: role.rate_rpm,
            permissions: role.permissions.clone(),
        });
    }
    for name in prev.keys().filter(|n| !current.contains_key(*n)) {
        events.push(AuditEvent::RoleDeleted { role: name.clone() });
    }
    events
}

/// Log one audit event. The `(detected by audit)` suffix tells these lines
/// from fleet-auth's own events of the same `event_type`.
fn emit(event: &AuditEvent) {
    match event {
        AuditEvent::RoleCreated {
            role,
            rate_rpm,
            permissions,
        } => tracing::info!(
            event_type = "role_created",
            role = %role,
            rate_rpm = ?rate_rpm,
            permissions = ?permissions,
            "role created (detected by audit)"
        ),
        AuditEvent::RoleChanged {
            role,
            permissions_added,
            permissions_removed,
            rate_rpm_before,
            rate_rpm_after,
            permissions,
        } => tracing::info!(
            event_type = "role_changed",
            role = %role,
            permissions_added = ?permissions_added,
            permissions_removed = ?permissions_removed,
            rate_rpm_before = ?rate_rpm_before,
            rate_rpm_after = ?rate_rpm_after,
            permissions = ?permissions,
            "role capability changed (detected by audit)"
        ),
        AuditEvent::RoleDeleted { role } => tracing::info!(
            event_type = "role_deleted",
            role = %role,
            "role deleted (detected by audit)"
        ),
        AuditEvent::KeyCreated {
            key_id,
            prefix,
            name,
            kind,
            roles,
            permissions,
            occurred_at,
        } => tracing::info!(
            event_type = "key_created",
            key_id = *key_id,
            prefix = %prefix,
            name = %name,
            kind = %kind,
            roles = ?roles,
            permissions = ?permissions,
            occurred_at = %occurred_at,
            "API key created (detected by audit)"
        ),
        AuditEvent::KeyRolesChanged {
            key_id,
            prefix,
            name,
            roles_before,
            roles,
            permissions,
        } => tracing::info!(
            event_type = "key_roles_changed",
            key_id = *key_id,
            prefix = %prefix,
            name = %name,
            roles_before = ?roles_before,
            roles = ?roles,
            permissions = ?permissions,
            "API key roles changed (detected by audit)"
        ),
        AuditEvent::KeyRevoked {
            key_id,
            prefix,
            name,
            roles,
            permissions,
            occurred_at,
        } => tracing::info!(
            event_type = "key_revoked",
            key_id = *key_id,
            prefix = %prefix,
            name = %name,
            roles = ?roles,
            permissions = ?permissions,
            occurred_at = %occurred_at,
            "API key revoked (detected by audit)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeZone, Utc};
    use fleet_auth::{PrincipalKind, RolePermission};

    use super::*;

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 6, hour, 0, 0).unwrap()
    }

    fn role(name: &str, perms: &[&str]) -> Role {
        Role {
            id: 0,
            name: name.to_owned(),
            rate_rpm: None,
            permissions: perms
                .iter()
                .map(|p| RolePermission {
                    app: "trawl".into(),
                    permission: (*p).to_owned(),
                })
                .collect(),
        }
    }

    /// A key created at 01:00; an inactive one was revoked at 02:00.
    fn key(id: i64, roles: &[&str], active: bool) -> ApiKeyInfo {
        ApiKeyInfo {
            id,
            prefix: format!("pfx{id}"),
            name: format!("key-{id}"),
            kind: PrincipalKind::Service,
            roles: roles.iter().map(|r| (*r).to_owned()).collect(),
            active,
            created_at: at(1),
            expires_at: None,
            last_used: None,
            revoked_at: (!active).then(|| at(2)),
        }
    }

    fn catalog() -> Vec<Role> {
        vec![role("a", &["query"]), role("b", &["ingest", "query"])]
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn created(key: &ApiKeyInfo, permissions: &[&str]) -> AuditEvent {
        AuditEvent::KeyCreated {
            key_id: key.id,
            prefix: key.prefix.clone(),
            name: key.name.clone(),
            kind: "service".into(),
            roles: key.roles.clone(),
            permissions: strings(permissions),
            occurred_at: at(1).to_rfc3339(),
        }
    }

    fn revoked(key: &ApiKeyInfo, permissions: &[&str]) -> AuditEvent {
        AuditEvent::KeyRevoked {
            key_id: key.id,
            prefix: key.prefix.clone(),
            name: key.name.clone(),
            roles: key.roles.clone(),
            permissions: strings(permissions),
            occurred_at: at(2).to_rfc3339(),
        }
    }

    fn roles_changed(before: &[&str], key: &ApiKeyInfo, permissions: &[&str]) -> AuditEvent {
        AuditEvent::KeyRolesChanged {
            key_id: key.id,
            prefix: key.prefix.clone(),
            name: key.name.clone(),
            roles_before: strings(before),
            roles: key.roles.clone(),
            permissions: strings(permissions),
        }
    }

    /// The events of one poll from `prev_keys` to `keys`, over an unchanged
    /// role catalog.
    fn events(prev_keys: &[ApiKeyInfo], keys: &[ApiKeyInfo]) -> Vec<AuditEvent> {
        let prev = snapshot_of(prev_keys, &catalog());
        diff(&prev, keys, &catalog()).0
    }

    #[test]
    fn first_seen_inactive_key_reports_created_then_revoked() {
        let k = key(7, &["a"], false);
        assert_eq!(
            events(&[], std::slice::from_ref(&k)),
            vec![created(&k, &["trawl:query"]), revoked(&k, &["trawl:query"])]
        );
    }

    #[test]
    fn roles_change_and_revoke_in_one_poll_report_both() {
        let now = key(7, &["b"], false);
        assert_eq!(
            events(&[key(7, &["a"], true)], std::slice::from_ref(&now)),
            vec![
                roles_changed(&["a"], &now, &["trawl:ingest", "trawl:query"]),
                revoked(&now, &["trawl:ingest", "trawl:query"]),
            ]
        );
    }

    #[test]
    fn revoke_only_reports_revoked() {
        let now = key(7, &["a"], false);
        assert_eq!(
            events(&[key(7, &["a"], true)], std::slice::from_ref(&now)),
            vec![revoked(&now, &["trawl:query"])]
        );
    }

    #[test]
    fn roles_change_only_reports_roles_changed() {
        let now = key(7, &["a", "b"], true);
        assert_eq!(
            events(&[key(7, &["a"], true)], std::slice::from_ref(&now)),
            vec![roles_changed(
                &["a"],
                &now,
                &["trawl:ingest", "trawl:query"]
            )]
        );
    }

    #[test]
    fn first_seen_active_key_reports_created() {
        let k = key(7, &["a"], true);
        assert_eq!(
            events(&[], std::slice::from_ref(&k)),
            vec![created(&k, &["trawl:query"])]
        );
    }

    #[test]
    fn unchanged_key_reports_nothing() {
        let k = key(7, &["a"], true);
        assert_eq!(
            events(std::slice::from_ref(&k), std::slice::from_ref(&k)),
            vec![]
        );
    }

    #[test]
    fn key_inactive_in_baseline_with_unchanged_roles_reports_nothing() {
        let k = key(7, &["a"], false);
        assert_eq!(
            events(std::slice::from_ref(&k), std::slice::from_ref(&k)),
            vec![]
        );
    }

    /// `fleet-admin roles delete --force` strips a role from revoked keys
    /// too. That is a role change, and the key is not revoked again.
    #[test]
    fn key_inactive_in_baseline_whose_roles_change_reports_roles_changed_only() {
        let now = key(7, &[], false);
        assert_eq!(
            events(&[key(7, &["a"], false)], std::slice::from_ref(&now)),
            vec![roles_changed(&["a"], &now, &[])]
        );
    }

    #[test]
    fn revoke_without_revoked_at_reports_unknown_occurred_at() {
        let mut now = key(7, &["a"], false);
        now.revoked_at = None;
        assert_eq!(
            events(&[key(7, &["a"], true)], &[now.clone()]),
            vec![AuditEvent::KeyRevoked {
                key_id: 7,
                prefix: "pfx7".into(),
                name: "key-7".into(),
                roles: strings(&["a"]),
                permissions: strings(&["trawl:query"]),
                occurred_at: "unknown".into(),
            }]
        );
    }

    /// The permissions resolve against the catalog of the same poll: here
    /// role `a` gains `export` in the poll that sees the revoke.
    #[test]
    fn key_revoked_carries_roles_and_permissions_of_the_same_poll() {
        let prev = snapshot_of(&[key(7, &["a", "b"], true)], &catalog());
        let roles = vec![role("a", &["export", "query"]), role("b", &["ingest"])];
        let (events, _) = diff(&prev, &[key(7, &["a", "b"], false)], &roles);
        assert_eq!(
            events.last(),
            Some(&AuditEvent::KeyRevoked {
                key_id: 7,
                prefix: "pfx7".into(),
                name: "key-7".into(),
                roles: strings(&["a", "b"]),
                permissions: strings(&["trawl:export", "trawl:ingest", "trawl:query"]),
                occurred_at: at(2).to_rfc3339(),
            })
        );
    }

    /// Role `a` changes, `b` is deleted, `c` is unchanged and `d` is new,
    /// while three keys change. Every role event comes before every key
    /// event. Key 2 still names the deleted role `b`, which contributes no
    /// permissions.
    #[test]
    fn role_catalog_events_precede_every_key_event() {
        let prev_roles = vec![
            role("a", &["query"]),
            role("b", &["ingest"]),
            role("c", &[]),
        ];
        let prev = snapshot_of(&[key(1, &["a"], true), key(2, &["c"], true)], &prev_roles);
        let roles = vec![
            role("a", &["export"]),
            role("c", &[]),
            role("d", &["query"]),
        ];
        let k1 = key(1, &["a"], false);
        let k2 = key(2, &["b", "d"], true);
        let k3 = key(3, &["d"], true);
        let (events, _) = diff(&prev, &[k3.clone(), k1.clone(), k2.clone()], &roles);
        assert_eq!(
            events,
            vec![
                AuditEvent::RoleChanged {
                    role: "a".into(),
                    permissions_added: strings(&["trawl:export"]),
                    permissions_removed: strings(&["trawl:query"]),
                    rate_rpm_before: None,
                    rate_rpm_after: None,
                    permissions: strings(&["trawl:export"]),
                },
                AuditEvent::RoleCreated {
                    role: "d".into(),
                    rate_rpm: None,
                    permissions: strings(&["trawl:query"]),
                },
                AuditEvent::RoleDeleted { role: "b".into() },
                revoked(&k1, &["trawl:export"]),
                roles_changed(&["c"], &k2, &["trawl:query"]),
                created(&k3, &["trawl:query"]),
            ]
        );
    }

    #[test]
    fn keys_are_reported_in_ascending_id_regardless_of_input_order() {
        let (k2, k5, k9) = (key(2, &[], true), key(5, &[], true), key(9, &[], true));
        assert_eq!(
            events(&[], &[k9.clone(), k2.clone(), k5.clone()]),
            vec![created(&k2, &[]), created(&k5, &[]), created(&k9, &[])]
        );
    }

    #[test]
    fn a_key_missing_from_the_fetch_reports_nothing() {
        assert_eq!(events(&[key(7, &["a"], true)], &[]), vec![]);
    }

    #[tokio::test(start_paused = true)]
    async fn stopped_resolves_when_shutdown_is_already_requested() {
        // `changed()` would wait here: the initial value counts as seen.
        let (_tx, mut rx) = watch::channel(true);
        tokio::time::timeout(Duration::from_secs(3600), stopped(&mut rx))
            .await
            .expect("an already-true value stops at once");
    }

    #[tokio::test(start_paused = true)]
    async fn stopped_resolves_when_the_sender_is_dropped() {
        let (tx, mut rx) = watch::channel(false);
        drop(tx);
        tokio::time::timeout(Duration::from_secs(3600), stopped(&mut rx))
            .await
            .expect("a dropped sender stops at once");
    }

    #[tokio::test(start_paused = true)]
    async fn stopped_ignores_a_false_update() {
        let (tx, mut rx) = watch::channel(false);
        tx.send(false).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(3600), stopped(&mut rx))
                .await
                .is_err(),
            "a false value must not stop the task"
        );
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(3600), stopped(&mut rx))
            .await
            .expect("a true value stops");
    }
}
