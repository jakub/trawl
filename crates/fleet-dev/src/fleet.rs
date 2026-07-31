// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use fleet_auth::{AuthError, KeyStore, PrincipalKind, RolePermission};
use sqlx::postgres::PgPoolOptions;

use crate::cli::App;
use crate::command::{CommandRunner, CommandSpec, require_success};
use crate::config::AppManifest;
use crate::environment;
use crate::error::{Error, Result};
use crate::resolver::SecretValue;
use crate::state::ScopedState;

pub const DEV_ROLE: &str = "fleet-developer";

#[derive(Debug)]
pub struct DeveloperKey {
    pub token: SecretValue,
    pub prefix: String,
}

pub fn migrate(
    runner: &dyn CommandRunner,
    trawl_root: &Path,
    fleet_database_url: &SecretValue,
) -> Result<()> {
    let build_spec = CommandSpec::new("cargo")
        .args(["build", "--quiet", "-p", "fleet-admin"])
        .cwd(trawl_root)
        .environment(environment::sanitized_base());
    let output = runner.output(&build_spec)?;
    require_success(
        &build_spec,
        &output,
        "failed to build fleet-admin before migration",
    )?;

    let mut environment = environment::sanitized_base();
    environment.insert(
        OsString::from("DATABASE_URL"),
        OsString::from(fleet_database_url.expose()),
    );
    let spec = CommandSpec::new(trawl_root.join("target/debug/fleet-admin").into_os_string())
        .args(["migrate"])
        .cwd(trawl_root)
        .environment(environment);
    let output = runner.output(&spec)?;
    require_success(
        &spec,
        &output,
        "fleet-admin migrate failed against the selected Fleet database",
    )
}

pub async fn reconcile(
    fleet_database_url: &SecretValue,
    registered_manifests: &BTreeMap<App, AppManifest>,
    state: &ScopedState,
) -> Result<DeveloperKey> {
    tokio::time::timeout(
        Duration::from_secs(30),
        reconcile_inner(fleet_database_url, registered_manifests, state),
    )
    .await
    .map_err(|_| {
        Error::InvalidArgument("timed out reconciling the Fleet development identity".to_owned())
    })?
}

async fn reconcile_inner(
    fleet_database_url: &SecretValue,
    registered_manifests: &BTreeMap<App, AppManifest>,
    state: &ScopedState,
) -> Result<DeveloperKey> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect(fleet_database_url.expose())
        .await?;
    let store = KeyStore::from_pool(pool.clone());
    store.ping().await?;

    let permissions = permission_union(registered_manifests)?;
    for permission in &permissions {
        if !store
            .is_known_permission(&permission.app, &permission.permission)
            .await?
        {
            // The registry is deliberately warn-only: a companion app may
            // declare the permission before its deployed binary has
            // registered the vocabulary. Manifest validation still pins the
            // app namespace and frozen permission lists.
            eprintln!(
                "fleet-dev: warning: manifest permission {}:{} is not registered in the selected Fleet database",
                permission.app, permission.permission
            );
        }
    }
    match store.get_role(DEV_ROLE).await {
        Ok(role) => {
            let existing: BTreeSet<_> = role.permissions.into_iter().collect();
            let missing: Vec<_> = permissions
                .iter()
                .filter(|permission| !existing.contains(*permission))
                .cloned()
                .collect();
            if !missing.is_empty() {
                store.add_role_permissions(DEV_ROLE, &missing).await?;
            }
        }
        Err(AuthError::RoleNotFound { .. }) => {
            store.create_role(DEV_ROLE, None, &permissions).await?;
        }
        Err(error) => return Err(error.into()),
    }

    let cached = state.read_api_key()?;
    let key = if let Some(token) = cached {
        match store.verify_key(&token).await {
            Ok(mut verified) => {
                let roles = verified.roles().to_owned();
                if roles.iter().any(|role| role != DEV_ROLE) {
                    return Err(Error::InvalidArgument(format!(
                        "cached development key {} has unexpected additional roles; revoke it and remove {}",
                        verified.prefix,
                        state.api_key_path().display()
                    )));
                }
                if verified.kind != PrincipalKind::Human {
                    store
                        .retype_key(&verified.prefix, PrincipalKind::Human)
                        .await?;
                    verified = store.verify_key(&token).await?;
                }
                if !roles.iter().any(|role| role == DEV_ROLE) {
                    store.assign_role(&verified.prefix, DEV_ROLE).await?;
                    verified = store.verify_key(&token).await?;
                }
                ensure_permissions(&verified, &permissions)?;
                DeveloperKey {
                    token: SecretValue::new(token),
                    prefix: verified.prefix,
                }
            }
            Err(AuthError::InvalidKey(_) | AuthError::MalformedToken(_)) => {
                mint_key(&store, state).await?
            }
            Err(error) => return Err(error.into()),
        }
    } else {
        mint_key(&store, state).await?
    };
    pool.close().await;
    Ok(key)
}

