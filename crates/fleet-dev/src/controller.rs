// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::cli::{App, Cli, Command, Exposure, PlanFormat};
use crate::command::{CommandRunner, CommandSpec, require_success};
use crate::config::{AppManifest, CommandManifest, MachineProfile, load_profile};
use crate::credentials::{
    ResolverInvocation, ServiceToken, load_service_token, run_app_resolver, validate_service_token,
};
use crate::database::{self, PreparedDatabases};
use crate::environment;
use crate::error::{Error, Result};
use crate::fleet::{self, DeveloperKey};
use crate::plan::{DevelopmentPlan, build as build_plan, render as render_plan};
use crate::runtime::RuntimeValues;
use crate::selection::{AppSelection, discover};
use crate::state::{GlobalLock, ScopedState, process_state_root};
use crate::topology::TailscaleNode;

#[derive(Debug)]
struct PreparedLaunch {
    values: RuntimeValues,
    developer_key: DeveloperKey,
    state: ScopedState,
    docker_started: bool,
}

pub async fn run(cli: Cli, runner: &dyn CommandRunner) -> Result<i32> {
    let (profile, profile_path) = load_profile(cli.config.as_deref())?;
    let profile = profile.with_overrides(cli.exposure, cli.database);
    profile.validate(&profile_path)?;
    let selection = discover(&cli, &profile)?;
    let tailscale = if profile.exposure == Exposure::Tailscale {
        Some(crate::tailscale::discover(runner, &profile.tailscale)?)
    } else {
        None
    };
    let plan = build_plan(&profile, &selection, tailscale.as_ref())?;

    match &cli.command {
        Command::Plan { format, .. } => {
            println!("{}", render_plan(&plan, *format));
            Ok(0)
        }
        Command::Doctor { .. } => {
            doctor(runner, &profile, &selection, tailscale.as_ref(), &plan).await?;
            Ok(0)
        }
        Command::Setup { force, .. } => {
            let state_root = process_state_root()?;
            let _lock = GlobalLock::acquire(&state_root)?;
            if let Some(node) = tailscale.as_ref() {
                crate::tailscale::setup(runner, node, &selected_manifests(&selection), *force)?;
            }
            if profile.database == crate::cli::DatabaseMode::Docker {
                database::docker_setup(runner, &selection.trawl_root)?;
            }
            let prepared = prepare_launch(runner, &profile, &selection, &state_root).await;
            finish_without_supervisor(runner, &selection, prepared)?;
            println!("fleet-dev: setup complete");
            Ok(0)
        }
        Command::Dev(_) | Command::All(_) => {
            let state_root = process_state_root()?;
            let _lock = GlobalLock::acquire(&state_root)?;
            if let Some(node) = tailscale.as_ref() {
                crate::tailscale::verify(runner, node, &selected_manifests(&selection))?;
                warn_tailnet_backend_exposure(&plan);
            }
            let prepared = prepare_launch(runner, &profile, &selection, &state_root).await;
            run_attached(runner, &selection, &plan, prepared).await
        }
    }
}

