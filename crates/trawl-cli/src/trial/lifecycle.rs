// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The trial verbs: `up`, `status`, `key`, `stop`, and `down`.
//!
//! `up`, `stop`, and `down` start the same way: the preflight check pins
//! the Docker driver to the local engine, then the state root is created
//! and the lifecycle lock taken. Preflight runs first because it only
//! reads, and nothing, not even the state root, may be created before it
//! passes. Then the ownership scan refuses anything on the engine that
//! carries the trial's names or labels without this trial's id, and one-off
//! containers a killed command left running are waited for.
//!
//! `up` then creates or resumes, one recorded phase at a time, so a rerun
//! after an interruption skips what is done:
//!
//! 1. ports: a bind test on `127.0.0.1`, unless the trial's own container
//!    holds the port; Docker's own publish error is the backstop.
//! 2. images: pulled on the first `up` when absent, then recorded by id; a
//!    resume refuses another engine, another image id, or a changed flag.
//! 3. the state, then the engine claim, then `compose.json`.
//! 4. databases: the superuser password file, PostgreSQL, the two owner
//!    roles and databases, the pgpass file, both configs, and the cookie
//!    key. Every secret goes through [`put_file`] or sealed stdin.
//! 5. `fleet-admin migrate`, through the pgpass file.
//! 6. the certificate, copied to the host and to trawl-web's volume.
//! 7. the two keys: kept when the token file matches the recorded prefix,
//!    else every active key of that name is revoked and a new one minted.
//! 8. `compose up --wait trawld trawl-web`, then `whoami` for both keys
//!    through the pinned certificate, and one authenticated query.
//!
//! `status`, `key`, and `-p trial` take no lock: they read files that are
//! only ever replaced by an atomic rename.

use std::collections::BTreeSet;
use std::io::{self, BufRead, IsTerminal as _, Write};
use std::time::{Duration, Instant};

use trawl_client::{HttpClient, TlsTrust};
use zeroize::Zeroizing;

use super::compose::{self, Service};
use super::docker::{Args, Docker, DockerCli, PROBE_TIMEOUT, Sensitivity};
use super::keys::{self, MintedToken, TrialKey};
use super::lock::TrialLock;
use super::ownership::{self, Foreign, Inventory, Kind, Ownership, Resource};
use super::paths::{TrialPaths, read_private, read_public, write_private};
use super::preflight::{self, Engine};
use super::render::{self, Containers};
use super::secrets::{self, CookieKey, DbPasswords, HexSecret, Target, put_file};
use super::state::{
    DownView, ImageRecord, Images, KeyRecord, Phases, Ports, SCHEMA, Samples, TlsRecord, TrialState,
};
use super::{
    CLAIM_NAME, DEFAULT_API_PORT, DEFAULT_WEB_PORT, IMAGE_REPO, POSTGRES_IMAGE, PROJECT,
    TrialError, UpArgs,
};

/// A one-shot `compose run` step: `fleet-admin`, `tls-init`, a key.
const RUN_TIMEOUT: Duration = Duration::from_mins(3);
/// `compose up --wait`, whose own `--wait-timeout` is [`WAIT_SECS`].
const UP_TIMEOUT: Duration = Duration::from_mins(5);
const WAIT_SECS: &str = "180";
/// `docker pull` of an image the engine does not have.
const PULL_TIMEOUT: Duration = Duration::from_mins(15);
/// `compose stop`.
const STOP_TIMEOUT: Duration = Duration::from_mins(2);
/// How long `up`, `stop`, and `down` wait for a one-off container that an
/// interrupted command left running.
const ONEOFF_WAIT: Duration = Duration::from_mins(1);
/// How long the API may refuse connections after `compose up --wait`.
const API_WAIT: Duration = Duration::from_secs(30);

/// A token file holds the token and one newline.
const MAX_TOKEN_BYTES: u64 = 4096;
/// `ca.pem` holds one certificate.
const MAX_CA_BYTES: u64 = 64 * 1024;

/// The query `up` runs to prove an authenticated query works.
const PROOF_QUERY: &str = "* | head 1";

/// One progress line on stderr. stdout carries only the summary.
fn progress(message: impl std::fmt::Display) {
    eprintln!("trawl trial: {message}");
}

/// What `up`, `stop`, and `down` hold while they work.
struct Session {
    docker: Docker,
    engine: Engine,
    _lock: TrialLock,
}

/// Preflight, then the state root and the lock.
async fn begin(paths: &TrialPaths) -> Result<Session, TrialError> {
    let (engine, docker) = preflight::preflight(
        DockerCli::new(),
        &paths.dir,
        std::env::var_os("DOCKER_HOST"),
    )
    .await?;
    paths.ensure_root()?;
    let lock = TrialLock::acquire(&paths.lock, || {
        progress("waiting for another `trawl trial` command to finish");
    })?;
    Ok(Session {
        docker,
        engine,
        _lock: lock,
    })
}

/// The trial's state, when there is a trial directory holding one.
fn load_state(paths: &TrialPaths) -> Result<Option<TrialState>, TrialError> {
    if !paths.check_dir()? {
        return Ok(None);
    }
    TrialState::load(&paths.state_file())
}

/// The trial id from a state file of any schema.
fn load_down_view(paths: &TrialPaths) -> Result<Option<DownView>, TrialError> {
    if !paths.check_dir()? {
        return Ok(None);
    }
    DownView::load(&paths.state_file())
}