fn permission_union(manifests: &BTreeMap<App, AppManifest>) -> Result<Vec<RolePermission>> {
    let mut permissions = BTreeSet::new();
    for manifest in manifests.values() {
        for spec in &manifest.auth.permissions {
            let (app, permission) = spec.split_once(':').ok_or_else(|| {
                Error::InvalidArgument(format!("manifest permission {spec:?} is malformed"))
            })?;
            permissions.insert(RolePermission {
                app: app.to_owned(),
                permission: permission.to_owned(),
            });
        }
    }
    Ok(permissions.into_iter().collect())
}

async fn mint_key(store: &KeyStore, state: &ScopedState) -> Result<DeveloperKey> {
    let user = std::env::var("USER").unwrap_or_else(|_| "developer".to_owned());
    let created = store
        .create_key(
            &format!("fleet-dev-{user}"),
            PrincipalKind::Human,
            &[DEV_ROLE.to_owned()],
            None,
        )
        .await?;
    state.write_api_key(&created.plaintext_token)?;
    Ok(DeveloperKey {
        token: SecretValue::new(created.plaintext_token.to_string()),
        prefix: created.info.prefix,
    })
}

fn ensure_permissions(
    verified: &fleet_auth::VerifiedKey,
    required: &[RolePermission],
) -> Result<()> {
    for permission in required {
        if !verified.has_app_permission(&permission.app, &permission.permission) {
            return Err(Error::InvalidArgument(format!(
                "development key {} is missing reconciled permission {}:{}",
                verified.prefix, permission.app, permission.permission
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::sync::Mutex;

    use super::*;
    use crate::command::CommandRunner;
    use crate::config::{
        AuthManifest, ConsumerEnvironment, DatabaseManifest, MigrationMode, WebManifest,
    };

    fn manifest(app: App, permissions: &[&str]) -> AppManifest {
        AppManifest {
            schema: 1,
            name: app,
            web: WebManifest {
                local_port: 8081,
                tailscale_port: 8444,
                backend_port: 8090,
                login_path: "/login".to_owned(),
            },
            database: DatabaseManifest {
                name: format!("{}_dev", app.as_str()),
                migration_mode: MigrationMode::ApplicationStartup,
                migration_command: Vec::new(),
            },
            resolver: None,
            preparation: None,
            auth: AuthManifest {
                permissions: permissions.iter().map(ToString::to_string).collect(),
            },
            processes: vec![],
            migration: ConsumerEnvironment::default(),
        }
    }

    #[test]
    fn permission_union_is_sorted_and_deduplicated() {
        let manifests = BTreeMap::from([
            (
                App::Trawl,
                manifest(App::Trawl, &["trawl:query", "trawl:query"]),
            ),
            (
                App::Coastwatch,
                manifest(App::Coastwatch, &["coastwatch:stories_read"]),
            ),
        ]);
        let permissions = permission_union(&manifests).unwrap();
        assert_eq!(permissions.len(), 2);
        assert_eq!(permissions[0].app, "coastwatch");
        assert_eq!(permissions[1].app, "trawl");
    }

    #[derive(Debug, Default)]
    struct RecordingRunner(Mutex<Vec<CommandSpec>>);

    impl CommandRunner for RecordingRunner {
        fn output(&self, spec: &CommandSpec) -> Result<std::process::Output> {
            self.0.lock().unwrap().push(spec.clone());
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    #[test]
    fn fleet_migration_builds_without_the_dsn_then_runs_the_binary_with_it() {
        let runner = RecordingRunner::default();
        migrate(
            &runner,
            Path::new("/checkout"),
            &SecretValue::new("postgres://secret".to_owned()),
        )
        .unwrap();
        let seen = runner.0.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].program, "cargo");
        assert!(
            !seen[0]
                .environment
                .contains_key(std::ffi::OsStr::new("DATABASE_URL"))
        );
        assert_eq!(
            seen[1].program,
            std::ffi::OsStr::new("/checkout/target/debug/fleet-admin")
        );
        assert_eq!(
            seen[1]
                .environment
                .get(std::ffi::OsStr::new("DATABASE_URL")),
            Some(&std::ffi::OsString::from("postgres://secret"))
        );
    }
}