async fn doctor(
    runner: &dyn CommandRunner,
    profile: &MachineProfile,
    selection: &AppSelection,
    tailscale: Option<&TailscaleNode>,
    plan: &DevelopmentPlan,
) -> Result<()> {
    for program in ["cargo", "mprocs"] {
        check_program(runner, program)?;
    }
    if selection.selected.values().any(|checkout| {
        checkout.manifest.processes.iter().any(|process| {
            process
                .command
                .first()
                .is_some_and(|program| program == "trunk")
        })
    }) {
        check_program(runner, "trunk")?;
    }
    if profile.database == crate::cli::DatabaseMode::Docker {
        check_program(runner, "docker")?;
        let spec = CommandSpec::new("docker")
            .args([
                OsString::from("compose"),
                OsString::from("--project-name"),
                OsString::from(database::DOCKER_PROJECT),
                OsString::from("--file"),
                selection
                    .trawl_root
                    .join(database::DOCKER_COMPOSE_FILE)
                    .into_os_string(),
                OsString::from("config"),
                OsString::from("--quiet"),
            ])
            .cwd(&selection.trawl_root)
            .environment(environment::sanitized_base());
        let output = runner.output(&spec)?;
        require_success(
            &spec,
            &output,
            "the dedicated Docker Compose configuration is invalid",
        )?;
    }
    let manifests = selected_manifests(selection);
    let token = if database::needs_onepassword(profile.database, &manifests) {
        check_program(runner, "op")?;
        let token = load_service_token(&profile.op)?;
        validate_service_token(runner, &token)?;
        Some(token)
    } else {
        None
    };
    if profile.database == crate::cli::DatabaseMode::Cnpg {
        database::validate_cnpg(
            runner,
            profile,
            &manifests,
            token.as_ref().expect("CNPG requires one password"),
        )
        .await?;
    }
    if let Some(node) = tailscale {
        crate::tailscale::verify(runner, node, &manifests)?;
    }
    println!("{}", render_plan(plan, PlanFormat::Human));
    println!("Doctor checks passed.");
    Ok(())
}

async fn prepare_launch(
    runner: &dyn CommandRunner,
    profile: &MachineProfile,
    selection: &AppSelection,
    state_root: &Path,
) -> Result<PreparedLaunch> {
    // Install the OS handlers before the first subprocess. `signal()` registers
    // synchronously, so an interrupt arriving during a blocking `cargo build`
    // is queued rather than killing the controller outright and orphaning a
    // container we just started.
    let mut signals = InterruptSignals::install()?;
    let flag = runner.interrupt_flag();
    // A spawned task, not a `select!` arm: preparation blocks its worker thread
    // inside `CommandRunner::output`, so a co-selected branch would never be
    // polled while a child is in flight.
    let watcher = tokio::spawn(async move {
        signals.recv().await;
        eprintln!("fleet-dev: interrupt received; stopping development preparation");
        if let Some(flag) = flag {
            flag.store(true, Ordering::Release);
        }
    });
    let docker_owned = AtomicBool::new(false);
    // An aborted child surfaces as `Error::CommandLimit`, so the ordinary error
    // path below performs the owned-Docker cleanup.
    let result = prepare_launch_inner(runner, profile, selection, state_root, &docker_owned).await;
    watcher.abort();
    result
}

async fn prepare_launch_inner(
    runner: &dyn CommandRunner,
    profile: &MachineProfile,
    selection: &AppSelection,
    state_root: &Path,
    docker_owned: &AtomicBool,
) -> Result<PreparedLaunch> {
    let selected = selected_manifests(selection);
    let token = if database::needs_onepassword(profile.database, &selected) {
        let token = load_service_token(&profile.op)?;
        validate_service_token(runner, &token)?;
        Some(token)
    } else {
        None
    };

    let databases = database::prepare(
        runner,
        &selection.trawl_root,
        profile,
        &selected,
        token.as_ref(),
        docker_owned,
    )
    .await?;
    let docker_started = databases.docker_started;
    match prepare_after_database(
        runner,
        profile,
        selection,
        state_root,
        token.as_ref(),
        databases,
    )
    .await
    {
        Ok(mut prepared) => {
            prepared.docker_started = docker_started;
            Ok(prepared)
        }
        Err(error) => {
            if docker_started {
                let _ = database::stop_owned_docker(runner, &selection.trawl_root);
                docker_owned.store(false, Ordering::Release);
            }
            Err(error)
        }
    }
}

/// Termination signals, with their OS handlers installed at construction.
#[cfg(unix)]
struct InterruptSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
    quit: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl InterruptSignals {
    fn install() -> Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
            quit: signal(SignalKind::quit())?,
        })
    }

    async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
            _ = self.hangup.recv() => {}
            _ = self.quit.recv() => {}
        }
    }
}

#[cfg(not(unix))]
struct InterruptSignals;

#[cfg(not(unix))]
impl InterruptSignals {
    const fn install() -> Result<Self> {
        Ok(Self)
    }

