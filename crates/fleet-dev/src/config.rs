// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cli::{App, DatabaseMode, Exposure};
use crate::error::{Error, Result};

pub const PROFILE_KIND: &str = "machine profile";
pub const MANIFEST_KIND: &str = "app manifest";
pub const DEFAULT_PROFILE_PATH: &str = "~/.config/fleet/dev.toml";
pub const MANIFEST_FILE: &str = "fleet-dev.toml";
/// Manifest process name of the Trunk-served SPA, identical across apps.
pub const WEB_UI_PROCESS: &str = "web-ui";
const OP_SERVICE_ACCOUNT_TOKEN: &str = "OP_SERVICE_ACCOUNT_TOKEN";
pub const TRAWL_DEV_PERMISSIONS: [&str; 8] = [
    "trawl:query",
    "trawl:schema_read",
    "trawl:validate",
    "trawl:saved_query",
    "trawl:export",
    "trawl:stream",
    "trawl:query_cancel",
    "trawl:server_manage",
];

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MachineProfile {
    #[serde(default)]
    pub exposure: Exposure,
    #[serde(default)]
    pub database: DatabaseMode,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default)]
    pub op: OpProfile,
    #[serde(default)]
    pub paths: PathsProfile,
    pub cnpg: Option<CnpgProfile>,
    #[serde(default)]
    pub tailscale: TailscaleProfile,
}

