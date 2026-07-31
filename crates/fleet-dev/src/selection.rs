// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::cli::{AllArgs, App, Cli, Command, DevArgs, Target};
use crate::config::{AppManifest, MachineProfile, load_manifest};
use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct AppSelection {
    pub trawl_root: PathBuf,
    pub selected: BTreeMap<App, AppCheckout>,
    /// Every discoverable manifest contributes permissions even when its
    /// processes are not selected.
    pub registered: BTreeMap<App, AppCheckout>,
    pub release_spa: bool,
}

#[derive(Debug, Clone)]
pub struct AppCheckout {
    pub root: PathBuf,
    pub manifest: AppManifest,
}

pub fn discover(cli: &Cli, profile: &MachineProfile) -> Result<AppSelection> {
    let trawl_root = resolve_trawl_root(cli.trawl_root.as_deref())?;
    let trawl = load_checkout(App::Trawl, &trawl_root)?;

    let (selected_apps, explicit_coastwatch, release_spa): (Vec<App>, Option<PathBuf>, bool) =
        match &cli.command {
            Command::Setup { target, .. } | Command::Doctor { target } => {
                let apps = target.map_or_else(
                    || {
                        let mut apps = vec![App::Trawl];
                        if optional_coastwatch(profile, &trawl_root).is_some() {
                            apps.push(App::Coastwatch);
                        }
                        apps
                    },
                    target_apps,
                );
                (apps, None, false)
            }
            Command::Plan { target, .. } => (target_apps(*target), None, false),
            Command::Dev(args) => dev_selection(args, &trawl_root)?,
            Command::All(args) => all_selection(args),
        };
    if release_spa && !selected_apps.contains(&App::Trawl) {
        return Err(Error::InvalidArgument(
            "--release-spa is valid only when Trawl is selected".to_owned(),
        ));
    }

    let coastwatch_root =
        if explicit_coastwatch.is_some() || selected_apps.contains(&App::Coastwatch) {
            discover_coastwatch(profile, &trawl_root, explicit_coastwatch.as_deref())?
        } else {
            optional_coastwatch(profile, &trawl_root)
        };
    let mut registered = BTreeMap::from([(App::Trawl, trawl)]);
    if let Some(root) = coastwatch_root.as_deref() {
        match load_checkout(App::Coastwatch, root) {
            Ok(checkout) => {
                registered.insert(App::Coastwatch, checkout);
            }
            Err(error) if !selected_apps.contains(&App::Coastwatch) => {
                tracing::debug!(%error, "ignoring undiscoverable optional Coastwatch manifest");
            }
            Err(error) => return Err(error),
        }
    }
    let mut selected = BTreeMap::new();
    for app in selected_apps {
        let checkout = registered.get(&app).ok_or_else(|| {
            Error::InvalidArgument(format!(
                "{app} was selected but no checkout with a valid fleet-dev.toml was found"
            ))
        })?;
        selected.insert(app, checkout.clone());
    }
    Ok(AppSelection {
        trawl_root,
        selected,
        registered,
        release_spa,
    })
}

fn optional_coastwatch(profile: &MachineProfile, trawl_root: &Path) -> Option<PathBuf> {
    match discover_coastwatch(profile, trawl_root, None) {
        Ok(root) => root,
        Err(error) => {
            tracing::debug!(%error, "ignoring unavailable optional Coastwatch checkout");
            None
        }
    }
}

fn dev_selection(args: &DevArgs, trawl_root: &Path) -> Result<(Vec<App>, Option<PathBuf>, bool)> {
    let root = canonical_directory(&args.app_root, args.app)?;
    if args.app == App::Trawl && root != trawl_root {
        return Err(Error::InvalidArgument(format!(
            "dev trawl --app-root {} does not match --trawl-root {}",
            root.display(),
            trawl_root.display()
        )));
    }
    Ok((
        vec![args.app],
        (args.app == App::Coastwatch).then_some(root),
        args.release_spa,
    ))
}

fn all_selection(args: &AllArgs) -> (Vec<App>, Option<PathBuf>, bool) {
    (
        vec![App::Trawl, App::Coastwatch],
        args.coastwatch_root.clone(),
        args.release_spa,
    )
}

fn target_apps(target: Target) -> Vec<App> {
    match target {
        Target::Trawl => vec![App::Trawl],
        Target::Coastwatch => vec![App::Coastwatch],
        Target::All => vec![App::Trawl, App::Coastwatch],
    }
}

