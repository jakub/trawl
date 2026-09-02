// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `fleet-admin roles ...` — CRUD for data-defined roles (ADR-0006).
//!
//! A role is a named cross-app bundle of `APP:PERMISSION` strings with an
//! optional `rate_rpm` ceiling. A mutation naming a permission absent from
//! the `app_permissions` registry warns on stderr and persists anyway: a
//! role may pre-name a permission an app is about to ship, so the warning
//! is only there to catch typos.

use fleet_auth::{KeyStore, Role, RolePermission};

use crate::commands::keys::{RoleName, confirm_or_refuse};
use crate::error::AdminError;

/// Parse an `APP:PERMISSION` argument into a [`RolePermission`].
///
/// Both halves are shape-validated with fleet-auth's canonical rules at the
/// clap boundary so malformed values never reach the SQL layer.
pub fn parse_perm(s: &str) -> Result<RolePermission, AdminError> {
    let bad = |reason: &'static str| AdminError::InvalidPermSpec {
        input: s.to_owned(),
        reason,
    };
    if s.chars().any(char::is_whitespace) {
        return Err(bad("whitespace not allowed in APP:PERMISSION"));
    }
    let (app, permission) = s
        .split_once(':')
        .ok_or_else(|| bad("expected APP:PERMISSION"))?;
    if app.is_empty() || permission.is_empty() {
        return Err(bad("empty app or permission"));
    }
    fleet_auth::validate_app_namespace(app)?;
    fleet_auth::validate_permission(permission)?;
    Ok(RolePermission {
        app: app.to_owned(),
        permission: permission.to_owned(),
    })
}

/// Warn on stderr for every named permission missing from the
/// `app_permissions` registry. The rows persist regardless (warn-only
/// registry) and the command still exits 0.
async fn warn_unknown_permissions(
    store: &KeyStore,
    permissions: &[RolePermission],
) -> Result<(), AdminError> {
    for rp in permissions {
        if !store.is_known_permission(&rp.app, &rp.permission).await? {
            eprintln!(
                "warning: {}:{} is not in the {} permission registry — persisted anyway \
                 (an app may be about to ship it; double-check for typos)",
                rp.app, rp.permission, rp.app
            );
        }
    }
    Ok(())
}

/// Render one role's detail block to stderr.
fn print_role(role: &Role) {
    eprintln!("  name:      {}", role.name);
    match role.rate_rpm {
        Some(rpm) => eprintln!("  rate_rpm:  {rpm}"),
        None => eprintln!("  rate_rpm:  (class default)"),
    }
    if role.permissions.is_empty() {
        eprintln!("  perms:     (none)");
    } else {
        eprintln!("  perms:     {}", format_perms(&role.permissions));
    }
}

