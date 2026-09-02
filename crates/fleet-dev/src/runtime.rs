// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

use serde::Serialize;
use tempfile::TempDir;
use tokio::process::Command;

use crate::cli::App;
use crate::environment;
use crate::error::{Error, Result};
use crate::fleet::DeveloperKey;
use crate::plan::{DevelopmentPlan, ProcessRole};
use crate::resolver::SecretValue;

#[derive(Debug)]
pub struct RuntimeValues {
    pub fleet: BTreeMap<String, SecretValue>,
    pub apps: BTreeMap<App, BTreeMap<String, SecretValue>>,
}

impl RuntimeValues {
    pub fn resolve(&self, app: App, name: &str) -> Result<&SecretValue> {
        if let Some(name) = name.strip_prefix("fleet.") {
            self.fleet.get(name)
        } else if let Some(name) = name.strip_prefix("app.") {
            self.apps.get(&app).and_then(|values| values.get(name))
        } else {
            None
        }
        .ok_or_else(|| {
            Error::InvalidArgument(format!("resolved value {name:?} is unavailable for {app}"))
        })
    }
}

#[derive(Debug)]
pub struct RuntimeFiles {
    _directory: TempDir,
    pub mprocs_config: PathBuf,
}

#[derive(Debug, Serialize)]
struct MprocsConfig {
    procs: BTreeMap<String, MprocsProcess>,
}

#[derive(Debug, Serialize)]
struct MprocsProcess {
    cmd: Vec<String>,
    cwd: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
}

pub fn render(
    plan: &DevelopmentPlan,
    values: &RuntimeValues,
    developer_key: &DeveloperKey,
    key_file: &Path,
) -> Result<RuntimeFiles> {
    let directory = tempfile::Builder::new().prefix("fleet-dev.").tempdir()?;
    set_directory_mode(directory.path())?;
    let mut procs = BTreeMap::new();
    for app_plan in &plan.apps {
        for process_plan in &app_plan.processes {
            let mut environment = process_plan.static_environment.clone();
            for (destination, source) in &process_plan.resolved_environment {
                environment.insert(
                    destination.clone(),
                    values.resolve(app_plan.app, source)?.expose().to_owned(),
                );
            }
            let mut command = process_plan.command.clone();
            apply_topology_runtime(
                process_plan.role,
                &mut command,
                &mut environment,
                app_plan,
                Path::new(&process_plan.cwd),
                directory.path(),
            )?;
            procs.insert(
                process_plan.name.clone(),
                MprocsProcess {
                    cmd: command,
                    cwd: process_plan.cwd.clone(),
                    env: environment,
                },
            );
        }
        procs.insert(
            format!("{}-login", app_plan.app.as_str()),
            login_process(
                &app_plan.topology.login_url,
                key_file,
                Path::new(&app_plan.root),
            ),
        );
    }

    let token = developer_key.token.expose().as_bytes();
    if token.is_empty() {
        // `windows(0)` panics, so an empty token would crash the leak check
        // below instead of running it. No key source produces one today; the
        // guard keeps a regression from landing as a panic.
        return Err(Error::InvalidArgument(
            "internal error: developer API key is empty".to_owned(),
        ));
    }
    let document =
        serde_json::to_vec_pretty(&MprocsConfig { procs }).expect("runtime config serializes");
    if document.windows(token.len()).any(|window| window == token) {
        return Err(Error::InvalidArgument(
            "internal error: developer API key leaked into mprocs config".to_owned(),
        ));
    }
    let path = directory.path().join("mprocs.json");
    write_private(&path, &document)?;
    Ok(RuntimeFiles {
        _directory: directory,
        mprocs_config: path,
    })
}

pub async fn run_mprocs(runtime: &RuntimeFiles) -> Result<ExitStatus> {
    run_mprocs_program(runtime, Path::new("mprocs")).await
}

async fn run_mprocs_program(runtime: &RuntimeFiles, program: &Path) -> Result<ExitStatus> {
    let mut command = Command::new(program);
    command
        .arg("--config")
        .arg(&runtime.mprocs_config)
        .env_clear()
        .envs(environment::sanitized_base())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|source| Error::Spawn {
        program: program.display().to_string(),
        source,
    })?;

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        let mut quit = signal(SignalKind::quit())?;
        tokio::select! {
            status = child.wait() => status.map_err(Error::Io),
            _ = interrupt.recv() => wait_or_forward(&mut child, nix::sys::signal::Signal::SIGINT).await,
            _ = terminate.recv() => wait_or_forward(&mut child, nix::sys::signal::Signal::SIGTERM).await,
            _ = hangup.recv() => wait_or_forward(&mut child, nix::sys::signal::Signal::SIGHUP).await,
            _ = quit.recv() => wait_or_forward(&mut child, nix::sys::signal::Signal::SIGQUIT).await,
        }
    }

    #[cfg(not(unix))]
    {
        tokio::select! {
            status = child.wait() => status.map_err(Error::Io),
            result = tokio::signal::ctrl_c() => {
                result?;
                child.start_kill()?;
                child.wait().await.map_err(Error::Io)
            }
        }
    }
}

