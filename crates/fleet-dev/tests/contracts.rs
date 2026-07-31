// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};

use clap::Parser;
use fleet_dev::cli::{App, Cli};
use fleet_dev::command::{CommandRunner, CommandSpec};
use fleet_dev::config::{MachineProfile, TRAWL_DEV_PERMISSIONS, load_manifest};
use fleet_dev::error::Result;
use fleet_dev::fleet::DeveloperKey;
use fleet_dev::plan;
use fleet_dev::resolver::SecretValue;
use fleet_dev::runtime::{self, RuntimeValues};
use fleet_dev::selection;

fn trawl_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("fleet-dev lives at crates/fleet-dev")
        .to_owned()
}

fn trawl_cli(extra: &[&str]) -> Cli {
    let root = trawl_root();
    let mut arguments = vec![
        "fleet-dev".to_owned(),
        "--trawl-root".to_owned(),
        root.display().to_string(),
    ];
    arguments.extend(extra.iter().map(ToString::to_string));
    Cli::try_parse_from(arguments).unwrap()
}

#[derive(Debug, Default)]
struct NoCommands(AtomicUsize);

impl CommandRunner for NoCommands {
    fn output(&self, spec: &CommandSpec) -> Result<Output> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("plan unexpectedly spawned {spec:?}");
    }
}

#[test]
fn committed_trawl_manifest_pins_the_frozen_contract() {
    let manifest = load_manifest(&trawl_root()).unwrap();
    assert_eq!(manifest.name, App::Trawl);
    assert_eq!(
        manifest.auth.permissions,
        TRAWL_DEV_PERMISSIONS.map(ToOwned::to_owned)
    );
    assert!(
        !manifest
            .auth
            .permissions
            .iter()
            .any(|item| item.ends_with(":ingest"))
    );
    assert_eq!(manifest.database.name, "trawl_dev");
    assert_eq!(
        manifest
            .processes
            .iter()
            .map(|process| process.name.as_str())
            .collect::<Vec<_>>(),
        ["trawld", "trawl-web", "web-ui"]
    );
    assert_eq!(
        manifest.preparation.as_ref().unwrap().command,
        ["cargo", "build", "-p", "trawl-server", "-p", "trawl-web"]
    );
    assert!(
        manifest
            .processes
            .iter()
            .filter(|process| !process.resolved_env.is_empty())
            .all(|process| process
                .command
                .first()
                .is_some_and(|program| program != "cargo")),
        "secret-bearing runtime processes must execute built binaries, not Cargo"
    );
}

#[tokio::test]
async fn docker_plan_is_secret_free_and_does_not_touch_commands_or_token_file() {
    let temporary = tempfile::tempdir().unwrap();
    let profile_path = temporary.path().join("dev.toml");
    std::fs::write(
        &profile_path,
        "database = \"docker\"\nexposure = \"localhost\"\n[op]\ntoken_file = \"/definitely/missing\"\n",
    )
    .unwrap();
    let root = trawl_root();
    let cli = Cli::try_parse_from([
        "fleet-dev",
        "--trawl-root",
        root.to_str().unwrap(),
        "--config",
        profile_path.to_str().unwrap(),
        "plan",
        "trawl",
        "--format",
        "json",
    ])
    .unwrap();
    let runner = NoCommands::default();
    assert_eq!(fleet_dev::controller::run(cli, &runner).await.unwrap(), 0);
    assert_eq!(runner.0.load(Ordering::SeqCst), 0);
}

