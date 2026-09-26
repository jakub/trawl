// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the trial needs from Docker, checked before anything is created.
//!
//! The trial needs Docker Engine on this machine, reached over a local
//! Unix socket, and the Compose v2 plugin at 2.20 or later. The check is a
//! pure decision over [`EngineFacts`], so every case is testable without
//! Docker; [`preflight`] gathers the facts and applies it.
//!
//! - `DOCKER_HOST`, when set to anything but a `unix://` address, is
//!   refused before any subprocess runs, so a remote engine is never
//!   contacted.
//! - The active context's endpoint must be `unix://` too; a remote
//!   context is refused before the engine is asked anything.
//! - The Compose floor is a comparison, so any later major version passes.
//!   The legacy `docker-compose` v1 executable is never run.
//!
//! There is no OS check and no sniffing for Podman or Docker Desktop: the
//! trial states the endpoint test it performs, and a local socket that
//! proxies elsewhere is its owner's business.

use std::ffi::{OsStr, OsString};
use std::fmt;

use super::docker::{Args, Docker, DockerError, PROBE_TIMEOUT, Sensitivity};

/// The oldest Compose plugin the trial accepts, as (major, minor).
/// 2.20 is the release that introduced `up --wait-timeout`.
pub const COMPOSE_FLOOR: (u64, u64) = (2, 20);

/// A Compose version's numeric core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ComposeVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl fmt::Display for ComposeVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Parse `docker compose version --short`: an optional leading `v`, then
/// `MAJOR.MINOR[.PATCH]`, then optionally a `-` or `+` suffix such as
/// `-desktop.1`. Anything else is `None`.
pub fn parse_compose_version(raw: &str) -> Option<ComposeVersion> {
    let text = raw.trim();
    let text = text.strip_prefix('v').unwrap_or(text);
    let end = text.find(['-', '+']).unwrap_or(text.len());
    let (core, _suffix) = text.split_at(end);
    let mut parts = core.split('.');
    let mut number = |required: bool| -> Option<u64> {
        match parts.next() {
            Some(part) if !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) => {
                part.parse().ok()
            }
            None if !required => Some(0),
            _ => None,
        }
    };
    let major = number(true)?;
    let minor = number(true)?;
    let patch = number(false)?;
    if parts.next().is_some() {
        return None;
    }
    Some(ComposeVersion {
        major,
        minor,
        patch,
    })
}

/// Everything the preflight decision reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineFacts {
    /// The `DOCKER_HOST` value, when set.
    pub docker_host: Option<OsString>,
    /// Whether the `docker` command could be run at all.
    pub docker_installed: bool,
    /// The active context's endpoint (`docker context inspect`), when it
    /// could be read.
    pub context_endpoint: Option<String>,
    /// Whether `docker version` reached the engine.
    pub server_reachable: bool,
    /// `docker compose version --short`, when the plugin answered.
    pub compose_version: Option<String>,
    /// `docker info` `.ID`, when the engine reported one.
    pub engine_id: Option<String>,
}

/// A usable engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Engine {
    pub compose: ComposeVersion,
    /// Recorded in the trial state; a resume on another engine refuses.
    pub engine_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PreflightError {
    #[error(
        "DOCKER_HOST points at a {scheme} address. The trial runs only on the local \
         Docker Engine over a unix:// socket: unset DOCKER_HOST, or set it to a unix:// path"
    )]
    DockerHostNotLocal { scheme: String },

    #[error(
        "the `docker` command was not found. The trial needs Docker Engine with the \
         Compose v2 plugin, version 2.20 or later"
    )]
    DockerMissing,

    #[error(
        "`docker context inspect` did not report the active context's endpoint. \
         The trial needs the local Docker Engine over a unix:// socket"
    )]
    ContextUnknown,

    #[error(
        "the active Docker context points at a {scheme} endpoint. The trial runs only on \
         the local Docker Engine over a unix:// socket: switch with \
         `docker context use default`, or unset DOCKER_CONTEXT"
    )]
    ContextNotLocal { scheme: String },

    #[error(
        "Docker Engine at {endpoint} is not reachable. Start it, and check that your user \
         may use the socket (for example, membership in the docker group)"
    )]
    ServerUnreachable { endpoint: String },

    #[error(
        "the Docker Compose v2 plugin is missing (`docker compose version` failed). \
         The trial needs Compose 2.20 or later; the legacy docker-compose v1 is not used"
    )]
    ComposeMissing,

    #[error(
        "`docker compose version --short` printed {found:?}, which is not a version. \
         The trial needs Compose 2.20 or later"
    )]
    ComposeUnparseable { found: String },

    #[error("Docker Compose {found} is too old. The trial needs Compose 2.20 or later")]
    ComposeTooOld { found: ComposeVersion },

    #[error("`docker info` did not report an engine ID, so the trial cannot pin its engine")]
    EngineIdUnknown,
}

