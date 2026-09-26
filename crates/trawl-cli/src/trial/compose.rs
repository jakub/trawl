// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The trial's Compose project and the files its containers read, as pure
//! renderers.
//!
//! [`render_compose`] turns the trial state into a Compose file. Compose
//! reads JSON, so the file is a [`serde_json::Value`] and no YAML writer is
//! needed. The output is deterministic and holds no secret, so `up`
//! rewrites it on every run and a snapshot test pins it.
//!
//! Services:
//!
//! - `postgres`: the recorded PostgreSQL image. Its superuser password is a
//!   0400 file in its own volume, named by `POSTGRES_PASSWORD_FILE`. No
//!   published port.
//! - `trawld`: the trial image with [`trawld_toml`], its data, the pgpass
//!   file, and the TLS pair in the `trawld` volume. `PGPASSFILE` is the
//!   only environment entry, and it is a path.
//! - `trawl-web`: the same image running `trawl-web` with [`web_toml`], the
//!   cookie key, and a copy of the public certificate in the `web` volume.
//!   It pins that certificate for its upstream.
//! - `fleet-admin` and `tls-init` (profile `init`): one-shot steps run with
//!   `compose run`. `fleet-admin` connects with a passwordless URL and the
//!   pgpass file, and its container keeps no log, because `keys create`
//!   prints a token.
//!
//! Every service, the network, and every volume carry the trial id label.
//! Every published port binds 127.0.0.1. No service has a restart policy,
//! and no image is pulled by Compose: `up` pulls and records images itself.
//!
//! Every `image:` is the recorded image id (`sha256:…`), never the
//! reference it was resolved from. A tag can move after `up` checked it,
//! through a concurrent `docker pull` or a rebuilt `--image`, and a later
//! Compose run would then mount the trial's volumes into another image.
//! An id cannot move.
//!
//! Compose interpolates `$` in every string of the file, so the renderer
//! writes each `$` as `$$` ([`escape_interpolation`]). Nothing in the
//! trial's file is meant to be interpolated.

use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::secrets::DbPasswords;
use super::state::TrialState;
use super::{LABEL_ID, PROJECT};

/// The rendered file in the trial directory.
pub const COMPOSE_FILE_NAME: &str = "compose.json";

/// The profile that keeps the one-shot services out of `compose up`.
pub const INIT_PROFILE: &str = "init";

/// trawld's HTTPS port inside its container.
pub const API_CONTAINER_PORT: u16 = 5514;

/// trawl-web's HTTP port inside its container.
pub const WEB_CONTAINER_PORT: u16 = 8090;

/// The PostgreSQL service's network name and port, as the DSNs spell them.
const DB_HOST: &str = "postgres";
const DB_PORT: u16 = 5432;

/// Mount point of the `postgres` volume: the `postgres:18` image declares
/// this directory as its volume and keeps `PGDATA` below it, so mounting it
/// explicitly leaves Docker no anonymous volume to create.
pub const PG_VOLUME_ROOT: &str = "/var/lib/postgresql";
/// The superuser password, 0400, owned by `postgres`.
pub const PG_SUPERUSER_PASSWORD: &str = "/var/lib/postgresql/trial/superuser.password";

/// Mount point of the `trawld` and `web` volumes: the image's `trawl`
/// home, the one directory that user owns (`/etc/trawl` is root's).
pub const TRAWL_VOLUME_ROOT: &str = "/var/lib/trawl";
/// trawld's config, 0400, owned by `trawl`. Holds no secret.
pub const TRAWLD_TOML: &str = "/var/lib/trawl/trial/trawld.toml";
/// Both database roles' passwords, 0400, owned by `trawl`.
pub const PGPASS: &str = "/var/lib/trawl/trial/secrets/pgpass";
/// Where `tls-init` publishes the certificate and key.
#[cfg(test)]
pub const TLS_DIR: &str = "/var/lib/trawl/trial/tls";
pub const TLS_CERT: &str = "/var/lib/trawl/trial/tls/cert.pem";
pub const TLS_KEY: &str = "/var/lib/trawl/trial/tls/key.pem";
/// trawld's parquet data.
pub const DATA_DIR: &str = "/var/lib/trawl/data";

