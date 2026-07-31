// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::fmt::Write;

use serde::{Deserialize, Serialize};

use crate::cli::{App, DatabaseMode, Exposure, PlanFormat};
use crate::database;
use crate::error::Result;
use crate::selection::AppSelection;
use crate::topology::{AppTopology, TailscaleNode, resolve, validate_unique};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DevelopmentPlan {
    pub schema: u32,
    pub exposure: Exposure,
    pub database: DatabaseMode,
    pub state_scope: String,
    pub requires_onepassword: bool,
    pub apps: Vec<AppPlan>,
    pub actions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppPlan {
    pub app: App,
    pub root: String,
    pub database: String,
    pub topology: AppTopology,
    pub permissions: Vec<String>,
    pub processes: Vec<ProcessPlan>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProcessPlan {
    pub name: String,
    pub command: Vec<String>,
    pub cwd: String,
    pub static_environment: BTreeMap<String, String>,
    pub resolved_environment: BTreeMap<String, String>,
}

pub fn build(
    profile: &crate::config::MachineProfile,
    selection: &AppSelection,
    tailscale: Option<&TailscaleNode>,
) -> Result<DevelopmentPlan> {
    let mut topologies = Vec::new();
    let mut apps = Vec::new();
    for (app, checkout) in &selection.selected {
        let topology = resolve(*app, &checkout.manifest.web, profile, tailscale)?;
        topologies.push(topology.clone());
        let processes = checkout
            .manifest
            .processes
            .iter()
            .map(|process| {
                let mut command = process.command.clone();
                if selection.release_spa
                    && *app == App::Trawl
                    && process.name == "web-ui"
                    && !command.iter().any(|arg| arg == "--release")
                {
                    command.push("--release".to_owned());
                }
                ProcessPlan {
                    name: format!("{}-{}", app.as_str(), process.name),
                    command,
                    cwd: process.cwd.as_ref().map_or_else(
                        || checkout.root.display().to_string(),
                        |cwd| checkout.root.join(cwd).display().to_string(),
                    ),
                    static_environment: process.env.clone(),
                    resolved_environment: process.resolved_env.clone(),
                }
            })
            .collect();
        apps.push(AppPlan {
            app: *app,
            root: checkout.root.display().to_string(),
            database: checkout.manifest.database.name.clone(),
            topology,
            permissions: checkout.manifest.auth.permissions.clone(),
            processes,
        });
    }
    validate_unique(&topologies)?;
    let manifests = selection
        .selected
        .iter()
        .map(|(app, checkout)| (*app, checkout.manifest.clone()))
        .collect();
    let state_scope = match profile.database {
        DatabaseMode::Docker => database::DOCKER_SCOPE.to_owned(),
        DatabaseMode::Cnpg => profile
            .cnpg
            .as_ref()
            .expect("validated CNPG profile")
            .state_scope
            .clone(),
    };
    Ok(DevelopmentPlan {
        schema: 1,
        exposure: profile.exposure,
        database: profile.database,
        state_scope,
        requires_onepassword: database::needs_onepassword(profile.database, &manifests),
        apps,
        actions: vec![
            "acquire global single-instance lock".to_owned(),
            format!(
                "prepare and validate {} database provider",
                mode(profile.database)
            ),
            "run fleet-admin migrate".to_owned(),
            "run selected application migration owners".to_owned(),
            "reconcile additive fleet-developer permissions and key".to_owned(),
            "render private Trunk and mprocs configuration".to_owned(),
            "run attached mprocs supervisor".to_owned(),
        ],
    })
}

pub fn render(plan: &DevelopmentPlan, format: PlanFormat) -> String {
    match format {
        PlanFormat::Json => serde_json::to_string_pretty(plan).expect("plan is serializable"),
        PlanFormat::Human => render_human(plan),
    }
}

fn render_human(plan: &DevelopmentPlan) -> String {
    let mut output = format!(
        "Fleet development plan\n\nExposure: {:?}\nDatabase: {:?}\nState scope: {}\n1Password: {}\n",
        plan.exposure,
        plan.database,
        plan.state_scope,
        if plan.requires_onepassword {
            "required at execution"
        } else {
            "not required"
        }
    );
    for app in &plan.apps {
        let _ = write!(
            output,
            "\n{} ({})\n  origin: {}\n  backend: {}\n  database: {}\n",
            app.app,
            app.root,
            app.topology.browser_origin,
            app.topology.backend_authority,
            app.database
        );
        for process in &app.processes {
            let _ = writeln!(output, "  process {}: {:?}", process.name, process.command);
            for (destination, source) in &process.resolved_environment {
                let _ = writeln!(output, "    {destination} <- {source}");
            }
        }
    }
    output.push_str("\nActions:\n");
    for (index, action) in plan.actions.iter().enumerate() {
        let _ = writeln!(output, "  {}. {action}", index + 1);
    }
    output
}

const fn mode(mode: DatabaseMode) -> &'static str {
    match mode {
        DatabaseMode::Docker => "Docker",
        DatabaseMode::Cnpg => "CNPG",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AppManifest, AuthManifest, ConsumerEnvironment, DatabaseManifest, MachineProfile,
        MigrationMode, ProcessManifest, WebManifest,
    };
    use crate::selection::{AppCheckout, AppSelection};

    fn checkout(app: App) -> AppCheckout {
        let (local_port, tailscale_port, backend_port) = match app {
            App::Trawl => (8081, 8444, 8090),
            App::Coastwatch => (8082, 8445, 3002),
        };
        AppCheckout {
            root: format!("/src/{}", app.as_str()).into(),
            manifest: AppManifest {
                schema: 1,
                name: app,
                web: WebManifest {
                    local_port,
                    tailscale_port,
                    backend_port,
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
                    permissions: Vec::new(),
                },
                processes: vec![ProcessManifest {
                    name: "web".to_owned(),
                    command: vec![format!("{}-web", app.as_str())],
                    cwd: None,
                    env: BTreeMap::new(),
                    resolved_env: BTreeMap::new(),
                }],
                migration: ConsumerEnvironment::default(),
            },
        }
    }

    fn selection(apps: &[App]) -> AppSelection {
        let registered = [App::Trawl, App::Coastwatch]
            .map(|app| (app, checkout(app)))
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        let selected = apps
            .iter()
            .map(|app| (*app, registered[app].clone()))
            .collect();
        AppSelection {
            trawl_root: "/src/trawl".into(),
            selected,
            registered,
            release_spa: false,
        }
    }

    #[test]
    fn plan_json_never_has_a_secret_value_field() {
        let empty = DevelopmentPlan {
            schema: 1,
            exposure: Exposure::Localhost,
            database: DatabaseMode::Docker,
            state_scope: "docker".to_owned(),
            requires_onepassword: false,
            apps: vec![],
            actions: vec![],
        };
        let json = render(&empty, PlanFormat::Json);
        assert!(!json.contains("flt_"));
        assert!(!json.contains("postgres://"));
        assert!(!json.contains("op://"));
        assert!(!json.contains("session_aead_key_ref"));
    }

    #[test]
    fn trawl_coastwatch_and_all_process_plans_are_selection_scoped() {
        let profile = MachineProfile::default();
        for (apps, expected) in [
            (
                vec![App::Trawl],
                vec![("trawl", "trawl-web", "http://localhost:8081")],
            ),
            (
                vec![App::Coastwatch],
                vec![("coastwatch", "coastwatch-web", "http://localhost:8082")],
            ),
            (
                vec![App::Trawl, App::Coastwatch],
                vec![
                    ("trawl", "trawl-web", "http://localhost:8081"),
                    ("coastwatch", "coastwatch-web", "http://localhost:8082"),
                ],
            ),
        ] {
            let plan = build(&profile, &selection(&apps), None).unwrap();
            assert_eq!(plan.apps.len(), expected.len());
            for (app, process, origin) in expected {
                let app = plan
                    .apps
                    .iter()
                    .find(|candidate| candidate.app.as_str() == app)
                    .unwrap();
                assert_eq!(app.processes[0].name, process);
                assert_eq!(app.topology.browser_origin, origin);
            }
        }
    }
}