/// Refuse a `DOCKER_HOST` that is set and is not a `unix://` address.
/// An empty value is unset, as Docker itself treats it.
pub fn check_docker_host(value: Option<&OsStr>) -> Result<(), PreflightError> {
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        return Ok(());
    };
    match value.to_str() {
        Some(text) if text.starts_with("unix://") => Ok(()),
        Some(text) => Err(PreflightError::DockerHostNotLocal {
            scheme: scheme(text),
        }),
        None => Err(PreflightError::DockerHostNotLocal {
            scheme: "non-UTF-8".into(),
        }),
    }
}

/// The preflight decision. The first unmet requirement is the error.
pub fn check(facts: &EngineFacts) -> Result<Engine, PreflightError> {
    check_docker_host(facts.docker_host.as_deref())?;
    if !facts.docker_installed {
        return Err(PreflightError::DockerMissing);
    }
    let endpoint = local_endpoint(facts.context_endpoint.as_deref())?;
    if !facts.server_reachable {
        return Err(PreflightError::ServerUnreachable {
            endpoint: endpoint.to_owned(),
        });
    }
    let raw = facts
        .compose_version
        .as_deref()
        .ok_or(PreflightError::ComposeMissing)?;
    let compose = parse_compose_version(raw).ok_or_else(|| PreflightError::ComposeUnparseable {
        found: raw.chars().take(64).collect(),
    })?;
    if (compose.major, compose.minor) < COMPOSE_FLOOR {
        return Err(PreflightError::ComposeTooOld { found: compose });
    }
    let engine_id = facts
        .engine_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or(PreflightError::EngineIdUnknown)?;
    Ok(Engine {
        compose,
        engine_id: engine_id.to_owned(),
    })
}

fn local_endpoint(endpoint: Option<&str>) -> Result<&str, PreflightError> {
    let endpoint = endpoint.ok_or(PreflightError::ContextUnknown)?;
    if endpoint.starts_with("unix://") {
        Ok(endpoint)
    } else {
        Err(PreflightError::ContextNotLocal {
            scheme: scheme(endpoint),
        })
    }
}

/// `tcp://` for `tcp://host:2375`: only the scheme, because the rest of an
/// address can carry a user name or credentials.
fn scheme(address: &str) -> String {
    match address.split_once("://") {
        Some((scheme, _)) if !scheme.is_empty() && scheme.len() <= 16 => format!("{scheme}://"),
        _ => "non-URL".to_owned(),
    }
}

/// Gather the facts and decide. `docker_host` is this process's
/// `DOCKER_HOST`; it is checked before any subprocess runs, and gathering
/// stops at the first unmet requirement, so a remote endpoint is never
/// contacted.
pub async fn preflight(
    docker: &Docker,
    docker_host: Option<OsString>,
) -> Result<Engine, super::TrialError> {
    check_docker_host(docker_host.as_deref())?;
    let facts = gather(docker, docker_host).await?;
    Ok(check(&facts)?)
}