/// trawl-web's config, 0400, owned by `trawl`, in the `web` volume.
pub const WEB_TOML: &str = "/var/lib/trawl/trial/web.toml";
/// The cookie AEAD key: exactly 32 raw bytes, the format
/// `[web] cookie_secret_path` reads.
pub const WEB_COOKIE: &str = "/var/lib/trawl/trial/web.cookie";
/// The public certificate trawl-web pins for its upstream.
pub const WEB_CA: &str = "/var/lib/trawl/trial/ca.pem";

/// The trial's services.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Postgres,
    Trawld,
    Web,
    FleetAdmin,
    TlsInit,
}

impl Service {
    #[cfg(test)]
    pub const ALL: [Self; 5] = [
        Self::Postgres,
        Self::Trawld,
        Self::Web,
        Self::FleetAdmin,
        Self::TlsInit,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Trawld => "trawld",
            Self::Web => "trawl-web",
            Self::FleetAdmin => "fleet-admin",
            Self::TlsInit => "tls-init",
        }
    }
}

/// The named volumes, keyed as the Compose file keys them. Docker names
/// them `trawl-trial_<key>`.
pub const VOLUMES: [&str; 3] = ["postgres", "trawld", "web"];

/// The `fleet` role's passwordless DSN; the password comes from pgpass.
pub fn fleet_database_url() -> String {
    format!("postgres://fleet@{DB_HOST}:{DB_PORT}/fleet")
}

/// The `trawl` role's passwordless DSN; the password comes from pgpass.
pub fn trawl_database_url() -> String {
    format!("postgres://trawl@{DB_HOST}:{DB_PORT}/trawl")
}

/// `tls-init`: generate the trial certificate unless one is published.
///
/// The pair is written into a fresh directory, the key is made 0400, and
/// the directory is renamed into place, so `tls/` either holds a complete
/// pair or does not exist. `trawl-admin` always adds `localhost`,
/// `127.0.0.1`, and `::1`; the SANs are still listed so the certificate's
/// contract reads here.
const TLS_INIT: &str = r#"set -eu
umask 077
d=/var/lib/trawl/trial/tls
if [ -e "$d/cert.pem" ]; then exit 0; fi
t="$d.new"
rm -rf -- "$t"
trawl-admin tls generate --output-dir "$t" --san trawld --san localhost --san 127.0.0.1
chmod 0400 "$t/key.pem"
mv -T -- "$t" "$d""#;

/// Render the Compose project for `state`.
pub fn render_compose(state: &TrialState) -> Value {
    let labels = json!({ LABEL_ID: state.trial_id });
    let trawl = state.images.trawl.id.as_str();
    let service = |image: &str, settings: Value| {
        let mut service = json!({
            "image": image,
            "pull_policy": "never",
            "labels": labels,
        });
        let map = service.as_object_mut().expect("an object");
        map.extend(settings.as_object().expect("an object").clone());
        service
    };

    let services = json!({
        Service::Postgres.name(): service(&state.images.postgres.id, json!({
            "environment": { "POSTGRES_PASSWORD_FILE": PG_SUPERUSER_PASSWORD },
            "volumes": [volume("postgres", PG_VOLUME_ROOT, false)],
            // Over TCP, so the init-time server, which listens on the
            // socket only, never reads as ready.
            "healthcheck": healthcheck(json!([
                "CMD", "pg_isready", "--host", "127.0.0.1", "--port", DB_PORT.to_string(),
                "--username", "postgres", "--dbname", "postgres",
            ])),
        })),
        Service::Trawld.name(): service(trawl, json!({
            "command": ["--config", TRAWLD_TOML, "--no-monitor"],
            "environment": { "PGPASSFILE": PGPASS },
            "volumes": [volume("trawld", TRAWL_VOLUME_ROOT, false)],
            "ports": published(state.ports.api, API_CONTAINER_PORT),
            "healthcheck": tcp_probe(API_CONTAINER_PORT),
            "depends_on": { Service::Postgres.name(): { "condition": "service_healthy" } },
        })),
        Service::Web.name(): service(trawl, json!({
            "entrypoint": ["trawl-web"],
            "command": ["--config", WEB_TOML],
            "volumes": [volume("web", TRAWL_VOLUME_ROOT, false)],
            "ports": published(state.ports.web, WEB_CONTAINER_PORT),
            "healthcheck": tcp_probe(WEB_CONTAINER_PORT),
            "depends_on": { Service::Trawld.name(): { "condition": "service_healthy" } },
        })),
        Service::FleetAdmin.name(): service(trawl, json!({
            "profiles": [INIT_PROFILE],
            "entrypoint": ["fleet-admin"],
            "environment": {
                "DATABASE_URL": fleet_database_url(),
                "PGPASSFILE": PGPASS,
            },
            "volumes": [volume("trawld", TRAWL_VOLUME_ROOT, true)],
            // `keys create` prints a token; no container log may keep it.
            "logging": { "driver": "none" },
        })),
        Service::TlsInit.name(): service(trawl, json!({
            "profiles": [INIT_PROFILE],
            "entrypoint": ["/bin/sh", "-c", TLS_INIT],
            "volumes": [volume("trawld", TRAWL_VOLUME_ROOT, false)],
            "network_mode": "none",
        })),
    });

    let volumes: serde_json::Map<String, Value> = VOLUMES
        .iter()
        .map(|name| ((*name).to_owned(), json!({ "labels": labels })))
        .collect();

    let mut project = json!({
        "name": PROJECT,
        "services": services,
        "networks": { "default": { "labels": labels } },
        "volumes": volumes,
    });
    escape_interpolation(&mut project);
    project
}

