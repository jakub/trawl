// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;
use sqlx::postgres::PgPoolOptions;

use crate::cli::{App, DatabaseMode};
use crate::command::{CommandRunner, CommandSpec, require_success};
use crate::config::{AppManifest, MachineProfile};
use crate::credentials::{ServiceToken, database_url};
use crate::environment;
use crate::error::{Error, Result};
use crate::resolver::SecretValue;
use crate::state::ProviderIdentity;

pub const DOCKER_SCOPE: &str = "docker";
pub const DOCKER_HOST: &str = "127.0.0.1";
pub const DOCKER_PORT: u16 = 5435;
pub const DOCKER_COMPOSE_FILE: &str = "fleet-dev.compose.yml";
pub const DOCKER_PROJECT: &str = "fleet-dev";
pub const DOCKER_SERVICE: &str = "fleet-dev-postgres";
pub const DOCKER_IMAGE: &str = "pgvector/pgvector:pg18";
/// The only container port the development provider may publish.
const DOCKER_CONTRACT_PORT: &str = "5432/tcp";
pub const FLEET_DATABASE: &str = "fleet_dev";
pub const REQUIRED_DOCKER_DATABASES: [&str; 3] = [FLEET_DATABASE, "trawl_dev", "coastwatch_dev"];

#[derive(Debug)]
pub struct PreparedDatabases {
    pub mode: DatabaseMode,
    pub state_scope: String,
    pub identity: ProviderIdentity,
    pub fleet_url: SecretValue,
    pub app_urls: BTreeMap<App, SecretValue>,
    /// The controller owns lifecycle cleanup only when it started Docker.
    pub docker_started: bool,
}

#[must_use]
pub fn needs_onepassword(mode: DatabaseMode, manifests: &BTreeMap<App, AppManifest>) -> bool {
    mode == DatabaseMode::Cnpg
        || manifests
            .values()
            .any(|manifest| manifest.resolver.is_some())
}

pub async fn prepare(
    runner: &dyn CommandRunner,
    trawl_root: &Path,
    profile: &MachineProfile,
    manifests: &BTreeMap<App, AppManifest>,
    token: Option<&ServiceToken>,
    docker_owned: &AtomicBool,
) -> Result<PreparedDatabases> {
    match profile.database {
        DatabaseMode::Docker => prepare_docker(runner, trawl_root, manifests, docker_owned).await,
        DatabaseMode::Cnpg => {
            let token = token.ok_or_else(|| {
                Error::InvalidArgument(
                    "CNPG preparation requires a validated 1Password service token".to_owned(),
                )
            })?;
            prepare_cnpg(runner, profile, manifests, token).await
        }
    }
}

pub fn docker_setup(runner: &dyn CommandRunner, trawl_root: &Path) -> Result<()> {
    // Half a gigabyte of image over a home link outlasts the default deadline,
    // and a silent multi-minute pull is indistinguishable from a hang.
    let spec = compose_spec(trawl_root)
        .args(["pull"])
        .timeout(crate::command::BUILD_TIMEOUT)
        .stream_output();
    let output = runner.output(&spec)?;
    require_success(
        &spec,
        &output,
        "failed to pull the dedicated Fleet development database image",
    )
}

pub fn stop_owned_docker(runner: &dyn CommandRunner, trawl_root: &Path) -> Result<()> {
    // This is the unwind path for an interrupted preparation, so it must be
    // exempt from the interrupt latch that aborted the preparation.
    let spec = compose_spec(trawl_root)
        .args(["stop", DOCKER_SERVICE])
        .runs_after_interrupt();
    let output = runner.output(&spec)?;
    require_success(
        &spec,
        &output,
        "failed to stop the Fleet development database container",
    )
}

pub async fn validate_cnpg(
    runner: &dyn CommandRunner,
    profile: &MachineProfile,
    manifests: &BTreeMap<App, AppManifest>,
    token: &ServiceToken,
) -> Result<()> {
    prepare_cnpg(runner, profile, manifests, token)
        .await
        .map(|_| ())
}