fn listing(resources: &[Foreign]) -> String {
    resources
        .iter()
        .map(|r| format!("  {r}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Refuse anything on the engine that is not this trial's. With no trial
/// id, anything in the union is an orphan.
fn require_owned(
    paths: &TrialPaths,
    inventory: &Inventory,
    our_id: Option<&str>,
) -> Result<(), TrialError> {
    match ownership::classify(inventory, our_id) {
        Ownership::Foreign(resources) if our_id.is_none() => Err(TrialError::Orphaned {
            dir: paths.dir.clone(),
            listing: listing(&resources),
        }),
        other => other.require_not_foreign().map(drop).map_err(Into::into),
    }
}

/// Wait for one-off containers an interrupted command left running, then
/// return a fresh inventory. Refuses, naming them, when they outlast
/// [`ONEOFF_WAIT`], and refuses anything foreign that appeared meanwhile.
async fn settle_oneoffs(
    docker: &Docker,
    paths: &TrialPaths,
    mut inventory: Inventory,
    our_id: &str,
) -> Result<Inventory, TrialError> {
    let deadline = Instant::now() + ONEOFF_WAIT;
    let mut told = false;
    loop {
        let pending: Vec<String> = ownership::unfinished_oneoffs(&inventory, our_id)
            .iter()
            .map(|r| r.name.clone())
            .collect();
        if pending.is_empty() {
            return Ok(inventory);
        }
        let names = pending.join(", ");
        if Instant::now() >= deadline {
            return Err(TrialError::OneoffsRunning { names });
        }
        if !told {
            progress(format_args!(
                "waiting for one-off containers an interrupted command left: {names}"
            ));
            told = true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        inventory = Inventory::scan(docker).await?;
        require_owned(paths, &inventory, Some(our_id))?;
    }
}

/// `docker compose` for the trial with `rest`, streamed to stderr.
async fn compose_stream(
    docker: &Docker,
    rest: &[&str],
    timeout: Duration,
) -> Result<(), TrialError> {
    let args = docker.compose(Args::new().args(rest));
    Ok(docker.stream(&args, timeout).await?)
}

/// A `compose` call whose output is parsed or discarded, never printed.
async fn compose_capture(
    docker: &Docker,
    rest: Args,
    stdin: Option<&[u8]>,
    sensitivity: Sensitivity,
) -> Result<super::docker::Output, TrialError> {
    let args = docker.compose(rest);
    Ok(docker
        .capture(&args, stdin, sensitivity, RUN_TIMEOUT)
        .await?)
}

// -- up ---------------------------------------------------------------------

/// `trawl trial up`.
pub async fn up(paths: &TrialPaths, args: &UpArgs) -> Result<(), TrialError> {
    let session = begin(paths).await?;
    let docker = &session.docker;
    progress(format_args!(
        "using Docker Engine {} at {}",
        session.engine.engine_id,
        docker.endpoint().as_str()
    ));
    let mut state = admit(paths, &session, args).await?;
    if state.images.trawl_overridden {
        progress(format_args!(
            "running the trawl image {} (--image)",
            state.images.trawl.reference
        ));
    }

    // Created on the first `up` by `create`; taken again here on a resume,
    // so a claim someone removed is restored before anything else moves.
    ownership::claim(docker, &state.trial_id, &state.images.trawl).await?;
    paths.ensure_dir()?;
    state.save(&paths.state_file())?;
    write_compose(paths, docker, &state)?;

    setup(docker, paths, &mut state).await?;
    start_services(docker, &state).await?;
    let clients = verify(paths, &state).await?;
    drop(clients);
    state.phases.services_verified = true;
    state.save(&paths.state_file())?;

    let mut stdout = io::stdout().lock();
    render::render_summary(&mut stdout, &state, paths)
        .map_err(|e| TrialError::io("write", "stdout", e))?;
    Ok(())
}

/// Everything `up` checks before it changes anything: ownership, one-off
/// containers, the resume rules, the ports, and the images. Returns the
/// resumed state, or a new trial's state with its claim taken.
async fn admit(
    paths: &TrialPaths,
    session: &Session,
    args: &UpArgs,
) -> Result<TrialState, TrialError> {
    let docker = &session.docker;
    let existing = load_state(paths)?;
    let our_id = existing.as_ref().map(|s| s.trial_id.clone());
    let mut inventory = Inventory::scan(docker).await?;
    require_owned(paths, &inventory, our_id.as_deref())?;
    if let Some(id) = &our_id {
        inventory = settle_oneoffs(docker, paths, inventory, id).await?;
    }

    let ports = match &existing {
        Some(state) => {
            check_resume(state, &session.engine.engine_id, args)?;
            state.ports
        }
        None => Ports {
            api: args.api_port.unwrap_or(DEFAULT_API_PORT),
            web: args.web_port.unwrap_or(DEFAULT_WEB_PORT),
        },
    };
    check_ports(&inventory, our_id.as_deref(), ports)?;

    let Some(state) = existing else {
        return create(paths, docker, &session.engine, args, ports).await;
    };
    let found = [
        inspect_image(docker, &state.images.trawl.reference).await?,
        inspect_image(docker, &state.images.postgres.reference).await?,
    ];
    let [Some(trawl), Some(postgres)] = found else {
        let gone = if found[0].is_none() {
            &state.images.trawl.reference
        } else {
            &state.images.postgres.reference
        };
        return Err(TrialError::ImageGone {
            reference: gone.clone(),
        });
    };
    check_images(&state.images, &trawl, &postgres)?;
    check_container_images(docker, &inventory, &state).await?;
    Ok(state)
}

/// The recorded phases that are not done yet: databases, the Fleet
/// schema, the certificate, and the keys.
async fn setup(
    docker: &Docker,
    paths: &TrialPaths,
    state: &mut TrialState,
) -> Result<(), TrialError> {
    if state.phases.database {
        // A stopped or resumed trial: every `fleet-admin` step below runs
        // with `--no-deps` and needs PostgreSQL up.
        start_postgres(docker).await?;
    } else {
        databases(docker, state).await?;
        state.phases.database = true;
        state.save(&paths.state_file())?;
    }
    if !state.phases.fleet_migrated {
        progress("applying the Fleet schema (fleet-admin migrate)");
        compose_capture(docker, keys::migrate_args(), None, Sensitivity::Diagnose).await?;
        state.phases.fleet_migrated = true;
        state.save(&paths.state_file())?;
    }
    if !state.phases.tls || read_public(&paths.ca_file(), MAX_CA_BYTES)?.is_none() {
        certificate(docker, paths, state).await?;
    }
    for key in TrialKey::ALL {
        ensure_key(docker, paths, state, key).await?;
    }
    Ok(())
}

/// `compose up --wait trawld trawl-web`. A failure names the ports,
/// because Docker's publish error is the backstop of the bind test.
async fn start_services(docker: &Docker, state: &TrialState) -> Result<(), TrialError> {
    progress("starting trawld and trawl-web");
    compose_stream(
        docker,
        &[
            "up",
            "-d",
            "--wait",
            "--wait-timeout",
            WAIT_SECS,
            Service::Trawld.name(),
            Service::Web.name(),
        ],
        UP_TIMEOUT,
    )
    .await
    .map_err(|e| match e {
        TrialError::Docker(source) => TrialError::ServicesFailed {
            source,
            api: state.ports.api,
            web: state.ports.web,
        },
        other => other,
    })
}

/// A resume keeps what the trial was created with: its engine, its ports,
/// and its image. An omitted flag means the recorded value; a different
/// one refuses.
pub fn check_resume(state: &TrialState, engine_id: &str, args: &UpArgs) -> Result<(), TrialError> {
    if state.engine_id != engine_id {
        return Err(TrialError::EngineChanged {
            recorded: state.engine_id.clone(),
            found: engine_id.to_owned(),
        });
    }
    let fixed = |flag, recorded: String| TrialError::FixedAtCreation { flag, recorded };
    if let Some(port) = args.api_port
        && port != state.ports.api
    {
        return Err(fixed("--api-port", state.ports.api.to_string()));
    }
    if let Some(port) = args.web_port
        && port != state.ports.web
    {
        return Err(fixed("--web-port", state.ports.web.to_string()));
    }
    if let Some(image) = &args.image
        && *image != state.images.trawl.reference
    {
        return Err(fixed("--image", state.images.trawl.reference.clone()));
    }
    Ok(())
}

/// A resume runs only the images the trial recorded: the same id, and the
/// same registry digest.
pub fn check_images(
    recorded: &Images,
    trawl: &ImageRecord,
    postgres: &ImageRecord,
) -> Result<(), TrialError> {
    for (recorded, found) in [(&recorded.trawl, trawl), (&recorded.postgres, postgres)] {
        if recorded.id != found.id || recorded.repo_digest != found.repo_digest {
            let show = |r: &ImageRecord| match &r.repo_digest {
                Some(digest) => format!("{} ({digest})", r.id),
                None => r.id.clone(),
            };
            return Err(TrialError::ImageChanged {
                reference: recorded.reference.clone(),
                recorded: show(recorded),
                found: show(found),
            });
        }
    }
    Ok(())
}

/// Every existing trial container runs one of the recorded image ids, and
/// PostgreSQL runs the PostgreSQL one.
pub fn check_running_images(found: &[(String, String)], images: &Images) -> Result<(), TrialError> {
    for (name, image) in found {
        let recorded = if name.starts_with(&format!("{PROJECT}-{}-", Service::Postgres.name())) {
            &images.postgres.id
        } else {
            &images.trawl.id
        };
        if image != recorded {
            return Err(TrialError::ContainerImageChanged {
                container: name.clone(),
                recorded: recorded.clone(),
                found: image.clone(),
            });
        }
    }
    Ok(())
}

/// Read the image of every non-one-off trial container and check it.
async fn check_container_images(
    docker: &Docker,
    inventory: &Inventory,
    state: &TrialState,
) -> Result<(), TrialError> {
    let ours: Vec<&Resource> = inventory
        .resources
        .iter()
        .filter(|r| r.kind == Kind::Container && !r.oneoff)
        .collect();
    if ours.is_empty() {
        return Ok(());
    }
    let args = Args::new()
        .args(["container", "inspect", "--format", "{{json .Image}}"])
        .args(ours.iter().map(|r| r.id.as_str()));
    let out = docker
        .capture(&args, None, Sensitivity::Diagnose, PROBE_TIMEOUT)
        .await?;
    let text = String::from_utf8_lossy(&out.stdout);
    let images: Vec<String> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<String>(line).unwrap_or_default())
        .collect();
    if images.len() != ours.len() {
        return Err(TrialError::ImageUnreadable {
            reference: "the trial's containers".into(),
        });
    }
    let found: Vec<(String, String)> = ours.iter().map(|r| r.name.clone()).zip(images).collect();
    check_running_images(&found, &state.images)
}

/// The ports `up` must bind-test: each one, unless the trial's own
/// container that publishes it is running and so holds it.
fn ports_to_test(
    inventory: &Inventory,
    our_id: Option<&str>,
    ports: Ports,
) -> Vec<(u16, &'static str)> {
    let running = |service: Service| {
        let prefix = format!("{PROJECT}-{}-", service.name());
        inventory.resources.iter().any(|r| {
            r.kind == Kind::Container
                && !r.oneoff
                && r.trial_id.as_deref() == our_id
                && our_id.is_some()
                && r.name.starts_with(&prefix)
                && r.state == "running"
        })
    };
    let mut test = Vec::new();
    if !running(Service::Trawld) {
        test.push((ports.api, "--api-port"));
    }
    if !running(Service::Web) {
        test.push((ports.web, "--web-port"));
    }
    test
}

/// Refuse a port another listener holds. A trial never adopts a listener.
fn check_ports(
    inventory: &Inventory,
    our_id: Option<&str>,
    ports: Ports,
) -> Result<(), TrialError> {
    if ports.api == ports.web {
        return Err(TrialError::SamePort { port: ports.api });
    }
    for (port, flag) in ports_to_test(inventory, our_id, ports) {
        if let Err(e) = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)) {
            return Err(TrialError::PortTaken {
                port,
                flag,
                reason: e.to_string(),
            });
        }
    }
    Ok(())
}

/// `docker image inspect` of `reference`: `None` when the engine has no
/// such image.
async fn inspect_image(
    docker: &Docker,
    reference: &str,
) -> Result<Option<ImageRecord>, TrialError> {
    let args = Args::new().args([
        "image",
        "inspect",
        "--format",
        r#"{"id":{{json .Id}},"digests":{{json .RepoDigests}}}"#,
        "--",
        reference,
    ]);
    let out = docker
        .output(&args, None, Sensitivity::Diagnose, PROBE_TIMEOUT)
        .await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).to_ascii_lowercase();
        if stderr.contains("no such image") {
            return Ok(None);
        }
        return Err(
            super::docker::failure(&args, out.status, &out.stderr, Sensitivity::Diagnose).into(),
        );
    }
    parse_image(reference, &out.stdout)
        .map(Some)
        .ok_or_else(|| TrialError::ImageUnreadable {
            reference: reference.to_owned(),
        })
}