    async fn recv(&mut self) {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn prepare_after_database(
    runner: &dyn CommandRunner,
    profile: &MachineProfile,
    selection: &AppSelection,
    state_root: &Path,
    token: Option<&ServiceToken>,
    databases: PreparedDatabases,
) -> Result<PreparedLaunch> {
    let state = ScopedState::open(state_root, &databases.state_scope, &databases.identity)?;
    fleet::migrate(runner, &selection.trawl_root, &databases.fleet_url)?;

    let session_key =
        crate::session_key::resolve(runner, &selection.trawl_root, profile, &state, token)?;
    let mut values = RuntimeValues {
        fleet: BTreeMap::from([
            ("database_url".to_owned(), databases.fleet_url),
            ("session_aead_key".to_owned(), session_key),
        ]),
        apps: databases
            .app_urls
            .into_iter()
            .map(|(app, url)| (app, BTreeMap::from([("database_url".to_owned(), url)])))
            .collect(),
    };

    // Preparation runs before the resolvers so a manifest can build its
    // resolver (and migration) binaries first — handing the 1Password service
    // token to a `cargo run` resolver would expose it to every build script
    // and proc-macro in the dependency graph. Manifest validation restricts
    // preparation's resolved sources to `fleet.*`, which all exist here.
    run_app_preparations(runner, selection, &values)?;

    for (app, checkout) in &selection.selected {
        if let Some(resolver) = &checkout.manifest.resolver {
            let token = token.ok_or_else(|| {
                Error::InvalidArgument(format!(
                    "resolver for {app} requires a validated service token"
                ))
            })?;
            let pre_resolved = resolver
                .resolved_env
                .iter()
                .map(|(destination, source)| {
                    Ok((destination.clone(), values.resolve(*app, source)?))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            let resolved = run_app_resolver(
                runner,
                &ResolverInvocation {
                    app: app.as_str(),
                    root: &checkout.root,
                    command: &resolver.command,
                    cwd: resolver.cwd.as_deref(),
                    static_environment: &resolver.env,
                    pre_resolved: &pre_resolved,
                    token,
                },
            )?;
            let app_values = values.apps.entry(*app).or_default();
            for (qualified, value) in resolved {
                let leaf = qualified
                    .strip_prefix("app.")
                    .expect("resolver parser namespaces app values");
                if app_values.insert(leaf.to_owned(), value).is_some() {
                    return Err(Error::InvalidArgument(format!(
                        "resolver for {app} attempted to overwrite reserved app value {leaf:?}"
                    )));
                }
            }
        }
    }

    run_app_migrations(runner, selection, &values)?;
    let registered = selection
        .registered
        .iter()
        .map(|(app, checkout)| (*app, checkout.manifest.clone()))
        .collect();
    let developer_key =
        fleet::reconcile(&values.fleet["database_url"], &registered, &state).await?;
    Ok(PreparedLaunch {
        values,
        developer_key,
        state,
        docker_started: false,
    })
}

fn run_app_preparations(
    runner: &dyn CommandRunner,
    selection: &AppSelection,
    values: &RuntimeValues,
) -> Result<()> {
    for (app, checkout) in &selection.selected {
        if let Some(preparation) = &checkout.manifest.preparation {
            run_command_manifest(
                runner,
                *app,
                &checkout.root,
                preparation,
                values,
                "preparation",
            )?;
        }
    }
    Ok(())
}

fn run_app_migrations(
    runner: &dyn CommandRunner,
    selection: &AppSelection,
    values: &RuntimeValues,
) -> Result<()> {
    for (app, checkout) in &selection.selected {
        if checkout.manifest.database.migration_mode == crate::config::MigrationMode::Command {
            let command = CommandManifest {
                command: checkout.manifest.database.migration_command.clone(),
                cwd: None,
                env: checkout.manifest.migration.env.clone(),
                resolved_env: checkout.manifest.migration.resolved_env.clone(),
            };
            run_command_manifest(runner, *app, &checkout.root, &command, values, "migration")?;
        }
    }
    Ok(())
}

fn run_command_manifest(
    runner: &dyn CommandRunner,
    app: App,
    root: &Path,
    command: &CommandManifest,
    values: &RuntimeValues,
    kind: &str,
) -> Result<()> {
    let (program, args) = command
        .command
        .split_first()
        .expect("validated command is non-empty");
    let mut child_environment = environment::sanitized_base();
    child_environment.extend(
        command
            .env
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value))),
    );
    for (destination, source) in &command.resolved_env {
        child_environment.insert(
            OsString::from(destination),
            OsString::from(values.resolve(app, source)?.expose()),
        );
    }
    let cwd = command
        .cwd
        .as_ref()
        .map_or_else(|| root.to_owned(), |relative| root.join(relative));
    let spec = CommandSpec::new(program)
        .args(args)
        .cwd(cwd)
        .environment(child_environment)
        // Preparation compiles the app and migration runs its schema tool.
        // Both need a build-shaped deadline, and both are useless without
        // their own diagnostics, so they stream to the developer's terminal.
        .timeout(crate::command::BUILD_TIMEOUT)
        .stream_output();
    let output = runner.output(&spec)?;
    require_success(&spec, &output, format!("{app} {kind} command failed"))
}

async fn run_attached(
    runner: &dyn CommandRunner,
    selection: &AppSelection,
    plan: &DevelopmentPlan,
    prepared: Result<PreparedLaunch>,
) -> Result<i32> {
    let prepared = prepared?;
    let runtime = crate::runtime::render(
        plan,
        &prepared.values,
        &prepared.developer_key,
        &prepared.state.api_key_path(),
    );
    let result = match runtime {
        Ok(runtime) => crate::runtime::run_mprocs(&runtime).await.map(exit_code),
        Err(error) => Err(error),
    };
    if prepared.docker_started {
        let cleanup = database::stop_owned_docker(runner, &selection.trawl_root);
        if let Err(cleanup_error) = cleanup {
            if result.is_ok() {
                return Err(cleanup_error);
            }
            eprintln!("fleet-dev: warning: Docker cleanup failed: {cleanup_error}");
        }
    }
    result
}

fn finish_without_supervisor(
    runner: &dyn CommandRunner,
    selection: &AppSelection,
    prepared: Result<PreparedLaunch>,
) -> Result<()> {
    let prepared = prepared?;
    if prepared.docker_started {
        database::stop_owned_docker(runner, &selection.trawl_root)?;
    }
    println!(
        "fleet-dev: development key {} is ready at {}",
        prepared.developer_key.prefix,
        prepared.state.api_key_path().display()
    );
    Ok(())
}

/// Say out loud what Tailscale exposure actually publishes.
///
/// Serve is the front door on the SPA port, but the API listener binds the
/// node's tailnet address so that Trunk's proxied `Host` matches the browser
/// Origin. Every tailnet peer can therefore reach the backend directly.
fn warn_tailnet_backend_exposure(plan: &DevelopmentPlan) {
    for app in &plan.apps {
        eprintln!(
            "fleet-dev: warning: {} binds its API on {}, which every tailnet peer \
             can reach directly without passing through Tailscale Serve",
            app.app, app.topology.api_bind
        );
    }
}

fn selected_manifests(selection: &AppSelection) -> BTreeMap<App, AppManifest> {
    selection
        .selected
        .iter()
        .map(|(app, checkout)| (*app, checkout.manifest.clone()))
        .collect()
}

fn check_program(runner: &dyn CommandRunner, program: &str) -> Result<()> {
    let spec = CommandSpec::new(program)
        .args(["--version"])
        .environment(environment::sanitized_base())
        .timeout(std::time::Duration::from_secs(15))
        .report_stderr();
    let output = runner.output(&spec)?;
    require_success(
        &spec,
        &output,
        format!("required program {program} is unavailable"),
    )
}

#[cfg(unix)]
fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}