async fn gather(
    docker: &Docker,
    docker_host: Option<OsString>,
) -> Result<EngineFacts, DockerError> {
    let mut facts = EngineFacts {
        docker_host,
        ..EngineFacts::default()
    };
    let probe = |argv: &'static [&'static str]| {
        let args = Args::new().args(argv);
        async move {
            docker
                .output(&args, None, Sensitivity::Diagnose, PROBE_TIMEOUT)
                .await
        }
    };

    let context = match probe(&["context", "inspect"]).await {
        Err(DockerError::NotInstalled) => return Ok(facts),
        other => other?,
    };
    facts.docker_installed = true;
    if context.status.success() {
        facts.context_endpoint = context_endpoint(&context.stdout);
    }
    if local_endpoint(facts.context_endpoint.as_deref()).is_err() {
        return Ok(facts);
    }

    let version = probe(&["version", "--format", "json"]).await?;
    facts.server_reachable = version.status.success() && server_present(&version.stdout);
    if !facts.server_reachable {
        return Ok(facts);
    }

    let compose = probe(&["compose", "version", "--short"]).await?;
    if compose.status.success() {
        facts.compose_version = Some(String::from_utf8_lossy(&compose.stdout).trim().to_owned());
    }

    let info = probe(&["info", "--format", "{{.ID}}"]).await?;
    if info.status.success() {
        facts.engine_id = Some(String::from_utf8_lossy(&info.stdout).trim().to_owned());
    }
    Ok(facts)
}

/// The Docker endpoint of the first context in `docker context inspect`
/// output.
fn context_endpoint(json: &[u8]) -> Option<String> {
    let contexts: serde_json::Value = serde_json::from_slice(json).ok()?;
    contexts
        .get(0)?
        .pointer("/Endpoints/docker/Host")?
        .as_str()
        .map(str::to_owned)
}