fn apply_topology_runtime(
    role: ProcessRole,
    command: &mut Vec<String>,
    environment: &mut BTreeMap<String, String>,
    app: &crate::plan::AppPlan,
    process_root: &Path,
    runtime_dir: &Path,
) -> Result<()> {
    match role {
        ProcessRole::WebBackend => {
            insert_web_topology(environment, &app.topology);
            environment.insert(
                app.app.bind_variable().to_owned(),
                app.topology.api_bind.clone(),
            );
        }
        ProcessRole::WebUi => {
            let trunk_config = runtime_dir.join(format!("{}.Trunk.toml", app.app.as_str()));
            render_trunk_config(&trunk_config, process_root, &app.topology)?;
            command.push("--config".to_owned());
            command.push(trunk_config.display().to_string());
        }
        ProcessRole::Opaque => {}
    }
    Ok(())
}

fn insert_web_topology(
    environment: &mut BTreeMap<String, String>,
    topology: &crate::topology::AppTopology,
) {
    environment.insert(
        "FLEET_DEV_BROWSER_ORIGIN".to_owned(),
        topology.browser_origin.clone(),
    );
    environment.insert("FLEET_DEV_LOGIN_URL".to_owned(), topology.login_url.clone());
    environment.insert(
        "FLEET_DEV_BACKEND_AUTHORITY".to_owned(),
        topology.backend_authority.clone(),
    );
    environment.insert("FLEET_DEV_API_BIND".to_owned(), topology.api_bind.clone());
    environment.insert(
        fleet_auth::ENV_SESSION_COOKIE_DOMAIN.to_owned(),
        topology.cookie_domain.clone().unwrap_or_default(),
    );
    environment.insert(
        fleet_auth::ENV_SESSION_COOKIE_PATH.to_owned(),
        topology.cookie_path.clone(),
    );
    environment.insert(
        fleet_auth::ENV_SESSION_COOKIE_SECURE.to_owned(),
        topology.cookie_secure.to_string(),
    );
}

fn render_trunk_config(
    path: &Path,
    ui_root: &Path,
    topology: &crate::topology::AppTopology,
) -> Result<()> {
    let source = ui_root.join("Trunk.toml");
    let raw = std::fs::read_to_string(&source).map_err(|source_error| Error::ReadFile {
        kind: "Trunk configuration",
        path: source.clone(),
        source: source_error,
    })?;
    let mut document: toml::Table =
        toml::from_str(&raw).map_err(|source_error| Error::ParseFile {
            kind: "Trunk configuration",
            path: source,
            message: source_error.to_string(),
        })?;
    let build = document
        .get_mut("build")
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| Error::InvalidArgument("Trunk.toml requires [build]".to_owned()))?;
    build.insert(
        "target".to_owned(),
        toml::Value::String(ui_root.join("index.html").display().to_string()),
    );
    build.insert(
        "dist".to_owned(),
        toml::Value::String(ui_root.join("dist").display().to_string()),
    );
    // An app may opt into `create_nonce` for its release pipeline, where its
    // web binary substitutes the placeholder per-request and emits the full
    // CSP header. Under `trunk serve` (verified on 0.21.14) the same flag
    // makes Trunk emit a nonce-only development CSP — `style-src 'nonce-…'`
    // — that blocks the app's own stylesheets and any font origins. The
    // controller serves the SPA through Trunk, so force it off here; the
    // source Trunk.toml keeps the release behavior.
    build.insert("create_nonce".to_owned(), toml::Value::Boolean(false));
    let serve = document
        .get_mut("serve")
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| Error::InvalidArgument("Trunk.toml requires [serve]".to_owned()))?;
    let addresses = topology
        .spa_bind
        .iter()
        .cloned()
        .map(toml::Value::String)
        .collect();
    serve.insert("addresses".to_owned(), toml::Value::Array(addresses));
    serve.insert(
        "port".to_owned(),
        toml::Value::Integer(i64::from(topology.spa_port)),
    );
    let proxies = document
        .get_mut("proxy")
        .and_then(toml::Value::as_array_mut)
        .ok_or_else(|| Error::InvalidArgument("Trunk.toml requires [[proxy]]".to_owned()))?;
    if proxies.len() != 1 {
        return Err(Error::InvalidArgument(
            "Trunk.toml must declare exactly one [[proxy]]".to_owned(),
        ));
    }
    proxies[0]
        .as_table_mut()
        .ok_or_else(|| Error::InvalidArgument("[[proxy]] must be a table".to_owned()))?
        .insert(
            "backend".to_owned(),
            toml::Value::String(format!("http://{}/api/", topology.backend_authority)),
        );
    let rendered = toml::to_string_pretty(&document)
        .map_err(|error| Error::InvalidArgument(format!("failed to render Trunk.toml: {error}")))?;
    write_private(path, rendered.as_bytes())
}