#[test]
fn trawl_plan_and_release_spa_are_deterministic() {
    let profile = MachineProfile::default();
    let normal = selection::discover(&trawl_cli(&["plan", "trawl"]), &profile).unwrap();
    let normal = plan::build(&profile, &normal, None).unwrap();
    assert_eq!(normal.apps.len(), 1);
    assert_eq!(
        normal.apps[0].topology.browser_origin,
        "http://localhost:8081"
    );
    assert_eq!(
        normal.apps[0]
            .processes
            .iter()
            .map(|process| process.name.as_str())
            .collect::<Vec<_>>(),
        ["trawl-trawld", "trawl-trawl-web", "trawl-web-ui"]
    );
    assert!(!normal.requires_onepassword);

    let root = trawl_root();
    let release = selection::discover(
        &trawl_cli(&[
            "dev",
            "trawl",
            "--app-root",
            root.to_str().unwrap(),
            "--release-spa",
        ]),
        &profile,
    )
    .unwrap();
    let release = plan::build(&profile, &release, None).unwrap();
    let web_ui = release.apps[0]
        .processes
        .iter()
        .find(|process| process.name == "trawl-web-ui")
        .unwrap();
    assert_eq!(web_ui.command, ["trunk", "serve", "--release"]);
}

#[test]
fn rendered_runtime_is_private_isolated_and_ephemeral() {
    let profile = MachineProfile::default();
    let selection = selection::discover(&trawl_cli(&["plan", "trawl"]), &profile).unwrap();
    let plan = plan::build(&profile, &selection, None).unwrap();
    let values = RuntimeValues {
        fleet: BTreeMap::from([
            (
                "database_url".to_owned(),
                SecretValue::new("postgres://fleet-secret".to_owned()),
            ),
            (
                "session_aead_key".to_owned(),
                SecretValue::new("session-secret".to_owned()),
            ),
        ]),
        apps: BTreeMap::from([(
            App::Trawl,
            BTreeMap::from([(
                "database_url".to_owned(),
                SecretValue::new("postgres://trawl-secret".to_owned()),
            )]),
        )]),
    };
    let key = DeveloperKey {
        token: SecretValue::new("flt_plaintext_developer_key".to_owned()),
        prefix: "flt_prefix".to_owned(),
    };
    let state = tempfile::tempdir().unwrap();
    let key_file = state.path().join("dev-api-key");
    std::fs::write(&key_file, key.token.expose()).unwrap();

    let runtime = runtime::render(&selection, &plan, &values, &key, &key_file).unwrap();
    let runtime_dir = runtime.mprocs_config.parent().unwrap().to_owned();
    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&runtime.mprocs_config).unwrap()).unwrap();
    let procs = document["procs"].as_object().unwrap();
    let trawld = procs["trawl-trawld"]["env"].as_object().unwrap();
    assert_eq!(trawld["FLEET_DATABASE_URL"], "postgres://fleet-secret");
    assert_eq!(trawld["TRAWL_DATABASE_URL"], "postgres://trawl-secret");
    assert!(!trawld.contains_key("FLEET_SESSION_AEAD_KEY"));

    let web = procs["trawl-trawl-web"]["env"].as_object().unwrap();
    assert_eq!(web["FLEET_SESSION_AEAD_KEY"], "session-secret");
    assert_eq!(web["FLEET_SESSION_COOKIE_DOMAIN"], "");
    assert_eq!(web["FLEET_SESSION_COOKIE_PATH"], "/");
    assert_eq!(web["FLEET_SESSION_COOKIE_SECURE"], "false");
    assert!(!web.contains_key("FLEET_DATABASE_URL"));
    assert!(!web.contains_key("TRAWL_DATABASE_URL"));

    let raw = std::fs::read_to_string(&runtime.mprocs_config).unwrap();
    assert!(!raw.contains("flt_plaintext_developer_key"));
    assert!(!raw.contains("OP_SERVICE_ACCOUNT_TOKEN"));
    let trunk_path = runtime_dir.join("trawl.Trunk.toml");
    let trunk = std::fs::read_to_string(trunk_path).unwrap();
    assert!(trunk.contains("backend = \"http://localhost:8090/api/\""));
    assert!(trunk.contains("port = 8081"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(&runtime_dir).unwrap().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&runtime.mprocs_config).unwrap().mode() & 0o777,
            0o600
        );
    }

    drop(runtime);
    assert!(!runtime_dir.exists());
}