/// Whether `docker version --format json` reported a server.
fn server_present(json: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.get("Server").map(serde_json::Value::is_object))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trial::docker::tests::stub;

    fn usable() -> EngineFacts {
        EngineFacts {
            docker_host: None,
            docker_installed: true,
            context_endpoint: Some("unix:///var/run/docker.sock".into()),
            server_reachable: true,
            compose_version: Some("2.29.1".into()),
            engine_id: Some("8ba78a3b-6b42-467d-b260-df6109c9505e".into()),
        }
    }

    #[test]
    fn compose_versions_compare_against_the_floor() {
        let cases = [
            ("2.19.9", Some(false)),
            ("2.20.0", Some(true)),
            ("v2.20.0", Some(true)),
            ("2.20", Some(true)),
            ("v2.29.1-desktop.1", Some(true)),
            ("2.21.0+azure-1", Some(true)),
            ("5.5.1", Some(true)),
            ("3.0.0", Some(true)),
            ("1.29.2", Some(false)),
            ("2.3.3", Some(false)),
            ("2.20.0\n", Some(true)),
            ("", None),
            ("docker-compose version 1.29.2", None),
            ("2", None),
            ("2.x.1", None),
            ("2.20.0.1", None),
            ("2..1", None),
            ("-2.20.0", None),
        ];
        for (raw, accepted) in cases {
            let parsed = parse_compose_version(raw);
            let got = parsed.map(|v| (v.major, v.minor) >= COMPOSE_FLOOR);
            assert_eq!(got, accepted, "{raw:?} parsed as {parsed:?}");
        }
        assert_eq!(
            parse_compose_version("v2.29.1-desktop.1"),
            Some(ComposeVersion {
                major: 2,
                minor: 29,
                patch: 1
            })
        );
    }

    #[test]
    fn a_usable_engine_passes_and_reports_its_id() {
        let engine = check(&usable()).unwrap();
        assert_eq!(engine.engine_id, "8ba78a3b-6b42-467d-b260-df6109c9505e");
        assert_eq!(engine.compose.to_string(), "2.29.1");
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "one row per case")]
    fn the_decision_table() {
        type Case = (
            &'static str,
            fn(&mut EngineFacts),
            Result<(), PreflightError>,
        );
        let cases: &[Case] = &[
            (
                "unix DOCKER_HOST",
                |f| f.docker_host = Some("unix:///run/user/1000/docker.sock".into()),
                Ok(()),
            ),
            (
                "empty DOCKER_HOST is unset",
                |f| f.docker_host = Some("".into()),
                Ok(()),
            ),
            (
                "tcp DOCKER_HOST",
                |f| f.docker_host = Some("tcp://example.invalid:2375".into()),
                Err(PreflightError::DockerHostNotLocal {
                    scheme: "tcp://".into(),
                }),
            ),
            (
                "ssh DOCKER_HOST hides the user",
                |f| f.docker_host = Some("ssh://root@build.example".into()),
                Err(PreflightError::DockerHostNotLocal {
                    scheme: "ssh://".into(),
                }),
            ),
            (
                "DOCKER_HOST wins over a missing binary",
                |f| {
                    f.docker_host = Some("tcp://x:2375".into());
                    f.docker_installed = false;
                },
                Err(PreflightError::DockerHostNotLocal {
                    scheme: "tcp://".into(),
                }),
            ),
            (
                "missing binary",
                |f| f.docker_installed = false,
                Err(PreflightError::DockerMissing),
            ),
            (
                "unknown context",
                |f| f.context_endpoint = None,
                Err(PreflightError::ContextUnknown),
            ),
            (
                "remote context",
                |f| f.context_endpoint = Some("tcp://10.0.0.5:2376".into()),
                Err(PreflightError::ContextNotLocal {
                    scheme: "tcp://".into(),
                }),
            ),
            (
                "ssh context",
                |f| f.context_endpoint = Some("ssh://me@host".into()),
                Err(PreflightError::ContextNotLocal {
                    scheme: "ssh://".into(),
                }),
            ),
            (
                "npipe context",
                |f| f.context_endpoint = Some("npipe:////./pipe/docker_engine".into()),
                Err(PreflightError::ContextNotLocal {
                    scheme: "npipe://".into(),
                }),
            ),
            (
                "unreachable server",
                |f| f.server_reachable = false,
                Err(PreflightError::ServerUnreachable {
                    endpoint: "unix:///var/run/docker.sock".into(),
                }),
            ),
            (
                "missing compose",
                |f| f.compose_version = None,
                Err(PreflightError::ComposeMissing),
            ),
            (
                "old compose",
                |f| f.compose_version = Some("2.19.9".into()),
                Err(PreflightError::ComposeTooOld {
                    found: ComposeVersion {
                        major: 2,
                        minor: 19,
                        patch: 9,
                    },
                }),
            ),
            (
                "garbled compose",
                |f| f.compose_version = Some("banana".into()),
                Err(PreflightError::ComposeUnparseable {
                    found: "banana".into(),
                }),
            ),
            (
                "desktop compose",
                |f| f.compose_version = Some("v2.29.1-desktop.1".into()),
                Ok(()),
            ),
            (
                "compose 5",
                |f| f.compose_version = Some("5.5.1".into()),
                Ok(()),
            ),
            (
                "no engine id",
                |f| f.engine_id = None,
                Err(PreflightError::EngineIdUnknown),
            ),
            (
                "blank engine id",
                |f| f.engine_id = Some(" \n".into()),
                Err(PreflightError::EngineIdUnknown),
            ),
        ];
        for (name, edit, expected) in cases {
            let mut facts = usable();
            edit(&mut facts);
            assert_eq!(&check(&facts).map(|_| ()), expected, "{name}");
        }
    }

    #[test]
    fn every_refusal_names_the_requirement() {
        let messages = [
            PreflightError::DockerMissing.to_string(),
            PreflightError::ComposeMissing.to_string(),
            PreflightError::ComposeTooOld {
                found: ComposeVersion {
                    major: 2,
                    minor: 19,
                    patch: 9,
                },
            }
            .to_string(),
        ];
        for message in messages {
            assert!(message.contains("2.20 or later"), "{message}");
        }
        let remote = PreflightError::DockerHostNotLocal {
            scheme: "tcp://".into(),
        }
        .to_string();
        assert!(
            remote.contains("DOCKER_HOST") && remote.contains("unix://"),
            "{remote}"
        );
    }

    /// Answers the four probes the way Docker 29 with Compose 5.5.1 does.
    const HEALTHY: &str = r#"