fn healthcheck(test: Value) -> Value {
    let mut check = json!({ "interval": "2s", "timeout": "5s", "retries": 90 });
    check["test"] = test;
    check
}

/// A healthcheck that connects to `port` with bash's `/dev/tcp`: the image
/// ships no HTTP client.
fn tcp_probe(port: u16) -> Value {
    healthcheck(json!([
        "CMD",
        "bash",
        "-c",
        format!("exec 3<>/dev/tcp/127.0.0.1/{port}"),
    ]))
}

fn volume(source: &str, target: &str, read_only: bool) -> Value {
    let mut mount = json!({ "type": "volume", "source": source, "target": target });
    if read_only {
        mount["read_only"] = true.into();
    }
    mount
}

/// One loopback-only published port.
fn published(host_port: u16, container_port: u16) -> Value {
    json!([{
        "target": container_port,
        "published": host_port.to_string(),
        "host_ip": "127.0.0.1",
        "protocol": "tcp",
    }])
}

/// Double every `$` in every string, key or value, so Compose reads it
/// literally.
fn escape_interpolation(value: &mut Value) {
    match value {
        Value::String(s) if s.contains('$') => *s = s.replace('$', "$$"),
        Value::Array(items) => items.iter_mut().for_each(escape_interpolation),
        Value::Object(map) => {
            let entries = std::mem::take(map);
            for (key, mut inner) in entries {
                escape_interpolation(&mut inner);
                map.insert(key.replace('$', "$$"), inner);
            }
        }
        _ => {}
    }
}

/// trawld's config. It holds no secret: the DSNs carry no password, and
/// trawld reads the passwords from the pgpass file `PGPASSFILE` names.
pub fn trawld_toml() -> String {
    format!(
        r#"# Rendered by `trawl trial up`. Holds no secret: the database passwords
# are in the pgpass file that PGPASSFILE names.

[server]
http_addr = "0.0.0.0:{API_CONTAINER_PORT}"
tls_cert_path = "{TLS_CERT}"
tls_key_path = "{TLS_KEY}"

[data]
path = "{DATA_DIR}"

[auth]
database_url = "{fleet}"

[storage]
database_url = "{trawl}"

[syslog]
enabled = false
"#,
        fleet = fleet_database_url(),
        trawl = trawl_database_url(),
    )
}