impl Default for MachineProfile {
    fn default() -> Self {
        Self {
            exposure: Exposure::Localhost,
            database: DatabaseMode::Docker,
            host: default_host(),
            op: OpProfile::default(),
            paths: PathsProfile::default(),
            cnpg: None,
            tailscale: TailscaleProfile::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpProfile {
    pub token_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PathsProfile {
    pub coastwatch: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CnpgProfile {
    pub host: String,
    #[serde(default = "default_pg_port")]
    pub port: u16,
    pub state_scope: String,
    pub credential_ref_template: String,
    pub session_aead_key_ref: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TailscaleProfile {
    pub hostname: Option<String>,
    pub ipv4: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppManifest {
    pub schema: u32,
    pub name: App,
    pub web: WebManifest,
    pub database: DatabaseManifest,
    #[serde(default)]
    pub resolver: Option<ResolverManifest>,
    #[serde(default)]
    pub preparation: Option<CommandManifest>,
    pub auth: AuthManifest,
    #[serde(default)]
    pub processes: Vec<ProcessManifest>,
    #[serde(default)]
    pub migration: ConsumerEnvironment,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebManifest {
    pub local_port: u16,
    pub tailscale_port: u16,
    pub backend_port: u16,
    #[serde(default = "default_login_path")]
    pub login_path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseManifest {
    pub name: String,
    pub migration_mode: MigrationMode,
    #[serde(default)]
    pub migration_command: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MigrationMode {
    ApplicationStartup,
    Command,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolverManifest {
    pub command: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub resolved_env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandManifest {
    pub command: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub resolved_env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthManifest {
    #[serde(default)]
    pub permissions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessManifest {
    pub name: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub resolved_env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerEnvironment {
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub resolved_env: BTreeMap<String, String>,
}

fn default_host() -> String {
    "localhost".to_owned()
}

const fn default_pg_port() -> u16 {
    5432
}

fn default_login_path() -> String {
    "/login".to_owned()
}

pub fn load_profile(explicit_path: Option<&Path>) -> Result<(MachineProfile, PathBuf)> {
    let path = explicit_path.map_or_else(
        || expand_tilde(Path::new(DEFAULT_PROFILE_PATH)),
        expand_tilde,
    );
    match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let profile: MachineProfile =
                toml::from_str(&raw).map_err(|source| Error::ParseFile {
                    kind: PROFILE_KIND,
                    path: path.clone(),
                    message: source.to_string(),
                })?;
            profile.validate_structure(&path)?;
            Ok((profile, path))
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound && explicit_path.is_none() => {
            Ok((MachineProfile::default(), path))
        }
        Err(source) => Err(Error::ReadFile {
            kind: PROFILE_KIND,
            path,
            source,
        }),
    }
}

pub fn load_manifest(root: &Path) -> Result<AppManifest> {
    let path = root.join(MANIFEST_FILE);
    let raw = std::fs::read_to_string(&path).map_err(|source| Error::ReadFile {
        kind: MANIFEST_KIND,
        path: path.clone(),
        source,
    })?;
    let manifest: AppManifest = toml::from_str(&raw).map_err(|source| Error::ParseFile {
        kind: MANIFEST_KIND,
        path: path.clone(),
        message: source.to_string(),
    })?;
    manifest.validate(&path)?;
    Ok(manifest)
}

impl MachineProfile {
    fn validate_structure(&self, path: &Path) -> Result<()> {
        validate_hostname(&self.host, "host")
            .map_err(|message| invalid_error(PROFILE_KIND, path, message))?;
        if let Some(cnpg) = &self.cnpg {
            cnpg.validate(path)?;
        }
        Ok(())
    }

    pub fn validate(&self, path: &Path) -> Result<()> {
        self.validate_structure(path)?;
        if self.database == DatabaseMode::Cnpg && self.cnpg.is_none() {
            return invalid(
                PROFILE_KIND,
                path,
                "database = \"cnpg\" requires a [cnpg] section",
            );
        }
        Ok(())
    }

    #[must_use]
    pub fn with_overrides(
        mut self,
        exposure: Option<Exposure>,
        database: Option<DatabaseMode>,
    ) -> Self {
        if let Some(exposure) = exposure {
            self.exposure = exposure;
        }
        if let Some(database) = database {
            self.database = database;
        }
        self
    }
}

impl CnpgProfile {
    fn validate(&self, path: &Path) -> Result<()> {
        if self.host.trim().is_empty() {
            return invalid(PROFILE_KIND, path, "cnpg.host must not be empty");
        }
        if self.port == 0 {
            return invalid(PROFILE_KIND, path, "cnpg.port must not be zero");
        }
        validate_state_scope(&self.state_scope).map_err(|message| Error::InvalidConfig {
            kind: PROFILE_KIND,
            path: path.to_owned(),
            message,
        })?;
        validate_credential_template(&self.credential_ref_template).map_err(|message| {
            Error::InvalidConfig {
                kind: PROFILE_KIND,
                path: path.to_owned(),
                message,
            }
        })?;
        if self.session_aead_key_ref.trim().is_empty() {
            return invalid(
                PROFILE_KIND,
                path,
                "cnpg.session_aead_key_ref must not be empty",
            );
        }
        Ok(())
    }
}

impl AppManifest {
    #[allow(clippy::too_many_lines)] // Keep untrusted manifest validation in one audit boundary.
    pub fn validate(&self, path: &Path) -> Result<()> {
        if self.schema != 1 {
            return invalid(
                MANIFEST_KIND,
                path,
                format!("unsupported schema {}; expected 1", self.schema),
            );
        }
        for (label, port) in [
            ("web.local_port", self.web.local_port),
            ("web.tailscale_port", self.web.tailscale_port),
            ("web.backend_port", self.web.backend_port),
        ] {
            if port == 0 {
                return invalid(MANIFEST_KIND, path, format!("{label} must not be zero"));
            }
        }
        if !self.web.login_path.starts_with('/') || self.web.login_path.starts_with("//") {
            return invalid(
                MANIFEST_KIND,
                path,
                "web.login_path must be an absolute, non-protocol-relative path",
            );
        }
        validate_database_name(&self.database.name)
            .map_err(|message| invalid_error(MANIFEST_KIND, path, message))?;
        match self.database.migration_mode {
            MigrationMode::ApplicationStartup if !self.database.migration_command.is_empty() => {
                return invalid(
                    MANIFEST_KIND,
                    path,
                    "application-startup migration mode must not declare migration_command",
                );
            }
            MigrationMode::Command if self.database.migration_command.is_empty() => {
                return invalid(
                    MANIFEST_KIND,
                    path,
                    "command migration mode requires migration_command",
                );
            }
            _ => {}
        }

        let mut process_names = BTreeSet::new();
        for process in &self.processes {
            if !process_names.insert(&process.name) {
                return invalid(
                    MANIFEST_KIND,
                    path,
                    format!("duplicate process name {:?}", process.name),
                );
            }
            validate_process_name(&process.name)
                .map_err(|message| invalid_error(MANIFEST_KIND, path, message))?;
            validate_command(&process.command, "process command")
                .map_err(|message| invalid_error(MANIFEST_KIND, path, message))?;
            if let Some(cwd) = process.cwd.as_deref() {
                validate_relative_path(cwd, "process cwd")
                    .map_err(|message| invalid_error(MANIFEST_KIND, path, message))?;
            }
            validate_environment(&process.env, &process.resolved_env)
                .map_err(|message| invalid_error(MANIFEST_KIND, path, message))?;
        }
        if self.processes.is_empty() {
            return invalid(MANIFEST_KIND, path, "at least one process is required");
        }
        let web_process_name = self.name.web_process_name();
        let web_process = self
            .processes
            .iter()
            .find(|process| process.name == web_process_name)
            .ok_or_else(|| {
                invalid_error(
                    MANIFEST_KIND,
                    path,
                    format!(
                        "{} manifest requires a {web_process_name:?} process",
                        self.name
                    ),
                )
            })?;
        if web_process
            .resolved_env
            .get(fleet_auth::ENV_SESSION_AEAD_KEY)
            .map(String::as_str)
            != Some("fleet.session_aead_key")
        {
            return invalid(
                MANIFEST_KIND,
                path,
                format!(
                    "process {web_process_name:?} must map {} = \"fleet.session_aead_key\"",
                    fleet_auth::ENV_SESSION_AEAD_KEY
                ),
            );
        }
        let web_ui = self
            .processes
            .iter()
            .find(|process| process.name == WEB_UI_PROCESS)
            .ok_or_else(|| {
                invalid_error(
                    MANIFEST_KIND,
                    path,
                    format!(
                        "{} manifest requires a {WEB_UI_PROCESS:?} process",
                        self.name
                    ),
                )
            })?;
        if web_ui.command.first().map(String::as_str) != Some("trunk") {
            return invalid(
                MANIFEST_KIND,
                path,
                "process \"web-ui\" must use the controller-parameterized Trunk convention",
            );
        }
        if let Some(resolver) = &self.resolver {
            validate_command(&resolver.command, "resolver command")
                .map_err(|message| invalid_error(MANIFEST_KIND, path, message))?;
            if let Some(cwd) = resolver.cwd.as_deref() {
                validate_relative_path(cwd, "resolver cwd")
                    .map_err(|message| invalid_error(MANIFEST_KIND, path, message))?;
            }
            validate_environment(&resolver.env, &resolver.resolved_env)
                .map_err(|message| invalid_error(MANIFEST_KIND, path, message))?;
        }
        if let Some(preparation) = &self.preparation {
            validate_command(&preparation.command, "preparation command")
                .map_err(|message| invalid_error(MANIFEST_KIND, path, message))?;
            if let Some(cwd) = preparation.cwd.as_deref() {
                validate_relative_path(cwd, "preparation cwd")
                    .map_err(|message| invalid_error(MANIFEST_KIND, path, message))?;
            }
            validate_environment(&preparation.env, &preparation.resolved_env)
                .map_err(|message| invalid_error(MANIFEST_KIND, path, message))?;
        }
        validate_environment(&self.migration.env, &self.migration.resolved_env)
            .map_err(|message| invalid_error(MANIFEST_KIND, path, message))?;
        let mut declared_permissions = BTreeSet::new();
        for permission in &self.auth.permissions {
            if !declared_permissions.insert(permission.as_str()) {
                return invalid(
                    MANIFEST_KIND,
                    path,
                    format!("duplicate permission {permission:?}"),
                );
            }
            let Some((app, name)) = permission.split_once(':') else {
                return invalid(
                    MANIFEST_KIND,
                    path,
                    format!("permission {permission:?} must be APP:PERMISSION"),
                );
            };
            fleet_auth::validate_app_namespace(app)
                .map_err(|error| invalid_error(MANIFEST_KIND, path, error.to_string()))?;
            fleet_auth::validate_permission(name)
                .map_err(|error| invalid_error(MANIFEST_KIND, path, error.to_string()))?;
            if app != self.name.as_str() {
                return invalid(
                    MANIFEST_KIND,
                    path,
                    format!(
                        "permission {permission:?} belongs to {app:?}, not manifest app {:?}",
                        self.name.as_str()
                    ),
                );
            }
        }
        if self.name == App::Trawl {
            let expected: BTreeSet<_> = TRAWL_DEV_PERMISSIONS.into_iter().collect();
            if declared_permissions != expected {
                return invalid(
                    MANIFEST_KIND,
                    path,
                    "Trawl auth.permissions must exactly match the frozen development set",
                );
            }
        }
        Ok(())
    }
}

#[must_use]
pub fn expand_tilde(path: &Path) -> PathBuf {
    PathBuf::from(shellexpand::tilde(&path.to_string_lossy()).into_owned())
}

pub fn validate_state_scope(scope: &str) -> std::result::Result<(), String> {
    if scope == "." || scope == ".." {
        return Err("cnpg.state_scope must explicitly reject `.` and `..`".to_owned());
    }
    if scope.is_empty() || scope.len() > 64 {
        return Err("cnpg.state_scope must contain 1..=64 ASCII characters".to_owned());
    }
    let mut chars = scope.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphanumeric()) {
        return Err("cnpg.state_scope must start with an ASCII letter or digit".to_owned());
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        return Err(
            "cnpg.state_scope may contain only ASCII letters, digits, `.`, `_`, and `-`".to_owned(),
        );
    }
    Ok(())
}

fn validate_credential_template(template: &str) -> std::result::Result<(), String> {
    if template.matches("{database}").count() != 1 || template.matches("{field}").count() != 1 {
        return Err(
            "cnpg.credential_ref_template must contain `{database}` and `{field}` exactly once"
                .to_owned(),
        );
    }
    let stripped = template.replace("{database}", "").replace("{field}", "");
    if stripped.contains('{') || stripped.contains('}') {
        return Err("cnpg.credential_ref_template contains an unsupported placeholder".to_owned());
    }
    Ok(())
}

fn validate_database_name(name: &str) -> std::result::Result<(), String> {
    let mut chars = name.chars();
    if !chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        || !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        || name.len() > 63
    {
        return Err(format!(
            "database name {name:?} must be a lowercase PostgreSQL identifier"
        ));
    }
    Ok(())
}

fn validate_hostname(host: &str, label: &str) -> std::result::Result<(), String> {
    let host = host.trim_end_matches('.');
    if host.is_empty()
        || host.len() > 253
        || !host.is_ascii()
        || host.contains(['/', ':'])
        || host.chars().any(char::is_whitespace)
        || host.split('.').any(|part| {
            part.is_empty()
                || part.len() > 63
                || part.starts_with('-')
                || part.ends_with('-')
                || !part
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        })
    {
        return Err(format!(
            "{label} {host:?} must be an ASCII DNS hostname or IPv4 address without a port"
        ));
    }
    Ok(())
}

fn validate_process_name(name: &str) -> std::result::Result<(), String> {
    if name == "login" {
        return Err(
            "process name \"login\" is reserved for the controller information pane".to_owned(),
        );
    }
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(format!(
            "process name {name:?} may contain only ASCII letters, digits, `-`, and `_`"
        ));
    }
    Ok(())
}

fn validate_command(command: &[String], label: &str) -> std::result::Result<(), String> {
    if command.is_empty() || command.iter().any(String::is_empty) {
        return Err(format!("{label} must contain non-empty argv entries"));
    }
    Ok(())
}

fn validate_relative_path(path: &Path, label: &str) -> std::result::Result<(), String> {
    use std::path::Component;
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(format!(
            "{label} {} must be a non-empty repository-relative path without `..`",
            path.display()
        ));
    }
    Ok(())
}

fn validate_environment(
    plain: &BTreeMap<String, String>,
    resolved: &BTreeMap<String, String>,
) -> std::result::Result<(), String> {
    for name in plain.keys().chain(resolved.keys()) {
        validate_env_name(name)?;
        if name == OP_SERVICE_ACCOUNT_TOKEN {
            return Err(format!(
                "{OP_SERVICE_ACCOUNT_TOKEN} is controller-owned and cannot be declared by a manifest"
            ));
        }
        if matches!(
            name.as_str(),
            "FLEET_DEV_BROWSER_ORIGIN"
                | "FLEET_DEV_LOGIN_URL"
                | "FLEET_DEV_BACKEND_AUTHORITY"
                | "FLEET_DEV_API_BIND"
                | "TRAWL_WEB_BIND_ADDR"
                | "COASTWATCH_WEB_ADDR"
                | fleet_auth::ENV_SESSION_COOKIE_DOMAIN
                | fleet_auth::ENV_SESSION_COOKIE_PATH
                | fleet_auth::ENV_SESSION_COOKIE_SECURE
        ) {
            return Err(format!(
                "{name} is topology-owned and cannot be declared by a manifest"
            ));
        }
    }
    if let Some(duplicate) = plain.keys().find(|name| resolved.contains_key(*name)) {
        return Err(format!(
            "environment destination {duplicate:?} is declared as both static and resolved"
        ));
    }
    for source in resolved.values() {
        let leaf = source
            .strip_prefix("fleet.")
            .or_else(|| source.strip_prefix("app."));
        if !leaf.is_some_and(|leaf| {
            !leaf.is_empty()
                && leaf.chars().all(|character| {
                    character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
                })
        }) {
            return Err(format!(
                "resolved source {source:?} must be a `fleet.` or `app.` lowercase logical name"
            ));
        }
    }
    Ok(())
}

fn validate_env_name(name: &str) -> std::result::Result<(), String> {
    let mut chars = name.chars();
    if !chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        || !chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(format!(
            "environment name {name:?} must match `[A-Z_][A-Z0-9_]*`"
        ));
    }
    Ok(())
}

fn invalid<T>(kind: &'static str, path: &Path, message: impl Into<String>) -> Result<T> {
    Err(invalid_error(kind, path, message))
}

fn invalid_error(kind: &'static str, path: &Path, message: impl Into<String>) -> Error {
    Error::InvalidConfig {
        kind,
        path: path.to_owned(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_state_scopes() {
        for good in ["cnpg", "docker", "fractal.dev_1", "a", "A-9"] {
            assert!(validate_state_scope(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "../escape",
            "with/slash",
            "with space",
            "é",
            &"a".repeat(65),
        ] {
            assert!(validate_state_scope(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn credential_template_is_frozen() {
        assert!(validate_credential_template("op://Vault/CNPG {database}/{field}").is_ok());
        for bad in [
            "op://Vault/item",
            "op://Vault/{database}",
            "op://Vault/{database}/{field}/{field}",
            "op://Vault/{database}/{field}/{other}",
        ] {
            assert!(validate_credential_template(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn portable_profile_defaults() {
        let profile = MachineProfile::default();
        assert_eq!(profile.exposure, Exposure::Localhost);
        assert_eq!(profile.database, DatabaseMode::Docker);
        assert_eq!(profile.host, "localhost");
    }

    #[test]
    fn invocation_overrides_win_over_profile_values() {
        let profile = MachineProfile {
            exposure: Exposure::Tailscale,
            database: DatabaseMode::Cnpg,
            ..MachineProfile::default()
        };
        let profile = profile.with_overrides(Some(Exposure::Localhost), Some(DatabaseMode::Docker));
        assert_eq!(profile.exposure, Exposure::Localhost);
        assert_eq!(profile.database, DatabaseMode::Docker);
    }

    #[test]
    fn docker_override_can_rescue_a_cnpg_profile_without_cnpg_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("dev.toml");
        std::fs::write(&path, "database = \"cnpg\"\n").unwrap();
        let (profile, loaded_path) = load_profile(Some(&path)).unwrap();
        let profile = profile.with_overrides(None, Some(DatabaseMode::Docker));
        profile.validate(&loaded_path).unwrap();
    }

    #[test]
    fn login_process_name_is_reserved() {
        assert!(validate_process_name("login").is_err());
        assert!(validate_process_name("web-login").is_ok());
    }

    #[test]
    fn profile_rejects_unknown_fields() {
        let error = toml::from_str::<MachineProfile>(
            r#"
                exposure = "localhost"
                database = "docker"
                surprise = true
            "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn consumer_rejects_duplicate_destinations() {
        let plain = BTreeMap::from([("DATABASE_URL".to_owned(), "literal".to_owned())]);
        let resolved = BTreeMap::from([("DATABASE_URL".to_owned(), "app.database_url".to_owned())]);
        assert!(validate_environment(&plain, &resolved).is_err());
    }

    #[test]
    fn manifest_paths_cannot_escape_checkout() {
        assert!(validate_relative_path(Path::new("crates/web-ui"), "cwd").is_ok());
        assert!(validate_relative_path(Path::new("../other"), "cwd").is_err());
        assert!(validate_relative_path(Path::new("/tmp"), "cwd").is_err());
    }

    #[test]
    fn manifest_cannot_shadow_the_controller_token_or_use_malformed_sources() {
        let token = BTreeMap::from([(
            OP_SERVICE_ACCOUNT_TOKEN.to_owned(),
            "not-even-a-real-token".to_owned(),
        )]);
        assert!(validate_environment(&token, &BTreeMap::new()).is_err());

        for source in ["fleet.", "app.UPPER", "other.secret", "app.key-name"] {
            let resolved = BTreeMap::from([("API_KEY".to_owned(), source.to_owned())]);
            assert!(
                validate_environment(&BTreeMap::new(), &resolved).is_err(),
                "{source}"
            );
        }
    }

    #[test]
    fn manifest_requires_shared_session_key_and_parameterized_web_ui() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .unwrap();
        let mut manifest = load_manifest(root).unwrap();
        manifest
            .processes
            .iter_mut()
            .find(|process| process.name == "trawl-web")
            .unwrap()
            .resolved_env
            .remove(fleet_auth::ENV_SESSION_AEAD_KEY);
        assert!(manifest.validate(Path::new("fleet-dev.toml")).is_err());

        let mut manifest = load_manifest(root).unwrap();
        manifest
            .processes
            .retain(|process| process.name != "web-ui");
        assert!(manifest.validate(Path::new("fleet-dev.toml")).is_err());
    }

    #[test]
    fn machine_host_is_a_bare_hostname() {
        for host in ["localhost", "127.0.0.1", "fractal.local", "node.example."] {
            assert!(validate_hostname(host, "host").is_ok(), "{host}");
        }
        for host in [
            "",
            "http://localhost",
            "localhost:8081",
            "-bad",
            "bad..host",
        ] {
            assert!(validate_hostname(host, "host").is_err(), "{host}");
        }
    }
}