async fn prepare_docker(
    runner: &dyn CommandRunner,
    trawl_root: &Path,
    manifests: &BTreeMap<App, AppManifest>,
    docker_owned: &AtomicBool,
) -> Result<PreparedDatabases> {
    let running_spec =
        compose_spec(trawl_root).args(["ps", "--status", "running", "--quiet", DOCKER_SERVICE]);
    let running_output = runner.output(&running_spec)?;
    require_success(
        &running_spec,
        &running_output,
        "failed to inspect the Fleet development database container",
    )?;
    let already_running = !running_output.stdout.iter().all(u8::is_ascii_whitespace);
    if !already_running {
        // Compose may create/start the container before `up --wait` returns,
        // including on a later health failure or controller timeout.
        docker_owned.store(true, Ordering::Release);
        // `up` pulls the image on a machine that never ran `setup`, so this
        // needs the same deadline and live progress as an explicit pull.
        let start_spec = compose_spec(trawl_root)
            .args(["up", "--detach", "--wait", DOCKER_SERVICE])
            .timeout(crate::command::BUILD_TIMEOUT)
            .stream_output();
        let start = runner.output(&start_spec).and_then(|output| {
            require_success(
                &start_spec,
                &output,
                "failed to start the Fleet development database container",
            )
        });
        if let Err(error) = start {
            return Err(cleanup_after_error(runner, trawl_root, docker_owned, error));
        }
    }

    if let Err(error) = validate_docker_container(runner) {
        if !already_running {
            return Err(cleanup_after_error(runner, trawl_root, docker_owned, error));
        }
        return Err(error);
    }

    let fleet_url = SecretValue::new(docker_url(FLEET_DATABASE));
    for database in REQUIRED_DOCKER_DATABASES {
        let url = docker_url(database);
        if let Err(error) = validate_database(&url, database).await {
            if !already_running {
                return Err(cleanup_after_error(runner, trawl_root, docker_owned, error));
            }
            return Err(error);
        }
    }
    if let Err(error) = validate_docker_capabilities(&docker_url(FLEET_DATABASE)).await {
        if !already_running {
            return Err(cleanup_after_error(runner, trawl_root, docker_owned, error));
        }
        return Err(error);
    }

    let app_urls = manifests
        .iter()
        .map(|(app, manifest)| (*app, SecretValue::new(docker_url(&manifest.database.name))))
        .collect();
    Ok(PreparedDatabases {
        mode: DatabaseMode::Docker,
        state_scope: DOCKER_SCOPE.to_owned(),
        identity: ProviderIdentity::new("docker", DOCKER_HOST, DOCKER_PORT),
        fleet_url,
        app_urls,
        docker_started: !already_running,
    })
}

fn cleanup_after_error(
    runner: &dyn CommandRunner,
    trawl_root: &Path,
    docker_owned: &AtomicBool,
    original: Error,
) -> Error {
    match stop_owned_docker(runner, trawl_root) {
        Ok(()) => {
            docker_owned.store(false, Ordering::Release);
            original
        }
        Err(cleanup) => Error::InvalidArgument(format!(
            "{original}; additionally failed to stop the controller-owned Docker container: {cleanup}"
        )),
    }
}

async fn prepare_cnpg(
    runner: &dyn CommandRunner,
    profile: &MachineProfile,
    manifests: &BTreeMap<App, AppManifest>,
    token: &ServiceToken,
) -> Result<PreparedDatabases> {
    let cnpg = profile.cnpg.as_ref().ok_or_else(|| {
        Error::InvalidArgument("CNPG mode requires a validated [cnpg] profile".to_owned())
    })?;
    let fleet_url = database_url(runner, token, cnpg, FLEET_DATABASE)?;
    validate_database(fleet_url.expose(), FLEET_DATABASE).await?;

    let mut app_urls = BTreeMap::new();
    for (app, manifest) in manifests {
        let url = database_url(runner, token, cnpg, &manifest.database.name)?;
        validate_database(url.expose(), &manifest.database.name).await?;
        if *app == App::Coastwatch {
            tokio::time::timeout(
                Duration::from_secs(15),
                validate_extensions(
                    url.expose(),
                    &manifest.database.name,
                    &["vector", "pg_trgm"],
                ),
            )
            .await
            .map_err(|_| {
                Error::InvalidArgument(format!(
                    "timed out validating extensions for {:?}",
                    manifest.database.name
                ))
            })??;
        }
        app_urls.insert(*app, url);
    }
    Ok(PreparedDatabases {
        mode: DatabaseMode::Cnpg,
        state_scope: cnpg.state_scope.clone(),
        identity: ProviderIdentity::new("cnpg", &cnpg.host, cnpg.port),
        fleet_url,
        app_urls,
        docker_started: false,
    })
}

async fn validate_database(url: &str, expected_database: &str) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(15),
        validate_database_inner(url, expected_database),
    )
    .await
    .map_err(|_| {
        Error::InvalidArgument(format!(
            "timed out validating database {expected_database:?}"
        ))
    })?
}