/// trawl-web's config. `[server]` and `[data]` are there because the
/// shared schema requires them; trawl-web reads only `[web]`.
pub fn web_toml(state: &TrialState) -> String {
    let web = state.ports.web;
    format!(
        r#"# Rendered by `trawl trial up`. Holds no secret: the cookie key is in
# the file cookie_secret_path names.

[server]

[data]
path = "{DATA_DIR}"

[web]
bind_addr = "0.0.0.0:{WEB_CONTAINER_PORT}"
upstream_url = "https://{trawld}:{API_CONTAINER_PORT}"
upstream_ca_path = "{WEB_CA}"
cookie_secret_path = "{WEB_COOKIE}"
public_origins = ["http://localhost:{web}", "http://127.0.0.1:{web}"]
# The browser leg is plain HTTP on loopback.
allow_insecure_cookies = true
"#,
        trawld = Service::Trawld.name(),
    )
}

/// SQL that creates the two owner roles and their databases, and sets the
/// roles' passwords. Idempotent: until the database phase is recorded,
/// every `up` generates new passwords and runs it again, so the roles and
/// the pgpass file converge.
///
/// It runs as the superuser through `psql` on stdin ([`database_sql_args`],
/// sealed). Statement logging is off for the session and an error never
/// logs its statement, so the passwords stay out of the server log.
pub fn database_sql(passwords: &DbPasswords) -> Zeroizing<String> {
    Zeroizing::new(format!(
        r"\set ON_ERROR_STOP on
SET log_statement = 'none';
SET log_min_duration_statement = -1;
SET log_min_error_statement = panic;
DO $trial$
BEGIN
  IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = 'fleet') THEN
    CREATE ROLE fleet LOGIN;
  END IF;
  IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = 'trawl') THEN
    CREATE ROLE trawl LOGIN;
  END IF;
END
$trial$;
ALTER ROLE fleet WITH LOGIN NOSUPERUSER PASSWORD '{fleet}';
ALTER ROLE trawl WITH LOGIN NOSUPERUSER PASSWORD '{trawl}';
SELECT 'CREATE DATABASE fleet OWNER fleet'
  WHERE NOT EXISTS (SELECT FROM pg_catalog.pg_database WHERE datname = 'fleet')\gexec
SELECT 'CREATE DATABASE trawl OWNER trawl'
  WHERE NOT EXISTS (SELECT FROM pg_catalog.pg_database WHERE datname = 'trawl')\gexec
DO $trial$
BEGIN
  IF EXISTS (
    SELECT FROM pg_catalog.pg_database d
      JOIN pg_catalog.pg_roles r ON r.oid = d.datdba
     WHERE d.datname IN ('fleet', 'trawl') AND r.rolname <> d.datname
  ) THEN
    RAISE EXCEPTION 'a trial database is not owned by its own role';
  END IF;
END
$trial$;
",
        fleet = passwords.fleet.expose(),
        trawl = passwords.trawl.expose(),
    ))
}

/// `compose exec` of `psql` as the superuser over the local socket,
/// reading [`database_sql`] on stdin. Run it sealed: `psql` quotes a
/// failing statement.
pub fn database_sql_args() -> super::docker::Args {
    super::docker::Args::new().args([
        "exec",
        "-T",
        "--user",
        "postgres",
        Service::Postgres.name(),
        "psql",
        "--no-psqlrc",
        "--quiet",
        "--set",
        "ON_ERROR_STOP=1",
        "--dbname",
        "postgres",
    ])
}