#[cfg(not(unix))]
fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::sync::Mutex;

    use super::*;
    use crate::config::{
        AuthManifest, ConsumerEnvironment, DatabaseManifest, MigrationMode, ProcessManifest,
        WebManifest,
    };
    use crate::resolver::SecretValue;
    use crate::selection::AppCheckout;

    #[derive(Debug, Default)]
    struct RecordingRunner {
        seen: Mutex<Vec<CommandSpec>>,
    }

    impl CommandRunner for RecordingRunner {
        fn output(&self, spec: &CommandSpec) -> Result<std::process::Output> {
            self.seen.lock().unwrap().push(spec.clone());
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    #[test]
    fn selected_manifest_projection_is_deterministic() {
        let empty = AppSelection {
            trawl_root: "/trawl".into(),
            selected: BTreeMap::new(),
            registered: BTreeMap::new(),
            release_spa: false,
        };
        assert!(selected_manifests(&empty).is_empty());
    }

    #[test]
    fn preparation_and_migration_receive_only_declared_values() {
        let root = tempfile::tempdir().unwrap();
        let manifest = AppManifest {
            schema: 1,
            name: App::Coastwatch,
            web: WebManifest {
                local_port: 8082,
                tailscale_port: 8445,
                backend_port: 3002,
                login_path: "/login".to_owned(),
            },
            database: DatabaseManifest {
                name: "coastwatch_dev".to_owned(),
                migration_mode: MigrationMode::Command,
                migration_command: vec!["coastwatch-migrate".to_owned()],
            },
            resolver: None,
            preparation: Some(CommandManifest {
                command: vec!["prepare".to_owned()],
                cwd: None,
                env: BTreeMap::from([("COASTWATCH_ENV".to_owned(), "dev".to_owned())]),
                resolved_env: BTreeMap::from([(
                    "FLEET_DATABASE_URL".to_owned(),
                    "fleet.database_url".to_owned(),
                )]),
            }),
            auth: AuthManifest {
                permissions: vec!["coastwatch:stories_read".to_owned()],
            },
            processes: vec![ProcessManifest {
                name: "web".to_owned(),
                command: vec!["coastwatch-web".to_owned()],
                cwd: None,
                env: BTreeMap::new(),
                resolved_env: BTreeMap::new(),
            }],
            migration: ConsumerEnvironment {
                env: BTreeMap::new(),
                resolved_env: BTreeMap::from([(
                    "DATABASE_URL".to_owned(),
                    "app.database_url".to_owned(),
                )]),
            },
        };
        let checkout = AppCheckout {
            root: root.path().to_owned(),
            manifest,
        };
        let selection = AppSelection {
            trawl_root: root.path().to_owned(),
            selected: BTreeMap::from([(App::Coastwatch, checkout.clone())]),
            registered: BTreeMap::from([(App::Coastwatch, checkout)]),
            release_spa: false,
        };
        let values = RuntimeValues {
            fleet: BTreeMap::from([(
                "database_url".to_owned(),
                SecretValue::new("postgres://fleet-secret".to_owned()),
            )]),
            apps: BTreeMap::from([(
                App::Coastwatch,
                BTreeMap::from([(
                    "database_url".to_owned(),
                    SecretValue::new("postgres://coastwatch-secret".to_owned()),
                )]),
            )]),
        };
        let runner = RecordingRunner::default();
        // The controller calls these in this order: preparation runs before
        // the resolver would, migration after it.
        run_app_preparations(&runner, &selection, &values).unwrap();
        run_app_migrations(&runner, &selection, &values).unwrap();
        let seen = runner.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].program, "prepare");
        assert_eq!(
            seen[0]
                .environment
                .get(std::ffi::OsStr::new("FLEET_DATABASE_URL")),
            Some(&OsString::from("postgres://fleet-secret"))
        );
        assert!(
            !seen[0]
                .environment
                .contains_key(std::ffi::OsStr::new("DATABASE_URL"))
        );
        assert_eq!(seen[1].program, "coastwatch-migrate");
        assert_eq!(
            seen[1]
                .environment
                .get(std::ffi::OsStr::new("DATABASE_URL")),
            Some(&OsString::from("postgres://coastwatch-secret"))
        );
        assert!(seen.iter().all(|spec| {
            !spec
                .environment
                .contains_key(std::ffi::OsStr::new(crate::credentials::OP_TOKEN_ENV))
        }));
    }
}