async fn validate_database_inner(url: &str, expected_database: &str) -> Result<()> {
    // validate_database_inner is a short-lived, single-connection
    // validation pool that closes before returning (ADR-0021 ruling 3).
    #[allow(clippy::disallowed_methods)]
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(url)
        .await?;
    let actual: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await?;
    let can_migrate: bool = sqlx::query_scalar(
        "SELECT has_schema_privilege(current_user, 'public', 'USAGE')
             AND has_schema_privilege(current_user, 'public', 'CREATE')",
    )
    .fetch_one(&pool)
    .await?;
    pool.close().await;
    if actual != expected_database {
        Err(Error::InvalidArgument(format!(
            "database provider connected to {actual:?}, expected {expected_database:?}"
        )))
    } else if can_migrate {
        Ok(())
    } else {
        Err(Error::InvalidArgument(format!(
            "database role lacks USAGE/CREATE migration rights on public schema in {expected_database:?}"
        )))
    }
}

async fn validate_extensions(url: &str, database: &str, required: &[&str]) -> Result<()> {
    // validate_extensions is a short-lived, single-connection
    // validation pool that closes before returning (ADR-0021 ruling 3).
    #[allow(clippy::disallowed_methods)]
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(url)
        .await?;
    let installed: Vec<String> =
        sqlx::query_scalar("SELECT extname FROM pg_extension WHERE extname = ANY($1)")
            .bind(required)
            .fetch_all(&pool)
            .await?;
    pool.close().await;
    let missing: Vec<_> = required
        .iter()
        .filter(|name| !installed.iter().any(|value| value == **name))
        .copied()
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Error::InvalidArgument(format!(
            "CNPG database {database:?} is missing pre-provisioned extensions: {}",
            missing.join(", ")
        )))
    }
}

async fn validate_docker_capabilities(url: &str) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(15),
        validate_docker_capabilities_inner(url),
    )
    .await
    .map_err(|_| {
        Error::InvalidArgument("timed out validating Docker database capabilities".to_owned())
    })?
}

async fn validate_docker_capabilities_inner(url: &str) -> Result<()> {
    // validate_docker_capabilities_inner is a short-lived,
    // single-connection validation pool that closes before returning
    // (ADR-0021 ruling 3).
    #[allow(clippy::disallowed_methods)]
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(url)
        .await?;
    let superuser: bool =
        sqlx::query_scalar("SELECT rolsuper FROM pg_roles WHERE rolname = current_user")
            .fetch_one(&pool)
            .await?;
    let extensions: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM pg_available_extensions
         WHERE name IN ('vector', 'pg_trgm')
         ORDER BY name",
    )
    .fetch_all(&pool)
    .await?;
    pool.close().await;
    if !superuser {
        return Err(Error::InvalidArgument(
            "Docker fleet_dev role is not the required loopback-only bootstrap superuser"
                .to_owned(),
        ));
    }
    if extensions != ["pg_trgm", "vector"] {
        return Err(Error::InvalidArgument(format!(
            "Docker image is missing required extensions; found {extensions:?}"
        )));
    }
    Ok(())
}

fn validate_docker_container(runner: &dyn CommandRunner) -> Result<()> {
    let spec = CommandSpec::new("docker")
        .args(["inspect", "--format", "{{json .}}", DOCKER_SERVICE])
        .environment(environment::sanitized_base())
        .report_stderr();
    let output = runner.output(&spec)?;
    require_success(
        &spec,
        &output,
        "failed to inspect the Fleet development database container",
    )?;
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|source| {
        Error::InvalidArgument(format!(
            "docker inspect returned an invalid container document: {source}"
        ))
    })?;
    let image = value.pointer("/Config/Image").and_then(Value::as_str);
    // The container may predate this controller, so inspect the whole
    // publishing surface: every binding on the contracted port must be
    // loopback, and no other container port may be published at all. Checking
    // only the first binding would accept a container that also published the
    // same port on a wildcard address.
    let expected_port = DOCKER_PORT.to_string();
    let contract_ok = value
        .pointer("/HostConfig/PortBindings")
        .and_then(Value::as_object)
        .is_some_and(|ports| {
            ports
                .iter()
                .all(|(port, bindings)| port == DOCKER_CONTRACT_PORT || is_unpublished(bindings))
                && ports
                    .get(DOCKER_CONTRACT_PORT)
                    .and_then(Value::as_array)
                    .is_some_and(|bindings| {
                        !bindings.is_empty()
                            && bindings.iter().all(|entry| {
                                entry.get("HostIp").and_then(Value::as_str) == Some(DOCKER_HOST)
                                    && entry.get("HostPort").and_then(Value::as_str)
                                        == Some(expected_port.as_str())
                            })
                    })
        });
    if image != Some(DOCKER_IMAGE) || !contract_ok {
        return Err(Error::InvalidArgument(format!(
            "container {DOCKER_SERVICE} does not match the fixed image/loopback-port contract"
        )));
    }
    Ok(())
}