/// The pgpass file both trawld and `fleet-admin` read: one line per role,
/// matching the DSNs exactly.
pub fn pgpass(passwords: &DbPasswords) -> Zeroizing<String> {
    Zeroizing::new(format!(
        "{DB_HOST}:{DB_PORT}:fleet:fleet:{fleet}\n{DB_HOST}:{DB_PORT}:trawl:trawl:{trawl}\n",
        fleet = passwords.fleet.expose(),
        trawl = passwords.trawl.expose(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trial::secrets::HexSecret;
    use crate::trial::state::tests::fixture;

    fn state() -> TrialState {
        fixture(crate::trial::DEFAULT_API_PORT)
    }

    fn passwords() -> DbPasswords {
        DbPasswords {
            fleet: HexSecret::generate(),
            trawl: HexSecret::generate(),
        }
    }

    #[test]
    fn the_rendered_project() {
        let text = serde_json::to_string_pretty(&render_compose(&state())).unwrap();
        insta::assert_snapshot!(text);
    }

    #[test]
    fn the_rendered_configs() {
        insta::assert_snapshot!("trawld_toml", trawld_toml());
        insta::assert_snapshot!("web_toml", web_toml(&state()));
    }

    /// Walk every value and key of the rendered project.
    fn walk<'a>(value: &'a Value, path: &str, visit: &mut impl FnMut(&str, &'a Value)) {
        visit(path, value);
        match value {
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    walk(item, &format!("{path}[{i}]"), visit);
                }
            }
            Value::Object(map) => {
                for (key, inner) in map {
                    walk(inner, &format!("{path}.{key}"), visit);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn every_resource_carries_the_trial_label() {
        let state = state();
        let project = render_compose(&state);
        let label = |v: &Value| v["labels"][LABEL_ID].as_str().map(str::to_owned);

        let services = project["services"].as_object().unwrap();
        assert_eq!(
            services
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            Service::ALL.map(Service::name).into_iter().collect(),
        );
        for (name, service) in services {
            assert_eq!(
                label(service).as_deref(),
                Some(state.trial_id.as_str()),
                "service {name}"
            );
        }
        let networks = project["networks"].as_object().unwrap();
        assert_eq!(networks.len(), 1);
        assert_eq!(
            label(&networks["default"]).as_deref(),
            Some(state.trial_id.as_str())
        );
        let volumes = project["volumes"].as_object().unwrap();
        assert_eq!(volumes.len(), VOLUMES.len());
        for (name, volume) in volumes {
            assert_eq!(
                label(volume).as_deref(),
                Some(state.trial_id.as_str()),
                "volume {name}"
            );
        }
        // Every mount is one of the labelled named volumes: no bind mount
        // and no anonymous volume.
        walk(&project, "", &mut |path, value| {
            if path.ends_with(".volumes") && path.starts_with(".services") {
                for mount in value.as_array().unwrap() {
                    assert_eq!(mount["type"], "volume", "{path}");
                    assert!(
                        volumes.contains_key(mount["source"].as_str().unwrap()),
                        "{path}"
                    );
                }
            }
        });
        assert_eq!(project["name"], PROJECT);
    }

    #[test]
    fn only_loopback_publishes_and_postgres_publishes_nothing() {
        let project = render_compose(&state());
        let mut published = Vec::new();
        for (name, service) in project["services"].as_object().unwrap() {
            assert!(service.get("expose").is_none(), "{name}");
            let Some(ports) = service.get("ports") else {
                continue;
            };
            for port in ports.as_array().unwrap() {
                assert_eq!(port["host_ip"], "127.0.0.1", "{name}");
                published.push((
                    name.clone(),
                    port["published"].clone(),
                    port["target"].clone(),
                ));
            }
        }
        published.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            published,
            [
                ("trawl-web".to_owned(), json!("18090"), json!(8090)),
                ("trawld".to_owned(), json!("15514"), json!(5514)),
            ]
        );
    }

    #[test]
    fn nothing_restarts_pulls_or_skips_verification() {
        let project = render_compose(&state());
        walk(&project, "", &mut |path, value| {
            let key = path.rsplit('.').next().unwrap_or_default();
            assert_ne!(key, "restart", "{path}");
            assert!(
                !key.to_ascii_lowercase().contains("insecure"),
                "{path} is an insecure setting"
            );
            if let Value::String(s) = value {
                assert!(!s.to_ascii_lowercase().contains("insecure"), "{path}: {s}");
                assert!(!s.contains("TRAWL_WEB_INSECURE_UPSTREAM"), "{path}");
            }
        });
        for (name, service) in project["services"].as_object().unwrap() {
            assert_eq!(service["pull_policy"], "never", "{name}");
            assert!(service.get("privileged").is_none(), "{name}");
        }
    }

    /// Every service, the `init` profile's included, runs the recorded
    /// image id, so a tag that moves after `up` checked it changes nothing.
    #[test]
    fn every_service_runs_its_recorded_image_id() {
        let state = state();
        let project = render_compose(&state);
        let services = project["services"].as_object().unwrap();
        assert_eq!(services.len(), Service::ALL.len());
        for service in Service::ALL {
            let recorded = match service {
                Service::Postgres => &state.images.postgres,
                Service::Trawld | Service::Web | Service::FleetAdmin | Service::TlsInit => {
                    &state.images.trawl
                }
            };
            let image = &services[service.name()]["image"];
            assert_eq!(image, recorded.id.as_str(), "{}", service.name());
            assert_ne!(image, recorded.reference.as_str(), "{}", service.name());
        }
    }

    /// The only environment entries are paths and passwordless URLs.
    #[test]
    fn no_environment_entry_holds_a_secret() {
        let project = render_compose(&state());
        let mut seen = Vec::new();
        for (name, service) in project["services"].as_object().unwrap() {
            if let Some(env) = service.get("environment") {
                for (key, value) in env.as_object().unwrap() {
                    seen.push(format!("{name}.{key}={}", value.as_str().unwrap()));
                }
            }
        }
        seen.sort();
        assert_eq!(
            seen,
            [
                "fleet-admin.DATABASE_URL=postgres://fleet@postgres:5432/fleet",
                "fleet-admin.PGPASSFILE=/var/lib/trawl/trial/secrets/pgpass",
                "postgres.POSTGRES_PASSWORD_FILE=/var/lib/postgresql/trial/superuser.password",
                "trawld.PGPASSFILE=/var/lib/trawl/trial/secrets/pgpass",
            ]
        );
    }

    #[test]
    fn no_generated_secret_reaches_a_rendered_file() {
        let passwords = passwords();
        let project = serde_json::to_string(&render_compose(&state())).unwrap();
        let rendered = [project, trawld_toml(), web_toml(&state())];
        // The payloads that do carry them, so the check is not vacuous.
        assert!(database_sql(&passwords).contains(passwords.fleet.expose()));
        assert!(pgpass(&passwords).contains(passwords.trawl.expose()));
        for text in &rendered {
            for secret in [passwords.fleet.expose(), passwords.trawl.expose()] {
                assert!(!text.contains(secret));
            }
        }
        // Every DSN names its user and no password.
        for url in [fleet_database_url(), trawl_database_url()] {
            let userinfo = url
                .strip_prefix("postgres://")
                .unwrap()
                .split_once('@')
                .unwrap()
                .0;
            assert!(!userinfo.contains(':'), "{url}");
        }
    }

    /// `allow_insecure_cookies` is the one setting that may say insecure:
    /// the browser leg is plain HTTP on loopback. Nothing may turn off
    /// certificate verification.
    #[test]
    fn the_configs_skip_no_verification() {
        for toml in [trawld_toml(), web_toml(&state())] {
            for line in toml.lines() {
                let lower = line.to_ascii_lowercase();
                if lower.contains("insecure") {
                    assert_eq!(line, "allow_insecure_cookies = true");
                }
                assert!(!line.contains("TRAWL_WEB_INSECURE_UPSTREAM"), "{line}");
            }
        }
    }

    #[test]
    fn dollars_are_escaped_everywhere() {
        let mut state = state();
        state.images.trawl.id = "sha256:${HOME}".into();
        let project = render_compose(&state);
        assert_eq!(project["services"]["trawld"]["image"], "sha256:$${HOME}");
        let script = project["services"]["tls-init"]["entrypoint"][2]
            .as_str()
            .unwrap();
        assert!(script.contains(r#"if [ -e "$$d/cert.pem" ]"#), "{script}");
        walk(&project, "", &mut |path, value| {
            if let Value::String(s) = value {
                assert!(!s.replace("$$", "").contains('$'), "{path}: {s}");
            }
        });
    }

    #[test]
    fn trawld_toml_parses_and_validates_through_trawl_config() {
        let config = trawl_config::Config::from_toml(&trawld_toml()).unwrap();
        assert_eq!(config.server.http_addr, "0.0.0.0:5514");
        assert_eq!(
            config.server.tls_cert_path.as_deref(),
            Some(std::path::Path::new(TLS_CERT))
        );
        assert_eq!(
            config.server.tls_key_path.as_deref(),
            Some(std::path::Path::new(TLS_KEY))
        );
        assert_eq!(config.data.path, DATA_DIR);
        assert_eq!(
            config.auth.database_url.as_deref(),
            Some(fleet_database_url().as_str())
        );
        assert_eq!(
            config.storage.database_url.as_deref(),
            Some(trawl_database_url().as_str())
        );
        assert!(!config.syslog.enabled);
        assert!(config.ingest.enabled);
    }

    #[test]
    fn web_toml_parses_through_trawl_config() {
        let config = trawl_config::Config::parse_toml(&web_toml(&state())).unwrap();
        let web = config.web;
        assert_eq!(web.bind_addr.as_deref(), Some("0.0.0.0:8090"));
        assert_eq!(web.upstream_url.as_deref(), Some("https://trawld:5514"));
        assert_eq!(
            web.upstream_ca_path.as_deref(),
            Some(std::path::Path::new(WEB_CA))
        );
        assert_eq!(
            web.cookie_secret_path.as_deref(),
            Some(std::path::Path::new(WEB_COOKIE))
        );
        assert_eq!(web.cookie_secret_env, None);
        assert!(web.allow_insecure_cookies);
        assert_eq!(
            web.public_origins,
            ["http://localhost:18090", "http://127.0.0.1:18090"]
        );
    }

    #[test]
    fn the_web_port_moves_the_origins_and_the_publish() {
        let mut state = state();
        state.ports.web = 28090;
        state.ports.api = 25514;
        let config = trawl_config::Config::parse_toml(&web_toml(&state)).unwrap();
        assert_eq!(
            config.web.public_origins,
            ["http://localhost:28090", "http://127.0.0.1:28090"]
        );
        let project = render_compose(&state);
        assert_eq!(
            project["services"]["trawl-web"]["ports"][0]["published"],
            "28090"
        );
        assert_eq!(
            project["services"]["trawld"]["ports"][0]["published"],
            "25514"
        );
    }

    #[test]
    fn pgpass_lines_match_the_dsns() {
        let passwords = passwords();
        let text = pgpass(&passwords);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                format!("postgres:5432:fleet:fleet:{}", passwords.fleet.expose()),
                format!("postgres:5432:trawl:trawl:{}", passwords.trawl.expose()),
            ]
        );
        for url in [fleet_database_url(), trawl_database_url()] {
            let rest = url.strip_prefix("postgres://").unwrap();
            let (user, rest) = rest.split_once('@').unwrap();
            let (host_port, db) = rest.split_once('/').unwrap();
            let prefix = format!("{host_port}:{db}:{user}:");
            assert!(lines.iter().any(|l| l.starts_with(&prefix)), "{url}");
        }
    }

    #[test]
    fn tls_init_publishes_the_pair_where_trawld_reads_it() {
        assert!(TLS_INIT.contains(&format!("d={TLS_DIR}\n")), "{TLS_INIT}");
        assert_eq!(TLS_CERT, format!("{TLS_DIR}/cert.pem"));
        assert_eq!(TLS_KEY, format!("{TLS_DIR}/key.pem"));
        for san in ["--san trawld", "--san localhost", "--san 127.0.0.1"] {
            assert!(TLS_INIT.contains(san), "{san}");
        }
        assert!(TLS_INIT.contains(r#"chmod 0400 "$t/key.pem""#));
    }

    #[test]
    fn database_sql_runs_as_the_superuser_over_the_socket() {
        assert_eq!(
            database_sql_args().display(),
            "docker exec -T --user postgres postgres psql --no-psqlrc --quiet \
             --set ON_ERROR_STOP=1 --dbname postgres"
        );
    }

    #[test]
    fn database_sql_quotes_nothing_but_hex() {
        let passwords = passwords();
        let sql = database_sql(&passwords);
        for secret in [passwords.fleet.expose(), passwords.trawl.expose()] {
            assert!(secret.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(sql.contains(&format!("PASSWORD '{secret}'")));
        }
        assert!(sql.starts_with("\\set ON_ERROR_STOP on\n"));
        let logging_off = sql.find("log_min_error_statement = panic").unwrap();
        assert!(logging_off < sql.find("PASSWORD").unwrap());
    }
}