/// Read one `image inspect` line. The registry digest is the one for the
/// reference's repository, else the first, else none.
fn parse_image(reference: &str, json: &[u8]) -> Option<ImageRecord> {
    #[derive(serde::Deserialize)]
    struct Inspected {
        id: String,
        digests: Option<Vec<String>>,
    }
    let text = std::str::from_utf8(json).ok()?.trim();
    let inspected: Inspected = serde_json::from_str(text).ok()?;
    if !inspected.id.starts_with("sha256:") {
        return None;
    }
    let digests = inspected.digests.unwrap_or_default();
    let repository = repository(reference);
    let repo_digest = digests
        .iter()
        .find(|d| {
            d.split_once('@')
                .is_some_and(|(repo, _)| repo == repository)
        })
        .or_else(|| digests.first())
        .cloned();
    Some(ImageRecord {
        reference: reference.to_owned(),
        id: inspected.id,
        repo_digest,
    })
}

/// `ghcr.io/jakub/trawl` for `ghcr.io/jakub/trawl:0.9.0`: the reference
/// without its tag or digest. A `:` before the last `/` is a registry port.
fn repository(reference: &str) -> &str {
    let reference = reference
        .split_once('@')
        .map_or(reference, |(repo, _)| repo);
    let last_slash = reference.rfind('/').map_or(0, |i| i + 1);
    match reference[last_slash..].rfind(':') {
        Some(colon) => &reference[..last_slash + colon],
        None => reference,
    }
}