case "$*" in
  "context inspect") printf '[{"Name":"default","Endpoints":{"docker":{"Host":"unix:///var/run/docker.sock"}}}]' ;;
  "version --format json") printf '{"Client":{"Version":"29.7.2"},"Server":{"Version":"29.7.2"}}' ;;
  "compose version --short") echo 5.5.1 ;;
  "info --format {{.ID}}") echo 8ba78a3b-6b42-467d-b260-df6109c9505e ;;
  *) echo "unexpected: $*" >&2; exit 99 ;;
esac"#;

    #[tokio::test]
    async fn a_remote_docker_host_is_refused_before_any_subprocess() {
        let (tmp, docker) = stub(r#"touch "$(dirname "$0")/ran""#);
        let err = preflight(&docker, Some("tcp://example.invalid:2375".into()))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("DOCKER_HOST points at a tcp://"),
            "{err}"
        );
        assert!(!tmp.path().join("ran").exists(), "docker was run");

        let (_tmp, docker) = stub(HEALTHY);
        let engine = preflight(&docker, Some("unix:///var/run/docker.sock".into()))
            .await
            .unwrap();
        assert_eq!(engine.compose.to_string(), "5.5.1");
    }

    #[tokio::test]
    async fn gather_reads_the_four_probes() {
        let (_tmp, docker) = stub(HEALTHY);
        let facts = gather(&docker, None).await.unwrap();
        assert_eq!(
            facts,
            EngineFacts {
                docker_host: None,
                docker_installed: true,
                context_endpoint: Some("unix:///var/run/docker.sock".into()),
                server_reachable: true,
                compose_version: Some("5.5.1".into()),
                engine_id: Some("8ba78a3b-6b42-467d-b260-df6109c9505e".into()),
            }
        );
        assert!(check(&facts).is_ok());
    }

    #[tokio::test]
    async fn gather_never_contacts_a_remote_context() {
        let (tmp, docker) = stub(&format!(
            r#"echo "$*" >> {log}
case "$*" in
  "context inspect") printf '[{{"Endpoints":{{"docker":{{"Host":"tcp://10.0.0.5:2376"}}}}}}]' ;;
  *) exit 99 ;;
esac"#,
            log = "\"$(dirname \"$0\")/calls\""
        ));
        let facts = gather(&docker, None).await.unwrap();
        assert_eq!(
            check(&facts),
            Err(PreflightError::ContextNotLocal {
                scheme: "tcp://".into()
            })
        );
        let calls = std::fs::read_to_string(tmp.path().join("calls")).unwrap();
        assert_eq!(calls, "context inspect\n");
    }

    #[tokio::test]
    async fn gather_reports_an_unreachable_server_and_missing_compose() {
        let (_tmp, docker) = stub(
            r#"case "$*" in
  "context inspect") printf '[{"Endpoints":{"docker":{"Host":"unix:///nope.sock"}}}]' ;;
  "version --format json") printf '{"Client":{},"Server":null}'; exit 1 ;;
  *) exit 99 ;;
esac"#,
        );
        let facts = gather(&docker, None).await.unwrap();
        assert_eq!(
            check(&facts),
            Err(PreflightError::ServerUnreachable {
                endpoint: "unix:///nope.sock".into()
            })
        );

        let (_tmp, docker) = stub(
            r#"case "$*" in
  "context inspect") printf '[{"Endpoints":{"docker":{"Host":"unix:///s.sock"}}}]' ;;
  "version --format json") printf '{"Server":{"Version":"29"}}' ;;
  "compose version --short") echo "docker: 'compose' is not a docker command." >&2; exit 1 ;;
  "info --format {{.ID}}") echo ID ;;
esac"#,
        );
        let facts = gather(&docker, None).await.unwrap();
        assert_eq!(check(&facts), Err(PreflightError::ComposeMissing));
    }
}