fn resolve_trawl_root(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(explicit) = explicit {
        return canonical_directory(explicit, App::Trawl);
    }
    let cwd = std::env::current_dir()?;
    for candidate in cwd.ancestors() {
        if candidate.join("Cargo.toml").is_file() && candidate.join("crates/fleet-auth").is_dir() {
            return canonical_directory(candidate, App::Trawl);
        }
    }
    Err(Error::InvalidArgument(
        "could not locate the Trawl checkout; use trawl/bin/fleet-dev or pass --trawl-root"
            .to_owned(),
    ))
}

fn discover_coastwatch(
    profile: &MachineProfile,
    trawl_root: &Path,
    explicit: Option<&Path>,
) -> Result<Option<PathBuf>> {
    if let Some(path) = explicit {
        return canonical_directory(path, App::Coastwatch).map(Some);
    }
    if let Some(path) = profile.paths.coastwatch.as_deref() {
        return canonical_directory(&crate::config::expand_tilde(path), App::Coastwatch).map(Some);
    }
    let sibling = trawl_root
        .parent()
        .map(|parent| parent.join("coastwatch"))
        .ok_or_else(|| Error::InvalidArgument("Trawl root has no parent directory".to_owned()))?;
    if sibling.is_dir() && sibling.join(crate::config::MANIFEST_FILE).is_file() {
        canonical_directory(&sibling, App::Coastwatch).map(Some)
    } else {
        Ok(None)
    }
}

fn load_checkout(expected: App, root: &Path) -> Result<AppCheckout> {
    let manifest = load_manifest(root)?;
    if manifest.name != expected {
        return Err(Error::InvalidArgument(format!(
            "{} declares app {}, expected {expected}",
            root.join(crate::config::MANIFEST_FILE).display(),
            manifest.name
        )));
    }
    Ok(AppCheckout {
        root: root.to_owned(),
        manifest,
    })
}

fn canonical_directory(path: &Path, app: App) -> Result<PathBuf> {
    let expanded = crate::config::expand_tilde(path);
    let canonical = std::fs::canonicalize(&expanded).map_err(|source| Error::ReadFile {
        kind: "application checkout",
        path: expanded.clone(),
        source,
    })?;
    if !canonical.is_dir() {
        return Err(Error::InvalidArgument(format!(
            "{app} checkout {} is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::config::PathsProfile;

    fn trawl_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .to_owned()
    }

    fn cli(arguments: &[&str]) -> Cli {
        let root = trawl_root();
        let mut values = vec![
            "fleet-dev".to_owned(),
            "--trawl-root".to_owned(),
            root.display().to_string(),
        ];
        values.extend(arguments.iter().map(ToString::to_string));
        Cli::try_parse_from(values).unwrap()
    }

    fn write_coastwatch_manifest(root: &Path) {
        std::fs::write(
            root.join(crate::config::MANIFEST_FILE),
            r#"
schema = 1
name = "coastwatch"

[web]
local_port = 8082
tailscale_port = 8445
backend_port = 3002
login_path = "/login"

[database]
name = "coastwatch_dev"
migration_mode = "command"
migration_command = ["coastwatch-migrate"]

[auth]
permissions = ["coastwatch:stories_read"]

[[processes]]
name = "web"
command = ["coastwatch-web"]

[processes.resolved_env]
FLEET_SESSION_AEAD_KEY = "fleet.session_aead_key"

[[processes]]
name = "web-ui"
command = ["trunk", "serve"]
cwd = "crates/web-ui"

[migration.resolved_env]
DATABASE_URL = "app.database_url"
"#,
        )
        .unwrap();
    }

    #[test]
    fn stale_optional_coastwatch_path_does_not_break_trawl_only_selection() {
        let profile = MachineProfile {
            paths: PathsProfile {
                coastwatch: Some("/definitely/missing/coastwatch".into()),
            },
            ..MachineProfile::default()
        };
        let selection = discover(&cli(&["plan", "trawl"]), &profile).unwrap();
        assert_eq!(
            selection.selected.keys().copied().collect::<Vec<_>>(),
            [App::Trawl]
        );
    }

    #[test]
    fn explicit_coastwatch_root_precedes_a_stale_profile_path() {
        let coastwatch = tempfile::tempdir().unwrap();
        write_coastwatch_manifest(coastwatch.path());
        let profile = MachineProfile {
            paths: PathsProfile {
                coastwatch: Some("/definitely/missing/coastwatch".into()),
            },
            ..MachineProfile::default()
        };
        let selection = discover(
            &cli(&[
                "all",
                "--coastwatch-root",
                coastwatch.path().to_str().unwrap(),
            ]),
            &profile,
        )
        .unwrap();
        assert_eq!(
            selection.selected[&App::Coastwatch].root,
            std::fs::canonicalize(coastwatch.path()).unwrap()
        );
    }
}