/// The image for a new trial: inspected, or pulled first when absent.
async fn resolve_image(docker: &Docker, reference: &str) -> Result<ImageRecord, TrialError> {
    if let Some(record) = inspect_image(docker, reference).await? {
        return Ok(record);
    }
    progress(format_args!("pulling {reference}"));
    let pull = Args::new().args(["pull", "--", reference]);
    docker.stream(&pull, PULL_TIMEOUT).await?;
    inspect_image(docker, reference)
        .await?
        .ok_or_else(|| TrialError::ImageGone {
            reference: reference.to_owned(),
        })
}

/// A new trial: its images, its state, and its engine claim. The state is
/// written before the claim, so an interrupted first `up` leaves a trial
/// the rerun resumes, never a claim without state. A claim that is refused
/// removes the directory this call created.
async fn create(
    paths: &TrialPaths,
    docker: &Docker,
    engine: &Engine,
    args: &UpArgs,
    ports: Ports,
) -> Result<TrialState, TrialError> {
    let reference = args
        .image
        .clone()
        .unwrap_or_else(|| format!("{IMAGE_REPO}:{}", env!("CARGO_PKG_VERSION")));
    let trawl = resolve_image(docker, &reference).await?;
    let postgres = resolve_image(docker, POSTGRES_IMAGE).await?;
    let state = TrialState {
        schema: SCHEMA,
        trial_id: secrets::random_hex(16).to_string(),
        project: PROJECT.to_owned(),
        cli_version: env!("CARGO_PKG_VERSION").to_owned(),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        engine_id: engine.engine_id.clone(),
        ports,
        images: Images {
            trawl,
            postgres,
            trawl_overridden: args.image.is_some(),
        },
        phases: Phases::default(),
        tls: None,
        keys: super::state::Keys::default(),
        samples: Samples::NotRequested,
    };
    progress(format_args!("creating trial {}", state.trial_id));
    paths.ensure_dir()?;
    state.save(&paths.state_file())?;
    if let Err(e) = ownership::claim(docker, &state.trial_id, &state.images.trawl).await {
        if let Ok(true) = paths.check_dir() {
            let _ = std::fs::remove_dir_all(&paths.dir);
        }
        return Err(e);
    }
    Ok(state)
}

/// Render `compose.json` for the state, at 0600. It holds no secret.
fn write_compose(
    paths: &TrialPaths,
    docker: &Docker,
    state: &TrialState,
) -> Result<(), TrialError> {
    let path = docker.compose_file();
    let mut bytes = serde_json::to_vec_pretty(&compose::render_compose(state)).map_err(|_| {
        TrialError::StateInvalid {
            path: path.clone(),
            detail: "cannot render the Compose file".into(),
        }
    })?;
    bytes.push(b'\n');
    debug_assert!(path.starts_with(&paths.dir));
    write_private(&path, &bytes, 0o600)
}

/// The database phase. Until it is recorded, every run generates new
/// passwords and writes both sides again, so they converge.
async fn databases(docker: &Docker, state: &TrialState) -> Result<(), TrialError> {
    progress("writing the PostgreSQL superuser password into its volume");
    let superuser = HexSecret::generate();
    put_file(
        docker,
        Target::Postgres,
        compose::PG_SUPERUSER_PASSWORD,
        superuser.expose().as_bytes(),
    )
    .await?;
    drop(superuser);

    start_postgres(docker).await?;

    progress("creating the fleet and trawl databases and their owner roles");
    let passwords = DbPasswords::generate();
    compose_capture(
        docker,
        compose::database_sql_args(),
        Some(compose::database_sql(&passwords).as_bytes()),
        Sensitivity::Sealed,
    )
    .await?;
    put_file(
        docker,
        Target::Trawld,
        compose::PGPASS,
        compose::pgpass(&passwords).as_bytes(),
    )
    .await?;
    drop(passwords);

    progress("writing the trawld and trawl-web configs and the cookie key");
    put_file(
        docker,
        Target::Trawld,
        compose::TRAWLD_TOML,
        compose::trawld_toml().as_bytes(),
    )
    .await?;
    put_file(
        docker,
        Target::Web,
        compose::WEB_TOML,
        compose::web_toml(state).as_bytes(),
    )
    .await?;
    let cookie = CookieKey::generate();
    put_file(docker, Target::Web, compose::WEB_COOKIE, cookie.expose()).await?;
    Ok(())
}

/// `compose up --wait postgres`.
async fn start_postgres(docker: &Docker) -> Result<(), TrialError> {
    progress("starting PostgreSQL");
    compose_stream(
        docker,
        &[
            "up",
            "-d",
            "--wait",
            "--wait-timeout",
            WAIT_SECS,
            Service::Postgres.name(),
        ],
        UP_TIMEOUT,
    )
    .await
}

/// The certificate phase: generate it unless it exists, then copy the
/// public certificate to the host and to trawl-web, and record its
/// fingerprint.
async fn certificate(
    docker: &Docker,
    paths: &TrialPaths,
    state: &mut TrialState,
) -> Result<(), TrialError> {
    progress("generating the trial certificate");
    compose_capture(
        docker,
        Args::new().args(["run", "--rm", "--no-deps", "-T", Service::TlsInit.name()]),
        None,
        Sensitivity::Diagnose,
    )
    .await?;
    let out = compose_capture(
        docker,
        Args::new()
            .args(["run", "--rm", "--no-deps", "-T", "--entrypoint", "cat"])
            .args([Service::TlsInit.name(), compose::TLS_CERT]),
        None,
        Sensitivity::Diagnose,
    )
    .await?;
    let pem = out.stdout.to_vec();
    let fingerprint = fingerprint(&pem)?;
    write_private(&paths.ca_file(), &pem, 0o644)?;
    put_file(docker, Target::Web, compose::WEB_CA, &pem).await?;
    state.tls = Some(TlsRecord {
        sha256_fingerprint: fingerprint,
    });
    state.phases.tls = true;
    state.save(&paths.state_file())
}

/// The SHA-256 fingerprint of the one certificate in `pem`, as colon
/// separated uppercase hex pairs.
pub fn fingerprint(pem: &[u8]) -> Result<String, TrialError> {
    use rustls_pki_types::CertificateDer;
    use rustls_pki_types::pem::PemObject as _;
    use sha2::Digest as _;

    let mut certs = CertificateDer::pem_slice_iter(pem);
    let cert = match certs.next() {
        Some(Ok(cert)) => cert,
        Some(Err(_)) => {
            return Err(TrialError::Certificate {
                reason: "it is not valid PEM",
            });
        }
        None => {
            return Err(TrialError::Certificate {
                reason: "it holds no certificate",
            });
        }
    };
    if certs.next().is_some() {
        return Err(TrialError::Certificate {
            reason: "it holds more than one certificate",
        });
    }
    let digest = sha2::Sha256::digest(cert.as_ref());
    Ok(digest
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":"))
}

/// The key record for `key` in the state.
fn key_record(state: &mut TrialState, key: TrialKey) -> &mut Option<KeyRecord> {
    match key {
        TrialKey::Operator => &mut state.keys.operator,
        TrialKey::Ingest => &mut state.keys.ingest,
    }
}