/// Docker records an exposed-but-unpublished port as `null` or `[]`.
fn is_unpublished(bindings: &Value) -> bool {
    bindings.is_null() || bindings.as_array().is_some_and(Vec::is_empty)
}

fn compose_spec(trawl_root: &Path) -> CommandSpec {
    CommandSpec::new("docker")
        .args([
            OsString::from("compose"),
            OsString::from("--project-name"),
            OsString::from(DOCKER_PROJECT),
            OsString::from("--file"),
            trawl_root.join(DOCKER_COMPOSE_FILE).into_os_string(),
        ])
        .cwd(trawl_root)
        .environment(environment::sanitized_base())
        .report_stderr()
}

fn docker_url(database: &str) -> String {
    let mut url = url::Url::parse("postgres://127.0.0.1").expect("static URL is valid");
    url.set_port(Some(DOCKER_PORT)).expect("valid Docker port");
    url.set_username("fleet_dev")
        .expect("static Docker username is valid");
    url.set_password(Some("fleet_dev"))
        .expect("static Docker password is valid");
    url.set_path(database);
    url.into()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    #[derive(Debug)]
    struct OneOutput {
        stdout: Vec<u8>,
        seen: std::sync::Mutex<Vec<CommandSpec>>,
    }

    impl CommandRunner for OneOutput {
        fn output(&self, spec: &CommandSpec) -> Result<std::process::Output> {
            self.seen.lock().unwrap().push(spec.clone());
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: self.stdout.clone(),
                stderr: Vec::new(),
            })
        }
    }

    #[derive(Debug)]
    struct SequenceRunner {
        outputs: std::sync::Mutex<VecDeque<std::process::Output>>,
        seen: std::sync::Mutex<Vec<CommandSpec>>,
    }

    impl SequenceRunner {
        fn successful(outputs: impl IntoIterator<Item = &'static [u8]>) -> Self {
            Self {
                outputs: std::sync::Mutex::new(
                    outputs
                        .into_iter()
                        .map(|stdout| std::process::Output {
                            status: std::process::ExitStatus::from_raw(0),
                            stdout: stdout.to_vec(),
                            stderr: Vec::new(),
                        })
                        .collect(),
                ),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for SequenceRunner {
        fn output(&self, spec: &CommandSpec) -> Result<std::process::Output> {
            self.seen.lock().unwrap().push(spec.clone());
            Ok(self.outputs.lock().unwrap().pop_front().unwrap())
        }
    }

    #[test]
    fn docker_urls_follow_fixed_contract() {
        assert_eq!(
            docker_url("trawl_dev"),
            "postgres://fleet_dev:fleet_dev@127.0.0.1:5435/trawl_dev"
        );
    }

    #[test]
    fn onepassword_is_demand_driven() {
        let manifests = BTreeMap::new();
        assert!(!needs_onepassword(DatabaseMode::Docker, &manifests));
        assert!(needs_onepassword(DatabaseMode::Cnpg, &manifests));
    }

    #[test]
    fn docker_inspection_enforces_image_and_loopback_binding() {
        let valid = OneOutput {
            stdout: br#"{
                "Config":{"Image":"pgvector/pgvector:pg18"},
                "HostConfig":{"PortBindings":{"5432/tcp":[
                    {"HostIp":"127.0.0.1","HostPort":"5435"}
                ]}}
            }"#
            .to_vec(),
            seen: std::sync::Mutex::new(Vec::new()),
        };
        validate_docker_container(&valid).unwrap();
        let seen = valid.seen.lock().unwrap();
        assert_eq!(seen[0].program, "docker");
        assert!(
            !seen[0]
                .environment
                .contains_key(std::ffi::OsStr::new("OP_SERVICE_ACCOUNT_TOKEN"))
        );

        let exposed = OneOutput {
            stdout: br#"{
                "Config":{"Image":"pgvector/pgvector:pg18"},
                "HostConfig":{"PortBindings":{"5432/tcp":[
                    {"HostIp":"0.0.0.0","HostPort":"5435"}
                ]}}
            }"#
            .to_vec(),
            seen: std::sync::Mutex::new(Vec::new()),
        };
        assert!(validate_docker_container(&exposed).is_err());
    }

    #[test]
    fn a_second_binding_or_an_extra_published_port_is_rejected() {
        for document in [
            // Loopback first, wildcard second: the whole publishing surface
            // has to be inspected, not just the first entry.
            br#"{"Config":{"Image":"pgvector/pgvector:pg18"},
                 "HostConfig":{"PortBindings":{"5432/tcp":[
                     {"HostIp":"127.0.0.1","HostPort":"5435"},
                     {"HostIp":"0.0.0.0","HostPort":"5435"}]}}}"#
                .as_slice(),
            // Contracted port is fine, but a second port is published.
            br#"{"Config":{"Image":"pgvector/pgvector:pg18"},
                 "HostConfig":{"PortBindings":{
                     "5432/tcp":[{"HostIp":"127.0.0.1","HostPort":"5435"}],
                     "9999/tcp":[{"HostIp":"0.0.0.0","HostPort":"9999"}]}}}"#
                .as_slice(),
            // The contracted port is not published at all.
            br#"{"Config":{"Image":"pgvector/pgvector:pg18"},
                 "HostConfig":{"PortBindings":{}}}"#
                .as_slice(),
        ] {
            let runner = OneOutput {
                stdout: document.to_vec(),
                seen: std::sync::Mutex::new(Vec::new()),
            };
            assert!(
                validate_docker_container(&runner).is_err(),
                "{}",
                String::from_utf8_lossy(document)
            );
        }
    }

    #[test]
    fn exposed_but_unpublished_sibling_ports_are_tolerated() {
        // `EXPOSE` without a host publish is not reachable, so it must not
        // fail an otherwise contract-conforming container.
        let runner = OneOutput {
            stdout: br#"{"Config":{"Image":"pgvector/pgvector:pg18"},
                 "HostConfig":{"PortBindings":{
                     "5432/tcp":[{"HostIp":"127.0.0.1","HostPort":"5435"}],
                     "9999/tcp":null}}}"#
                .to_vec(),
            seen: std::sync::Mutex::new(Vec::new()),
        };
        validate_docker_container(&runner).unwrap();
    }

    #[tokio::test]
    async fn owned_docker_is_stopped_when_validation_fails_without_fallback() {
        let runner = SequenceRunner::successful([
            b"".as_slice(),
            b"".as_slice(),
            b"not-json".as_slice(),
            b"".as_slice(),
        ]);
        let owned = AtomicBool::new(false);
        let error = prepare_docker(
            &runner,
            Path::new("/trawl"),
            &BTreeMap::<App, AppManifest>::new(),
            &owned,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("invalid container document"));
        assert!(!owned.load(Ordering::Acquire));
        let seen = runner.seen.lock().unwrap();
        let args = seen
            .iter()
            .map(|spec| {
                spec.args
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert!(args[0].ends_with(&["--quiet".to_owned(), DOCKER_SERVICE.to_owned()]));
        assert!(args[1].ends_with(&[
            "--detach".to_owned(),
            "--wait".to_owned(),
            DOCKER_SERVICE.to_owned()
        ]));
        assert_eq!(seen[2].program, "docker");
        assert!(args[3].ends_with(&["stop".to_owned(), DOCKER_SERVICE.to_owned()]));
        assert!(seen.iter().all(|spec| spec.program != "op"));
    }

    #[tokio::test]
    async fn cnpg_connection_failure_never_invokes_docker() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let token_path = directory.path().join("token");
        std::fs::write(&token_path, "service-account-token").unwrap();
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let token = crate::credentials::load_service_token(&crate::config::OpProfile {
            token_file: Some(token_path),
        })
        .unwrap();
        let runner =
            SequenceRunner::successful([b"fleet_user\n".as_slice(), b"password\n".as_slice()]);
        let profile = MachineProfile {
            database: DatabaseMode::Cnpg,
            cnpg: Some(crate::config::CnpgProfile {
                host: "127.0.0.1".to_owned(),
                port: 1,
                state_scope: "cnpg-test".to_owned(),
                credential_ref_template: "op://Test/{database}/{field}".to_owned(),
                session_aead_key_ref: "op://Test/session/key".to_owned(),
            }),
            ..MachineProfile::default()
        };
        let owned = AtomicBool::new(false);
        assert!(
            prepare(
                &runner,
                Path::new("/trawl"),
                &profile,
                &BTreeMap::new(),
                Some(&token),
                &owned,
            )
            .await
            .is_err()
        );
        let seen = runner.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().all(|spec| spec.program == "op"));
        assert!(!owned.load(Ordering::Acquire));
    }
}