/// Render a permission bundle as `"app:perm, app:perm"` (already sorted by
/// the store's ORDER BY).
fn format_perms(perms: &[RolePermission]) -> String {
    perms
        .iter()
        .map(|rp| format!("{}:{}", rp.app, rp.permission))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `roles create --name <N> [--perm APP:PERMISSION]... [--rate-rpm N]`.
pub async fn create(
    store: &KeyStore,
    name: &RoleName,
    permissions: &[RolePermission],
    rate_rpm: Option<u32>,
) -> Result<(), AdminError> {
    let role = store
        .create_role(name.as_str(), rate_rpm, permissions)
        .await?;
    warn_unknown_permissions(store, permissions).await?;
    eprintln!("created role:\n");
    print_role(&role);
    Ok(())
}

/// `roles list` — every role with its bundle, one table.
pub async fn list(store: &KeyStore) -> Result<(), AdminError> {
    let roles = store.list_roles().await?;
    if roles.is_empty() {
        eprintln!("no roles found");
        return Ok(());
    }

    let table = format_roles_table(&roles);
    println!("{table}");
    eprintln!("{} role(s)", roles.len());
    Ok(())
}

/// `roles show <NAME>` — one role's full detail.
pub async fn show(store: &KeyStore, name: &RoleName) -> Result<(), AdminError> {
    let role = store.get_role(name.as_str()).await?;
    let key_count = store.count_role_assignments(name.as_str()).await?;
    print_role(&role);
    eprintln!("  keys:      {key_count}");
    Ok(())
}

/// `roles add-perm <NAME> <APP:PERMISSION>...`.
pub async fn add_perm(
    store: &KeyStore,
    name: &RoleName,
    permissions: &[RolePermission],
) -> Result<(), AdminError> {
    let role = store
        .add_role_permissions(name.as_str(), permissions)
        .await?;
    warn_unknown_permissions(store, permissions).await?;
    eprintln!("updated role:\n");
    print_role(&role);
    Ok(())
}

/// `roles set-rate <NAME> (--rate-rpm N | --default)`.
///
/// `None` clears the ceiling back to the route-class config defaults.
/// Non-destructive: the permission bundle and every key assignment survive,
/// so re-tiering a class of service needs no delete-and-recreate.
pub async fn set_rate(
    store: &KeyStore,
    name: &RoleName,
    rate_rpm: Option<u32>,
) -> Result<(), AdminError> {
    let role = store.set_role_rate_rpm(name.as_str(), rate_rpm).await?;
    eprintln!("updated role:\n");
    print_role(&role);
    Ok(())
}

/// `roles remove-perm <NAME> <APP:PERMISSION>...`.
pub async fn remove_perm(
    store: &KeyStore,
    name: &RoleName,
    permissions: &[RolePermission],
) -> Result<(), AdminError> {
    let removed = store
        .remove_role_permissions(name.as_str(), permissions)
        .await?;
    if removed < permissions.len() as u64 {
        eprintln!(
            "warning: {} of {} named permission(s) were not on the role",
            permissions.len() as u64 - removed,
            permissions.len()
        );
    }
    let role = store.get_role(name.as_str()).await?;
    eprintln!("updated role:\n");
    print_role(&role);
    Ok(())
}

/// `roles delete <NAME> [--force] [--yes]`.
///
/// Refuses while keys still hold the role unless `--force` (store-side
/// `RoleInUse` carrying the affected-key count). Destructive, so it is
/// TTY-confirmation-gated like `keys revoke` — the prompt names how many
/// keys lose the role when forcing.
pub async fn delete(
    store: &KeyStore,
    name: &RoleName,
    force: bool,
    yes: bool,
) -> Result<(), AdminError> {
    // Read the current assignment count for the prompt. The store recounts
    // under its own transaction, so this is informational only.
    let key_count = store.count_role_assignments(name.as_str()).await?;

    if !yes {
        let question = if key_count > 0 {
            format!(
                "delete role {name}? {key_count} key(s) currently hold it{}",
                if force {
                    " and will lose it"
                } else {
                    " (will refuse without --force)"
                }
            )
        } else {
            format!("delete role {name}? no keys hold it")
        };
        if !confirm_or_refuse(&question)? {
            return Ok(());
        }
    }

    store.delete_role(name.as_str(), force).await?;
    eprintln!("deleted role {name}");
    Ok(())
}

/// Render a [`Role`] slice as a `comfy_table` block.
fn format_roles_table(roles: &[Role]) -> comfy_table::Table {
    let mut table = comfy_table::Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_FULL)
        .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS)
        .set_content_arrangement(comfy_table::ContentArrangement::Dynamic);

    table.set_header(vec!["name", "rate_rpm", "permissions"]);

    for role in roles {
        table.add_row(vec![
            role.name.clone(),
            role.rate_rpm
                .map_or_else(|| "-".to_owned(), |rpm| rpm.to_string()),
            if role.permissions.is_empty() {
                "(none)".to_owned()
            } else {
                format_perms(&role.permissions)
            },
        ]);
    }

    table
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rp(app: &str, permission: &str) -> RolePermission {
        RolePermission {
            app: app.into(),
            permission: permission.into(),
        }
    }

    #[test]
    fn parse_perm_valid() {
        let p = parse_perm("trawl:query").unwrap();
        assert_eq!(p.app, "trawl");
        assert_eq!(p.permission, "query");
    }

    #[test]
    fn parse_perm_missing_colon() {
        assert!(parse_perm("trawlquery").is_err());
    }

    #[test]
    fn parse_perm_empty_halves() {
        assert!(parse_perm(":query").is_err());
        assert!(parse_perm("trawl:").is_err());
    }

    #[test]
    fn parse_perm_rejects_bad_charset() {
        assert!(parse_perm("Trawl:query").is_err());
        assert!(parse_perm("trawl:Query").is_err());
        assert!(parse_perm("trawl:que ry").is_err());
        // Permissions can't be colon-namespaced — one app half, one perm half.
        assert!(parse_perm("trawl:sub:query").is_err());
    }

    #[test]
    fn format_perms_joins_pairs() {
        assert_eq!(
            format_perms(&[rp("trawl", "query"), rp("coastwatch", "stories_read")]),
            "trawl:query, coastwatch:stories_read"
        );
    }

    #[test]
    fn snapshot_roles_table() {
        let roles = vec![
            Role {
                id: 1,
                name: "trawl-admin".into(),
                rate_rpm: None,
                permissions: vec![
                    rp("trawl", "query"),
                    rp("trawl", "schema_read"),
                    rp("trawl", "server_manage"),
                ],
            },
            Role {
                id: 2,
                name: "shipper".into(),
                rate_rpm: Some(2000),
                permissions: vec![rp("trawl", "ingest")],
            },
            Role {
                id: 3,
                name: "placeholder".into(),
                rate_rpm: None,
                permissions: vec![],
            },
        ];
        insta::assert_snapshot!(format_roles_table(&roles).to_string());
    }
}