fn token_file(paths: &TrialPaths, key: TrialKey) -> std::path::PathBuf {
    match key {
        TrialKey::Operator => paths.operator_token_file(),
        TrialKey::Ingest => paths.ingest_token_file(),
    }
}

/// Whether the token file's key is the recorded one: the file holds one
/// token and a newline, and its prefix is the recorded prefix.
pub fn token_matches(file: Option<&[u8]>, recorded: Option<&KeyRecord>, key: TrialKey) -> bool {
    let (Some(file), Some(recorded)) = (file, recorded) else {
        return false;
    };
    recorded.name == key.name()
        && MintedToken::parse(file).is_some_and(|t| t.prefix.as_str() == recorded.prefix)
}

/// Keep the key when its token file matches the record. Otherwise the
/// plaintext is lost, or was never written: revoke every active key of
/// that name, mint a new one, write the token file, then record it.
async fn ensure_key(
    docker: &Docker,
    paths: &TrialPaths,
    state: &mut TrialState,
    key: TrialKey,
) -> Result<(), TrialError> {
    ensure_role(docker, key).await?;
    let path = token_file(paths, key);
    let file = read_private(&path, MAX_TOKEN_BYTES)?.map(Zeroizing::new);
    if token_matches(
        file.as_deref().map(Vec::as_slice),
        key_record(state, key).as_ref(),
        key,
    ) {
        return Ok(());
    }
    drop(file);

    let listed = compose_capture(docker, keys::keys_list_args(), None, Sensitivity::Sealed).await?;
    let table = std::str::from_utf8(&listed.stdout).map_err(|_| TrialError::Fleet {
        what: "`keys list` printed text that is not UTF-8".into(),
    })?;
    let revoke = keys::active_prefixes(&keys::parse_keys_table(table)?, key);
    if !revoke.is_empty() {
        progress(format_args!(
            "revoking {} earlier {} key(s) whose token file is gone",
            revoke.len(),
            key.name()
        ));
    }
    for prefix in &revoke {
        compose_capture(
            docker,
            keys::key_revoke_args(prefix),
            None,
            Sensitivity::Sealed,
        )
        .await?;
    }

    progress(format_args!("minting the {} key", key.name()));
    let out = compose_capture(
        docker,
        keys::key_create_args(key),
        None,
        Sensitivity::Sealed,
    )
    .await?;
    let minted = MintedToken::parse(&out.stdout).ok_or_else(|| TrialError::Fleet {
        what: format!(
            "`keys create --name {}` did not print one token and a newline",
            key.name()
        ),
    })?;
    write_private(&path, &minted.file_bytes, 0o600)?;
    *key_record(state, key) = Some(KeyRecord {
        name: key.name().to_owned(),
        prefix: minted.prefix.as_str().to_owned(),
    });
    state.save(&paths.state_file())
}

/// Create the key's role unless `roles show` finds it.
async fn ensure_role(docker: &Docker, key: TrialKey) -> Result<(), TrialError> {
    let show = docker.compose(keys::role_show_args(key));
    let out = docker
        .output(&show, None, Sensitivity::Diagnose, RUN_TIMEOUT)
        .await?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.contains(&format!("role not found: {}", key.name())) {
        return Err(
            super::docker::failure(&show, out.status, &out.stderr, Sensitivity::Diagnose).into(),
        );
    }
    progress(format_args!("creating the {} role", key.name()));
    compose_capture(
        docker,
        keys::role_create_args(key),
        None,
        Sensitivity::Diagnose,
    )
    .await?;
    Ok(())
}

/// The two keys' clients, pinned to the trial certificate.
pub struct Clients {
    pub operator: HttpClient,
    pub ingest: HttpClient,
}

impl std::fmt::Debug for Clients {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Clients(<redacted>)")
    }
}

/// A client for `key`, pinned to the host `ca.pem`.
fn client(paths: &TrialPaths, state: &TrialState, key: TrialKey) -> Result<HttpClient, TrialError> {
    let ca = read_public(&paths.ca_file(), MAX_CA_BYTES)?.ok_or(TrialError::Certificate {
        reason: "ca.pem is missing from the trial directory",
    })?;
    let bytes = Zeroizing::new(
        read_private(&token_file(paths, key), MAX_TOKEN_BYTES)?.ok_or_else(|| {
            TrialError::NoOperatorToken {
                dir: paths.dir.clone(),
            }
        })?,
    );
    let token = Zeroizing::new(
        std::str::from_utf8(&bytes)
            .map_err(|_| TrialError::Fleet {
                what: format!("the {} token file is not UTF-8", key.name()),
            })?
            .trim()
            .to_owned(),
    );
    HttpClient::with_trust(
        render::api_url(state),
        token.as_str(),
        &TlsTrust::PinnedCa(ca),
    )
    .map_err(|source| TrialError::Api {
        url: render::api_url(state),
        source,
    })
}

/// `whoami` for both keys through the pinned certificate, then one
/// authenticated query. Returns the clients for the sample step.
async fn verify(paths: &TrialPaths, state: &TrialState) -> Result<Clients, TrialError> {
    progress("checking both keys and one query through the pinned certificate");
    let url = render::api_url(state);
    let api = |source| TrialError::Api {
        url: url.clone(),
        source,
    };
    let clients = Clients {
        operator: client(paths, state, TrialKey::Operator)?,
        ingest: client(paths, state, TrialKey::Ingest)?,
    };
    let deadline = Instant::now() + API_WAIT;
    let operator = loop {
        match clients.operator.whoami().await {
            Ok(who) => break who,
            Err(trawl_client::ClientError::Network(_)) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => return Err(api(e)),
        }
    };
    check_identity(TrialKey::Operator, &operator)?;
    let ingest = clients.ingest.whoami().await.map_err(api)?;
    check_identity(TrialKey::Ingest, &ingest)?;
    clients
        .operator
        .query_paginated(PROOF_QUERY, Some(1), None)
        .await
        .map_err(api)?;
    Ok(clients)
}