fn login_process(login_url: &str, key_file: &Path, root: &Path) -> MprocsProcess {
    MprocsProcess {
        cmd: vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "printf 'Paste this API key into %s:\\n\\n' \"$FLEET_DEV_LOGIN_URL\"; cat \"$FLEET_DEV_KEY_FILE\"; printf '\\n'".to_owned(),
        ],
        cwd: root.display().to_string(),
        env: BTreeMap::from([
            (
                "FLEET_DEV_LOGIN_URL".to_owned(),
                login_url.to_owned(),
            ),
            (
                "FLEET_DEV_KEY_FILE".to_owned(),
                key_file.display().to_string(),
            ),
        ]),
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn set_directory_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_directory_mode(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
async fn wait_or_forward(
    child: &mut tokio::process::Child,
    signal: nix::sys::signal::Signal,
) -> Result<ExitStatus> {
    if let Ok(status) = tokio::time::timeout(Duration::from_millis(250), child.wait()).await {
        return status.map_err(Error::Io);
    }
    if let Some(id) = child.id() {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(id.cast_signed()), signal);
    }
    if let Ok(status) = tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        status.map_err(Error::Io)
    } else {
        if let Some(id) = child.id() {
            kill_descendants(id);
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(id.cast_signed()),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        child.start_kill()?;
        child.wait().await.map_err(Error::Io)
    }
}

#[cfg(target_os = "linux")]
fn kill_descendants(root: u32) {
    let mut descendants = Vec::new();
    collect_descendants(root, &mut descendants);
    for pid in descendants.into_iter().rev() {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid.cast_signed()),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

#[cfg(target_os = "linux")]
fn collect_descendants(parent: u32, output: &mut Vec<u32>) {
    let path = format!("/proc/{parent}/task/{parent}/children");
    let Ok(children) = std::fs::read_to_string(path) else {
        return;
    };
    for child in children
        .split_ascii_whitespace()
        .filter_map(|value| value.parse::<u32>().ok())
    {
        output.push(child);
        collect_descendants(child, output);
    }
}

#[cfg(target_os = "macos")]
fn kill_descendants(root: u32) {
    let mut descendants = Vec::new();
    collect_descendants(root, &mut descendants);
    for pid in descendants.into_iter().rev() {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid.cast_signed()),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

#[cfg(target_os = "macos")]
fn collect_descendants(parent: u32, output: &mut Vec<u32>) {
    let result = std::process::Command::new("/usr/bin/pgrep")
        .args(["-P", &parent.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(result) = result else {
        return;
    };
    for child in String::from_utf8_lossy(&result.stdout)
        .split_ascii_whitespace()
        .filter_map(|value| value.parse::<u32>().ok())
    {
        output.push(child);
        collect_descendants(child, output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_process_contains_only_paths_not_plaintext_key() {
        let process = login_process(
            "http://localhost:8081/login",
            Path::new("/state/dev-api-key"),
            Path::new("/repo"),
        );
        let json = serde_json::to_string(&process).unwrap();
        assert!(json.contains("/state/dev-api-key"));
        assert!(!json.contains("flt_"));
        assert!(!json.contains("postgres://"));
    }

    #[test]
    fn an_empty_developer_key_is_refused_rather_than_panicking() {
        // No key source produces an empty token, but `windows(0)` would panic
        // if one did.
        let plan = DevelopmentPlan {
            schema: 1,
            exposure: crate::cli::Exposure::Localhost,
            database: crate::cli::DatabaseMode::Docker,
            state_scope: "docker".to_owned(),
            requires_onepassword: false,
            apps: vec![],
            actions: vec![],
        };
        let values = RuntimeValues {
            fleet: BTreeMap::new(),
            apps: BTreeMap::new(),
        };
        let key = DeveloperKey {
            token: SecretValue::new(String::new()),
            prefix: "flt_prefix".to_owned(),
        };
        let error = render(&plan, &values, &key, Path::new("/state/dev-api-key")).unwrap_err();
        assert!(
            error.to_string().contains("developer API key is empty"),
            "got: {error}"
        );
    }

    #[test]
    fn coastwatch_shaped_trunk_config_disables_the_serve_mode_nonce_csp() {
        let directory = tempfile::tempdir().unwrap();
        let ui = directory.path().join("web-ui");
        std::fs::create_dir(&ui).unwrap();
        std::fs::write(
            ui.join("Trunk.toml"),
            r#"
[build]
target = "index.html"
dist = "dist"
create_nonce = true
nonce_placeholder = "__CSP_NONCE__"

[serve]
addresses = ["127.0.0.1"]
port = 8082

[[proxy]]
backend = "http://127.0.0.1:3002/api/"
no_redirect = true
"#,
        )
        .unwrap();
        let output = directory.path().join("generated.toml");
        let topology = crate::topology::AppTopology {
            app: App::Coastwatch,
            browser_origin: "https://fractal.example.ts.net:8445".to_owned(),
            login_url: "https://fractal.example.ts.net:8445/login".to_owned(),
            backend_authority: "fractal.example.ts.net:3002".to_owned(),
            api_bind: "100.64.0.10:3002".to_owned(),
            spa_bind: vec!["127.0.0.1".to_owned(), "::1".to_owned()],
            spa_port: 8082,
            cookie_domain: None,
            cookie_path: "/".to_owned(),
            cookie_secure: true,
        };
        let source_before = std::fs::read_to_string(ui.join("Trunk.toml")).unwrap();
        render_trunk_config(&output, &ui, &topology).unwrap();
        let rendered: toml::Value =
            toml::from_str(&std::fs::read_to_string(output).unwrap()).unwrap();
        // Trunk's serve-mode nonce CSP blocks the app's own stylesheets, so
        // the generated config must always disable it…
        assert_eq!(rendered["build"]["create_nonce"].as_bool(), Some(false));
        // …while the app's source Trunk.toml keeps the release-build nonce
        // pipeline untouched.
        assert_eq!(
            std::fs::read_to_string(ui.join("Trunk.toml")).unwrap(),
            source_before
        );
        assert_eq!(rendered["proxy"][0]["no_redirect"].as_bool(), Some(true));
        assert_eq!(
            rendered["proxy"][0]["backend"].as_str(),
            Some("http://fractal.example.ts.net:3002/api/")
        );
        assert_eq!(rendered["serve"]["addresses"].as_array().unwrap().len(), 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supervisor_status_is_propagated_and_runtime_cleans_up_on_drop() {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let directory = tempfile::Builder::new()
            .prefix("fleet-dev-supervisor-test.")
            .tempdir()
            .unwrap();
        let config = directory.path().join("mprocs.json");
        std::fs::write(&config, b"{\"procs\":{}}").unwrap();
        let runtime = RuntimeFiles {
            _directory: directory,
            mprocs_config: config,
        };
        let runtime_root = runtime.mprocs_config.parent().unwrap().to_owned();

        let scripts = tempfile::tempdir().unwrap();
        let program = scripts.path().join("fake-mprocs");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o700);
        let mut file = options.open(&program).unwrap();
        file.write_all(b"#!/bin/sh\nexit 23\n").unwrap();
        drop(file);
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();

        let status = run_mprocs_program(&runtime, &program).await.unwrap();
        assert_eq!(status.code(), Some(23));
        assert!(runtime_root.exists());
        drop(runtime);
        assert!(!runtime_root.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn forwarded_signal_is_observed_by_the_supervisor() {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let scripts = tempfile::tempdir().unwrap();
        let program = scripts.path().join("signal-supervisor");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&program)
            .unwrap();
        file.write_all(b"#!/bin/sh\ntrap 'exit 42' TERM\nwhile :; do sleep 1; done\n")
            .unwrap();
        drop(file);
        let mut command = Command::new(&program);
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }
        let mut child = command.spawn().unwrap();
        let status = wait_or_forward(&mut child, nix::sys::signal::Signal::SIGTERM)
            .await
            .unwrap();
        assert_eq!(status.code(), Some(42));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn forced_cleanup_kills_a_supervisor_grandchild_tree() {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::process::CommandExt;

        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("grandchild.pid");
        let program = directory.path().join("tree-supervisor");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&program)
            .unwrap();
        file.write_all(
            b"#!/bin/sh\ntrap '' TERM\nsh -c 'trap \"\" TERM; while :; do sleep 1; done' &\necho $! > \"$PID_FILE\"\nwait\n",
        )
        .unwrap();
        drop(file);

        let mut command = Command::new(&program);
        command.env("PID_FILE", &pid_file);
        command.as_std_mut().process_group(0);
        let mut child = command.spawn().unwrap();
        let grandchild = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(raw) = std::fs::read_to_string(&pid_file) {
                    break raw.trim().parse::<u32>().unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let root = child.id().unwrap();
        kill_descendants(root);
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(root.cast_signed()),
            nix::sys::signal::Signal::SIGKILL,
        );
        child.wait().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while Path::new(&format!("/proc/{grandchild}")).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}