/// The key's kind and its exact permission set, as ADR-0045 fixes them.
/// The message never quotes the key's prefix.
pub fn check_identity(key: TrialKey, who: &trawl_api::WhoAmIResponse) -> Result<(), TrialError> {
    let expected_kind = match key {
        TrialKey::Operator => trawl_api::PrincipalKind::Human,
        TrialKey::Ingest => trawl_api::PrincipalKind::Service,
    };
    let identity = |problem: String| TrialError::Identity {
        key: key.name(),
        problem,
    };
    if who.name != key.name() {
        return Err(identity(format!("the server names it {:?}", who.name)));
    }
    if who.kind != expected_kind {
        return Err(identity(format!(
            "its kind is {}, not {}",
            who.kind.as_str(),
            expected_kind.as_str()
        )));
    }
    let found: BTreeSet<&str> = who.permissions.iter().map(String::as_str).collect();
    let expected: BTreeSet<&str> = key.permissions().iter().copied().collect();
    if found != expected {
        return Err(identity(format!(
            "its permissions are [{}], not [{}]",
            found.into_iter().collect::<Vec<_>>().join(", "),
            expected.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(())
}

// -- status and key ---------------------------------------------------------

/// `trawl trial status`: works without the lock, and without Docker.
pub async fn status(paths: &TrialPaths) -> Result<(), TrialError> {
    let state = load_state(paths)?.ok_or_else(|| TrialError::NotCreated {
        dir: paths.dir.clone(),
    })?;
    let containers = match preflight::preflight(
        DockerCli::new(),
        &paths.dir,
        std::env::var_os("DOCKER_HOST"),
    )
    .await
    {
        Ok((_, docker)) => match Inventory::scan(&docker).await {
            Ok(inventory) => Containers::Listed(our_containers(&inventory, &state.trial_id)),
            Err(e) => Containers::Unknown(e.to_string()),
        },
        Err(e) => Containers::Unknown(e.to_string()),
    };
    let set: Vec<&str> = render::CONNECTION_VARIABLES
        .into_iter()
        .filter(|name| std::env::var_os(name).is_some())
        .collect();
    render::render_env_warnings(&mut io::stderr().lock(), &set)
        .map_err(|e| TrialError::io("write", "stderr", e))?;
    render::render_status(&mut io::stdout().lock(), &state, paths, &containers)
        .map_err(|e| TrialError::io("write", "stdout", e))
}

/// `(name, state)` of the trial's containers, one-offs excluded.
fn our_containers(inventory: &Inventory, our_id: &str) -> Vec<(String, String)> {
    let mut list: Vec<(String, String)> = inventory
        .resources
        .iter()
        .filter(|r| r.kind == Kind::Container && !r.oneoff && r.trial_id.as_deref() == Some(our_id))
        .map(|r| (r.name.clone(), r.state.clone()))
        .collect();
    list.sort();
    list
}

/// `trawl trial key`: the operator token file's bytes on stdout, and
/// nothing else anywhere.
pub fn key(paths: &TrialPaths) -> Result<(), TrialError> {
    if !paths.check_dir()? {
        return Err(TrialError::NotCreated {
            dir: paths.dir.clone(),
        });
    }
    let path = paths.operator_token_file();
    let bytes = Zeroizing::new(read_private(&path, MAX_TOKEN_BYTES)?.ok_or_else(|| {
        TrialError::NoOperatorToken {
            dir: paths.dir.clone(),
        }
    })?);
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(&bytes)
        .and_then(|()| stdout.flush())
        .map_err(|e| TrialError::io("write", "stdout", e))
}

// -- stop and down ----------------------------------------------------------

/// `trawl trial stop`: stop the containers; volumes, keys, samples, and
/// state stay.
pub async fn stop(paths: &TrialPaths) -> Result<(), TrialError> {
    let session = begin(paths).await?;
    let docker = &session.docker;
    let view = load_down_view(paths)?;
    let inventory = Inventory::scan(docker).await?;
    require_owned(
        paths,
        &inventory,
        view.as_ref().map(|v| v.trial_id.as_str()),
    )?;
    let Some(view) = view else {
        return Err(TrialError::NotCreated {
            dir: paths.dir.clone(),
        });
    };
    settle_oneoffs(docker, paths, inventory, &view.trial_id).await?;
    if !docker.compose_file().is_file() {
        return Err(TrialError::StateInvalid {
            path: docker.compose_file(),
            detail: "it is missing; `trawl trial up` writes it again".into(),
        });
    }
    progress("stopping the trial's containers");
    compose_stream(docker, &["stop"], STOP_TIMEOUT).await?;
    progress("stopped. `trawl trial up` resumes the trial");
    Ok(())
}

/// What `down` does once it has listed the trial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmation {
    Proceed,
    Declined,
    /// stdin is not a terminal and `--yes` was not given.
    NeedsYes,
}

/// `--yes` proceeds. Otherwise a terminal is asked `[y/N]`, and only `y`
/// or `yes` proceeds; with no terminal, nothing is asked.
pub fn confirm(
    yes: bool,
    tty: bool,
    input: &mut impl BufRead,
    prompt: &mut impl Write,
) -> io::Result<Confirmation> {
    if yes {
        return Ok(Confirmation::Proceed);
    }
    if !tty {
        return Ok(Confirmation::NeedsYes);
    }
    write!(prompt, "Delete all of it? [y/N] ")?;
    prompt.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Confirmation::Proceed,
        _ => Confirmation::Declined,
    })
}

/// `trawl trial down`: delete every resource that carries this trial's id,
/// the claim last, then the trial directory. The lock file stays.
pub async fn down(paths: &TrialPaths, yes: bool) -> Result<(), TrialError> {
    let session = begin(paths).await?;
    let docker = &session.docker;
    let view = load_down_view(paths)?;
    let inventory = Inventory::scan(docker).await?;
    require_owned(
        paths,
        &inventory,
        view.as_ref().map(|v| v.trial_id.as_str()),
    )?;
    let Some(view) = view else {
        // Nothing on the engine, and no state: at most an empty directory.
        if paths.check_dir()? {
            remove_dir(paths)?;
            progress("removed a trial directory that held no trial");
        } else {
            progress("there is no trial here; nothing to delete");
        }
        return Ok(());
    };
    let inventory = settle_oneoffs(docker, paths, inventory, &view.trial_id).await?;

    let mut stdout = io::stdout().lock();
    render::render_inventory(&mut stdout, &inventory, &paths.dir)
        .and_then(|()| stdout.flush())
        .map_err(|e| TrialError::io("write", "stdout", e))?;
    drop(stdout);
    let stdin = io::stdin();
    let tty = stdin.is_terminal();
    let answer = confirm(yes, tty, &mut stdin.lock(), &mut io::stderr().lock())
        .map_err(|e| TrialError::io("read", "stdin", e))?;
    match answer {
        Confirmation::Proceed => {}
        Confirmation::Declined => return Err(TrialError::Declined),
        Confirmation::NeedsYes => return Err(TrialError::ConfirmationRequired),
    }

    delete(docker, &inventory, &view.trial_id).await?;
    remove_dir(paths)?;
    progress("deleted the trial");
    Ok(())
}

/// Delete the inventory by id: containers, networks, volumes, and the
/// claim last. Only resources carrying `our_id` are ever named.
async fn delete(docker: &Docker, inventory: &Inventory, our_id: &str) -> Result<(), TrialError> {
    let ours = |kind: Kind, claim: bool| -> Vec<&str> {
        inventory
            .resources
            .iter()
            .filter(|r| {
                r.kind == kind
                    && r.trial_id.as_deref() == Some(our_id)
                    && (r.name == CLAIM_NAME) == claim
            })
            .map(|r| r.id.as_str())
            .collect()
    };
    let steps: [(&[&str], Vec<&str>); 4] = [
        (
            &["container", "rm", "--force", "--"],
            ours(Kind::Container, false),
        ),
        (&["network", "rm", "--"], ours(Kind::Network, false)),
        (&["volume", "rm", "--"], ours(Kind::Volume, false)),
        (
            &["container", "rm", "--force", "--"],
            ours(Kind::Container, true),
        ),
    ];
    for (command, ids) in steps {
        if ids.is_empty() {
            continue;
        }
        let args = Args::new().args(command).args(ids);
        docker
            .capture(&args, None, Sensitivity::Diagnose, RUN_TIMEOUT)
            .await?;
    }
    Ok(())
}

/// Remove the trial directory, after checking it is a real directory of
/// ours. `remove_dir_all` does not follow symlinks inside it.
fn remove_dir(paths: &TrialPaths) -> Result<(), TrialError> {
    if paths.check_dir()? {
        std::fs::remove_dir_all(&paths.dir).map_err(|e| TrialError::io("remove", &paths.dir, e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trial::state::tests::fixture;

    fn args() -> UpArgs {
        UpArgs {
            api_port: None,
            web_port: None,
            image: None,
            no_sample_data: false,
        }
    }

    #[test]
    fn a_resume_keeps_its_engine_ports_and_image() {
        let state = fixture(DEFAULT_API_PORT);
        check_resume(&state, "ENGINE:ID", &args()).expect("omitted flags resume");
        let same = UpArgs {
            api_port: Some(DEFAULT_API_PORT),
            web_port: Some(DEFAULT_WEB_PORT),
            image: Some(state.images.trawl.reference.clone()),
            ..args()
        };
        check_resume(&state, "ENGINE:ID", &same).expect("the recorded values resume");

        let err = check_resume(&state, "OTHER:ENGINE", &args()).unwrap_err();
        assert!(
            matches!(&err, TrialError::EngineChanged { recorded, found }
                if recorded == "ENGINE:ID" && found == "OTHER:ENGINE"),
            "{err:?}"
        );
        assert!(err.to_string().contains("trawl trial down"), "{err}");

        for (changed, flag) in [
            (
                UpArgs {
                    api_port: Some(1),
                    ..args()
                },
                "--api-port",
            ),
            (
                UpArgs {
                    web_port: Some(1),
                    ..args()
                },
                "--web-port",
            ),
            (
                UpArgs {
                    image: Some("elsewhere:1".into()),
                    ..args()
                },
                "--image",
            ),
        ] {
            let err = check_resume(&state, "ENGINE:ID", &changed).unwrap_err();
            assert!(
                matches!(err, TrialError::FixedAtCreation { flag: f, .. } if f == flag),
                "{flag}: {err:?}"
            );
        }
    }

    fn record(reference: &str, id: &str, digest: Option<&str>) -> ImageRecord {
        ImageRecord {
            reference: reference.into(),
            id: id.into(),
            repo_digest: digest.map(Into::into),
        }
    }

    /// The acceptance criterion: a resume refuses when a recorded image
    /// digest differs.
    #[test]
    fn a_resume_refuses_a_different_image_digest_or_id() {
        let images = fixture(1).images;
        check_images(&images, &images.trawl, &images.postgres).expect("unchanged images resume");

        let moved_digest = record(
            &images.trawl.reference,
            &images.trawl.id,
            Some("ghcr.io/jakub/trawl@sha256:ffff"),
        );
        let err = check_images(&images, &moved_digest, &images.postgres).unwrap_err();
        assert!(
            matches!(&err, TrialError::ImageChanged { reference, recorded, found }
                if reference == "ghcr.io/jakub/trawl:0.9.0"
                    && recorded.contains("sha256:bbbb")
                    && found.contains("sha256:ffff")),
            "{err:?}"
        );
        assert!(err.to_string().contains("trawl trial down"), "{err}");

        let rebuilt = record(POSTGRES_IMAGE, "sha256:dddd", None);
        let err = check_images(&images, &images.trawl, &rebuilt).unwrap_err();
        assert!(
            matches!(&err, TrialError::ImageChanged { reference, .. } if reference == POSTGRES_IMAGE),
            "{err:?}"
        );
    }

    #[test]
    fn running_containers_must_run_the_recorded_ids() {
        let images = fixture(1).images;
        let ok = [
            (
                "trawl-trial-postgres-1".to_owned(),
                "sha256:cccc".to_owned(),
            ),
            ("trawl-trial-trawld-1".to_owned(), "sha256:aaaa".to_owned()),
            (CLAIM_NAME.to_owned(), "sha256:aaaa".to_owned()),
        ];
        check_running_images(&ok, &images).unwrap();
        let swapped = [(
            "trawl-trial-postgres-1".to_owned(),
            "sha256:aaaa".to_owned(),
        )];
        assert!(matches!(
            check_running_images(&swapped, &images),
            Err(TrialError::ContainerImageChanged { .. })
        ));
    }

    #[test]
    fn image_inspect_output_is_read_with_the_matching_digest() {
        let json = br#"{"id":"sha256:1111","digests":["mirror.example/trawl@sha256:aa","ghcr.io/jakub/trawl@sha256:bb"]}"#;
        let image = parse_image("ghcr.io/jakub/trawl:0.9.0", json).unwrap();
        assert_eq!(image.id, "sha256:1111");
        assert_eq!(
            image.repo_digest.as_deref(),
            Some("ghcr.io/jakub/trawl@sha256:bb")
        );
        let local = parse_image(
            "trawl-trial-local:issue-203",
            br#"{"id":"sha256:2222","digests":[]}"#,
        )
        .unwrap();
        assert_eq!(local.repo_digest, None);
        let null = parse_image("x:1", br#"{"id":"sha256:3333","digests":null}"#).unwrap();
        assert_eq!(null.repo_digest, None);
        assert!(parse_image("x:1", b"not json").is_none());
        assert!(parse_image("x:1", br#"{"id":"md5:1","digests":[]}"#).is_none());

        assert_eq!(
            repository("ghcr.io/jakub/trawl:0.9.0"),
            "ghcr.io/jakub/trawl"
        );
        assert_eq!(repository("localhost:5000/trawl"), "localhost:5000/trawl");
        assert_eq!(repository("localhost:5000/trawl:1"), "localhost:5000/trawl");
        assert_eq!(repository("postgres:18"), "postgres");
        assert_eq!(repository("postgres@sha256:aa"), "postgres");
    }

    fn resource(name: &str, trial: &str, oneoff: bool, state: &str) -> Resource {
        Resource {
            kind: Kind::Container,
            id: format!("{name}-id"),
            name: name.into(),
            project: Some(PROJECT.into()),
            trial_id: Some(trial.into()),
            oneoff,
            state: state.into(),
        }
    }

    /// A trial's own running container holds its port; anything else,
    /// including a stopped trial, is bind-tested.
    #[test]
    fn only_a_running_trial_container_skips_the_bind_test() {
        let ports = Ports {
            api: 15514,
            web: 18090,
        };
        let id = "0123456789abcdef0123456789abcdef";
        let inventory = Inventory {
            resources: vec![
                resource("trawl-trial-trawld-1", id, false, "running"),
                resource("trawl-trial-trawl-web-1", id, false, "exited"),
                resource("trawl-trial-trawl-web-run-1", id, true, "running"),
            ],
        };
        assert_eq!(
            ports_to_test(&inventory, Some(id), ports),
            [(18090, "--web-port")]
        );
        assert_eq!(
            ports_to_test(&inventory, None, ports),
            [(15514, "--api-port"), (18090, "--web-port")]
        );
        assert_eq!(
            ports_to_test(&Inventory::default(), Some(id), ports),
            [(15514, "--api-port"), (18090, "--web-port")]
        );
    }

    #[test]
    fn a_held_port_is_refused_by_number_and_flag() {
        let held = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        let err =
            check_ports(&Inventory::default(), None, Ports { api: port, web: 1 }).unwrap_err();
        let message = err.to_string();
        assert!(message.contains(&format!("port {port}")), "{message}");
        assert!(message.contains("--api-port"), "{message}");

        let err = check_ports(&Inventory::default(), None, Ports { api: 7, web: 7 }).unwrap_err();
        assert!(matches!(err, TrialError::SamePort { port: 7 }), "{err:?}");
    }

    #[test]
    fn down_asks_a_terminal_and_requires_yes_without_one() {
        let ask = |yes: bool, tty: bool, answer: &str| {
            let mut prompt = Vec::new();
            let got = confirm(yes, tty, &mut answer.as_bytes(), &mut prompt).unwrap();
            (got, String::from_utf8(prompt).unwrap())
        };
        for answer in ["y\n", "yes\n", "Y\n", " YES \n"] {
            let (got, prompt) = ask(false, true, answer);
            assert_eq!(got, Confirmation::Proceed, "{answer:?}");
            assert_eq!(prompt, "Delete all of it? [y/N] ");
        }
        for answer in ["n\n", "\n", "", "no\n", "yess\n"] {
            assert_eq!(
                ask(false, true, answer).0,
                Confirmation::Declined,
                "{answer:?}"
            );
        }
        let (got, prompt) = ask(false, false, "y\n");
        assert_eq!(
            got,
            Confirmation::NeedsYes,
            "no terminal: stdin is not read"
        );
        assert!(prompt.is_empty());
        for tty in [true, false] {
            let (got, prompt) = ask(true, tty, "");
            assert_eq!(got, Confirmation::Proceed);
            assert!(prompt.is_empty(), "--yes asks nothing");
        }
        assert!(
            TrialError::ConfirmationRequired
                .to_string()
                .contains("--yes")
        );
    }

    fn who(
        name: &str,
        kind: trawl_api::PrincipalKind,
        permissions: &[&str],
    ) -> trawl_api::WhoAmIResponse {
        trawl_api::WhoAmIResponse {
            prefix: "pfxsecret".into(),
            name: name.into(),
            kind,
            roles: vec![name.into()],
            permissions: permissions.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    #[test]
    fn identity_is_the_kind_and_the_exact_permission_set() {
        use trawl_api::PrincipalKind::{Human, Service};
        let operator = keys::OPERATOR_PERMISSIONS;
        check_identity(TrialKey::Operator, &who("trial-operator", Human, &operator)).unwrap();
        check_identity(TrialKey::Ingest, &who("trial-ingest", Service, &["ingest"])).unwrap();

        let wider: Vec<&str> = operator.iter().copied().chain(["ingest"]).collect();
        for (key, response) in [
            (
                TrialKey::Operator,
                who("trial-operator", Service, &operator),
            ),
            (TrialKey::Operator, who("trial-operator", Human, &wider)),
            (
                TrialKey::Operator,
                who("trial-operator", Human, &operator[1..]),
            ),
            (
                TrialKey::Ingest,
                who("trial-ingest", Service, &["ingest", "query"]),
            ),
            (TrialKey::Ingest, who("other", Service, &["ingest"])),
        ] {
            let err = check_identity(key, &response).unwrap_err();
            assert!(!err.to_string().contains("pfxsecret"), "{err}");
        }
    }

    #[test]
    fn a_token_file_matches_only_its_recorded_prefix() {
        let token = b"flt_dGhpcyBpcyBhIHRlc3QgdG9rZW4gYm9keSAxMjM0NTY\n";
        let recorded = KeyRecord {
            name: "trial-operator".into(),
            prefix: "dGhpcyBp".into(),
        };
        assert!(token_matches(
            Some(token),
            Some(&recorded),
            TrialKey::Operator
        ));
        assert!(
            !token_matches(None, Some(&recorded), TrialKey::Operator),
            "file lost"
        );
        assert!(
            !token_matches(Some(token), None, TrialKey::Operator),
            "never recorded"
        );
        assert!(
            !token_matches(Some(token), Some(&recorded), TrialKey::Ingest),
            "other name"
        );
        let other = KeyRecord {
            prefix: "AAAAAAAA".into(),
            ..recorded.clone()
        };
        assert!(!token_matches(
            Some(token),
            Some(&other),
            TrialKey::Operator
        ));
        assert!(!token_matches(
            Some(&token[..20]),
            Some(&recorded),
            TrialKey::Operator
        ));
    }

    #[test]
    fn the_fingerprint_is_sha256_of_the_der() {
        let pair = rcgen::generate_simple_self_signed(vec!["trawld".to_owned()]).unwrap();
        let pem = pair.cert.pem();
        let expected = {
            use sha2::Digest as _;
            sha2::Sha256::digest(pair.cert.der().as_ref())
        };
        let got = fingerprint(pem.as_bytes()).unwrap();
        assert_eq!(got.len(), 32 * 3 - 1);
        assert_eq!(got.split(':').count(), 32);
        let bytes: Vec<u8> = got
            .split(':')
            .map(|pair| {
                assert_eq!(pair, pair.to_uppercase(), "uppercase hex");
                u8::from_str_radix(pair, 16).unwrap()
            })
            .collect();
        assert_eq!(bytes, expected.as_slice());
        assert!(fingerprint(b"").is_err());
        assert!(fingerprint(format!("{pem}{pem}").as_bytes()).is_err());
    }
}
