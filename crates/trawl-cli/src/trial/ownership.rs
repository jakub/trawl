// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Which Docker resources are the trial's, decided from their labels.
//!
//! The trial acts only on what carries its id (ADR-0045). The
//! [`Inventory`] is every container, volume, and network in the union of:
//!
//! - the Compose project label `com.docker.compose.project=trawl-trial`,
//! - the trial id label, whatever its value,
//! - a name the trial reserves: the engine claim container, and the names
//!   Compose gives the project's volumes and network. Compose adopts an
//!   existing volume or network of that name with only a warning, so an
//!   unlabelled one there must be refused, not reused.
//!
//! [`classify`] calls a resource in that union foreign when its id label is
//! missing or differs from ours, and every mutating verb refuses before
//! mutation when anything is foreign.
//!
//! One trial per engine is enforced by the claim: a container named
//! [`CLAIM_NAME`], labelled with the trial id, created and never started
//! before any other resource. A second trial's create fails on the name.
//! The claim carries no Compose project label, so Compose never treats it
//! as an orphan.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

use super::compose::VOLUMES;
use super::docker::{Args, Docker, PROBE_TIMEOUT, Sensitivity, failure};
use super::{CLAIM_NAME, LABEL_ID, PROJECT};

/// Compose's project label.
const PROJECT_LABEL: &str = "com.docker.compose.project";
/// Compose's label on `compose run` containers.
const ONEOFF_LABEL: &str = "com.docker.compose.oneoff";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Container,
    Volume,
    Network,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Container => "container",
            Self::Volume => "volume",
            Self::Network => "network",
        })
    }
}

/// A container, volume, or network in the trial's union.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub kind: Kind,
    /// The full ID; a volume's name is its ID.
    pub id: String,
    pub name: String,
    /// The Compose project label, when present and non-empty.
    pub project: Option<String>,
    /// The trial id label, when present and non-empty.
    pub trial_id: Option<String>,
    /// A `compose run` container.
    pub oneoff: bool,
    /// A container's state (`running`, `exited`, ...); empty otherwise.
    pub state: String,
}

impl Resource {
    fn in_union(&self) -> bool {
        self.project.as_deref() == Some(PROJECT)
            || self.trial_id.is_some()
            || reserved_names(self.kind).contains(&self.name)
    }
}

/// The names the trial reserves for a kind of resource.
fn reserved_names(kind: Kind) -> Vec<String> {
    match kind {
        Kind::Container => vec![CLAIM_NAME.to_owned()],
        Kind::Volume => VOLUMES.iter().map(|v| format!("{PROJECT}_{v}")).collect(),
        Kind::Network => vec![format!("{PROJECT}_default")],
    }
}

/// The trial's union of resources on the engine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    pub resources: Vec<Resource>,
}

/// One line of the listing formats below.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Listed {
    #[serde(default)]
    id: Option<String>,
    name: String,
    project: String,
    trial: String,
    #[serde(default)]
    oneoff: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

/// A `--format` template printing one JSON object per resource: each
/// field is a Go template `json` call, so the line is JSON whatever the
/// values hold. `.Label` prints an empty string for a missing label.
fn format(fields: &[(&str, &str)]) -> String {
    let fields: Vec<String> = fields
        .iter()
        .map(|(key, template)| format!(r#""{key}":{{{{json {template}}}}}"#))
        .collect();
    format!("{{{}}}", fields.join(","))
}

fn label(key: &str) -> String {
    format!(r#"(.Label "{key}")"#)
}

/// Every container, stopped ones included, with full IDs.
fn containers_args() -> Args {
    let format = format(&[
        ("id", ".ID"),
        ("name", ".Names"),
        ("state", ".State"),
        ("project", &label(PROJECT_LABEL)),
        ("trial", &label(LABEL_ID)),
        ("oneoff", &label(ONEOFF_LABEL)),
    ]);
    Args::new().args(["ps", "--all", "--no-trunc", "--format", &format])
}

fn volumes_args() -> Args {
    let format = format(&[
        ("name", ".Name"),
        ("project", &label(PROJECT_LABEL)),
        ("trial", &label(LABEL_ID)),
    ]);
    Args::new().args(["volume", "ls", "--format", &format])
}

/// Every network, with full IDs.
fn networks_args() -> Args {
    let format = format(&[
        ("id", ".ID"),
        ("name", ".Name"),
        ("project", &label(PROJECT_LABEL)),
        ("trial", &label(LABEL_ID)),
    ]);
    Args::new().args(["network", "ls", "--no-trunc", "--format", &format])
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OwnershipError {
    #[error("docker listed a {kind} trawl cannot read (line {line})")]
    Listing { kind: Kind, line: usize },

    #[error(
        "Docker resources that are not this trial's use the trial's names or labels:\n{}\n\
         The trial acts only on resources labelled with its own id. Remove these \
         yourself if they are leftovers (for example `docker rm`, `docker volume rm`, \
         `docker network rm`), or leave them and do not run a trial on this engine",
        list(.resources)
    )]
    Foreign { resources: Vec<Foreign> },
}

fn list(resources: &[Foreign]) -> String {
    resources
        .iter()
        .map(|r| format!("  {r}"))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Inventory {
    /// Build the inventory from the three listings, keeping the union.
    pub fn from_listings(
        containers: &str,
        volumes: &str,
        networks: &str,
    ) -> Result<Self, OwnershipError> {
        let mut resources = Vec::new();
        for (kind, text) in [
            (Kind::Container, containers),
            (Kind::Volume, volumes),
            (Kind::Network, networks),
        ] {
            for (index, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let bad = || OwnershipError::Listing {
                    kind,
                    line: index + 1,
                };
                let listed: Listed = serde_json::from_str(line).map_err(|_| bad())?;
                let nonempty = |s: String| (!s.is_empty()).then_some(s);
                let id = match kind {
                    Kind::Volume => listed.name.clone(),
                    Kind::Container | Kind::Network => {
                        listed.id.filter(|id| !id.is_empty()).ok_or_else(bad)?
                    }
                };
                let resource = Resource {
                    kind,
                    id,
                    name: listed.name,
                    project: nonempty(listed.project),
                    trial_id: nonempty(listed.trial),
                    oneoff: listed.oneoff.as_deref() == Some("True"),
                    state: listed.state.unwrap_or_default(),
                };
                if resource.in_union() {
                    resources.push(resource);
                }
            }
        }
        Ok(Self { resources })
    }

    /// List the engine's containers, volumes, and networks and keep the
    /// trial's union.
    pub async fn scan(docker: &Docker) -> Result<Self, super::TrialError> {
        let mut texts = Vec::with_capacity(3);
        for args in [containers_args(), volumes_args(), networks_args()] {
            let out = docker
                .capture(&args, None, Sensitivity::Diagnose, PROBE_TIMEOUT)
                .await?;
            texts.push(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        Ok(Self::from_listings(&texts[0], &texts[1], &texts[2])?)
    }
}

/// A resource that is not ours, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Foreign {
    pub kind: Kind,
    pub name: String,
    /// The id it carries; `None` when it carries none.
    pub trial_id: Option<String>,
}

impl fmt::Display for Foreign {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.trial_id {
            Some(id) => write!(f, "{} {} (trial id {id})", self.kind, self.name),
            None => write!(f, "{} {} (no trial id label)", self.kind, self.name),
        }
    }
}

/// Whose resources are on the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// Nothing in the union.
    Empty,
    /// Everything in the union carries our id.
    Ours,
    /// At least one resource in the union is not ours.
    Foreign(Vec<Foreign>),
}

impl Ownership {
    /// Refuse when anything is foreign.
    pub fn require_not_foreign(self) -> Result<Self, OwnershipError> {
        match self {
            Self::Foreign(resources) => Err(OwnershipError::Foreign { resources }),
            other => Ok(other),
        }
    }
}

/// Classify the inventory against our trial id. With no id (no state on
/// this host), every resource in the union is foreign: a trial never
/// adopts what it cannot prove it made.
pub fn classify(inventory: &Inventory, our_id: Option<&str>) -> Ownership {
    if inventory.resources.is_empty() {
        return Ownership::Empty;
    }
    let mut foreign: Vec<Foreign> = inventory
        .resources
        .iter()
        .filter(|r| our_id.is_none() || r.trial_id.as_deref() != our_id)
        .map(|r| Foreign {
            kind: r.kind,
            name: r.name.clone(),
            trial_id: r.trial_id.clone(),
        })
        .collect();
    if foreign.is_empty() {
        return Ownership::Ours;
    }
    foreign.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    Ownership::Foreign(foreign)
}

/// Our `compose run` containers that have not finished: a killed `up` can
/// leave one running (a `keys create` that commits after the rerun lists
/// keys), so `up`, `stop`, and `down` wait for these, then refuse.
pub fn unfinished_oneoffs<'a>(inventory: &'a Inventory, our_id: &str) -> Vec<&'a Resource> {
    inventory
        .resources
        .iter()
        .filter(|r| {
            r.kind == Kind::Container
                && r.oneoff
                && r.trial_id.as_deref() == Some(our_id)
                && !matches!(r.state.as_str(), "created" | "exited" | "dead")
        })
        .collect()
}

/// `docker container create` of the claim: never started, no ports, no
/// restart policy, and no Compose label.
pub fn claim_create_args(trial_id: &str, image_id: &str) -> Args {
    Args::new()
        .args(["container", "create", "--name", CLAIM_NAME, "--label"])
        .arg(format!("{LABEL_ID}={trial_id}"))
        .args([image_id, "true"])
}

/// `docker container inspect` of the claim's labels.
pub fn claim_inspect_args() -> Args {
    Args::new().args([
        "container",
        "inspect",
        "--format",
        "{{json .Config.Labels}}",
        CLAIM_NAME,
    ])
}

/// What an existing claim means for us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimDecision {
    /// It carries our id: an earlier run of this trial made it.
    Proceed,
    /// Another trial, or something else, holds the engine.
    Refuse { holder: Option<String> },
}

/// Decide from the existing claim's labels (`docker container inspect`
/// `.Config.Labels` JSON; `null` when it has none).
pub fn decide_claim(labels_json: &[u8], our_id: &str) -> ClaimDecision {
    let labels: Option<BTreeMap<String, String>> =
        serde_json::from_slice(labels_json).unwrap_or_default();
    let holder = labels
        .and_then(|mut labels| labels.remove(LABEL_ID))
        .filter(|id| !id.is_empty());
    if holder.as_deref() == Some(our_id) {
        ClaimDecision::Proceed
    } else {
        ClaimDecision::Refuse { holder }
    }
}

/// How [`claim`] went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claimed {
    /// This call created the claim.
    Created,
    /// Our claim was already there.
    Existing,
}

/// Take the engine for `trial_id`, atomically: create the claim, and if
/// the name is taken, proceed only when the existing claim is ours.
pub async fn claim(
    docker: &Docker,
    trial_id: &str,
    image_id: &str,
) -> Result<Claimed, super::TrialError> {
    let create = claim_create_args(trial_id, image_id);
    let created = docker
        .output(&create, None, Sensitivity::Diagnose, PROBE_TIMEOUT)
        .await?;
    if created.status.success() {
        return Ok(Claimed::Created);
    }
    // The create failed. If a claim exists, that is why; if none does, the
    // create's own error is the one to report.
    let existing = docker
        .output(
            &claim_inspect_args(),
            None,
            Sensitivity::Diagnose,
            PROBE_TIMEOUT,
        )
        .await?;
    if !existing.status.success() {
        return Err(failure(
            &create,
            created.status,
            &created.stderr,
            Sensitivity::Diagnose,
        )
        .into());
    }
    match decide_claim(&existing.stdout, trial_id) {
        ClaimDecision::Proceed => Ok(Claimed::Existing),
        ClaimDecision::Refuse { holder } => Err(OwnershipError::Foreign {
            resources: vec![Foreign {
                kind: Kind::Container,
                name: CLAIM_NAME.to_owned(),
                trial_id: holder,
            }],
        }
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trial::docker::tests::stub;

    const OURS: &str = "0123456789abcdef0123456789abcdef";
    const THEIRS: &str = "fedcba9876543210fedcba9876543210";

    fn container(name: &str, project: &str, trial: &str, oneoff: &str, state: &str) -> String {
        serde_json::json!({
            "id": format!("{name}-id"),
            "name": name,
            "state": state,
            "project": project,
            "trial": trial,
            "oneoff": oneoff,
        })
        .to_string()
    }

    fn volume(name: &str, project: &str, trial: &str) -> String {
        serde_json::json!({ "name": name, "project": project, "trial": trial }).to_string()
    }

    fn network(name: &str, project: &str, trial: &str) -> String {
        serde_json::json!({ "id": format!("{name}-id"), "name": name, "project": project, "trial": trial })
            .to_string()
    }

    /// A full trial of ours, plus an unrelated project that must be
    /// ignored.
    fn our_trial() -> (String, String, String) {
        let containers = [
            container("trawl-trial-postgres-1", PROJECT, OURS, "False", "running"),
            container("trawl-trial-trawld-1", PROJECT, OURS, "False", "running"),
            container(CLAIM_NAME, "", OURS, "", "created"),
            container("coastwatch-db-1", "coastwatch", "", "False", "running"),
        ]
        .join("\n");
        let volumes = [
            volume("trawl-trial_postgres", PROJECT, OURS),
            volume("trawl-trial_trawld", PROJECT, OURS),
            volume("6b170652b65a", "", ""),
        ]
        .join("\n");
        let networks = [
            network("trawl-trial_default", PROJECT, OURS),
            network("bridge", "", ""),
        ]
        .join("\n");
        (containers, volumes, networks)
    }

    fn inventory(parts: &(String, String, String)) -> Inventory {
        Inventory::from_listings(&parts.0, &parts.1, &parts.2).unwrap()
    }

    #[test]
    fn nothing_in_the_union_is_empty() {
        let inv = Inventory::from_listings(
            &container("coastwatch-db-1", "coastwatch", "", "False", "running"),
            &volume("data", "", ""),
            &network("bridge", "", ""),
        )
        .unwrap();
        assert_eq!(inv, Inventory::default());
        assert_eq!(classify(&inv, Some(OURS)), Ownership::Empty);
        assert_eq!(classify(&inv, None), Ownership::Empty);
        assert_eq!(
            classify(&Inventory::from_listings("", "", "").unwrap(), None),
            Ownership::Empty
        );
    }

    #[test]
    fn our_own_trial_is_ours_and_unrelated_resources_are_ignored() {
        let inv = inventory(&our_trial());
        assert_eq!(inv.resources.len(), 6);
        assert!(
            inv.resources
                .iter()
                .all(|r| r.trial_id.as_deref() == Some(OURS))
        );
        assert_eq!(classify(&inv, Some(OURS)), Ownership::Ours);
        assert_eq!(inv.resources[0].id, "trawl-trial-postgres-1-id");
    }

    #[test]
    fn a_trial_with_no_state_here_owns_nothing() {
        let inv = inventory(&our_trial());
        let Ownership::Foreign(foreign) = classify(&inv, None) else {
            panic!("with no id every resource is foreign");
        };
        assert_eq!(foreign.len(), 6);
    }

    #[test]
    fn a_foreign_id_is_foreign() {
        let mut parts = our_trial();
        parts.2.push('\n');
        parts
            .2
            .push_str(&network("trawl-trial_default", PROJECT, THEIRS));
        let inv = inventory(&parts);
        assert_eq!(
            classify(&inv, Some(OURS)),
            Ownership::Foreign(vec![Foreign {
                kind: Kind::Network,
                name: "trawl-trial_default".into(),
                trial_id: Some(THEIRS.into()),
            }])
        );
    }

    #[test]
    fn a_project_resource_without_the_label_is_foreign() {
        let mut parts = our_trial();
        parts.0.push('\n');
        parts.0.push_str(&container(
            "trawl-trial-web-1",
            PROJECT,
            "",
            "False",
            "exited",
        ));
        let err = classify(&inventory(&parts), Some(OURS))
            .require_not_foreign()
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("container trawl-trial-web-1 (no trial id label)"),
            "{message}"
        );
        assert!(message.contains("docker rm"), "{message}");
    }

    /// Compose would adopt an unlabelled volume of the project's name
    /// with only a warning.
    #[test]
    fn an_unlabelled_resource_with_a_reserved_name_is_foreign() {
        let inv = Inventory::from_listings(
            "",
            &volume("trawl-trial_postgres", "", ""),
            &network("trawl-trial_default", "", ""),
        )
        .unwrap();
        let Ownership::Foreign(foreign) = classify(&inv, Some(OURS)) else {
            panic!("reserved names are in the union");
        };
        let names: Vec<_> = foreign.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["trawl-trial_postgres", "trawl-trial_default"]);
    }

    #[test]
    fn a_foreign_claim_is_foreign() {
        let inv =
            Inventory::from_listings(&container(CLAIM_NAME, "", THEIRS, "", "created"), "", "")
                .unwrap();
        assert_eq!(
            classify(&inv, Some(OURS)),
            Ownership::Foreign(vec![Foreign {
                kind: Kind::Container,
                name: CLAIM_NAME.into(),
                trial_id: Some(THEIRS.into()),
            }])
        );
        // A trial label outside the project is still in the union.
        let inv = Inventory::from_listings(
            &container("elsewhere", "other", THEIRS, "", "running"),
            "",
            "",
        )
        .unwrap();
        assert!(matches!(classify(&inv, Some(OURS)), Ownership::Foreign(_)));
    }

    #[test]
    fn an_unreadable_listing_is_an_error_naming_only_the_line() {
        let err = Inventory::from_listings("{\"id\":\"x\"}", "", "").unwrap_err();
        assert_eq!(
            err,
            OwnershipError::Listing {
                kind: Kind::Container,
                line: 1
            }
        );
        let err = Inventory::from_listings("", "", "\n\nnot json").unwrap_err();
        assert_eq!(
            err,
            OwnershipError::Listing {
                kind: Kind::Network,
                line: 3
            }
        );
    }

    #[test]
    fn unfinished_oneoffs_are_ours_and_not_done() {
        let containers = [
            container(
                "trawl-trial-fleet-admin-run-1",
                PROJECT,
                OURS,
                "True",
                "running",
            ),
            container(
                "trawl-trial-fleet-admin-run-2",
                PROJECT,
                OURS,
                "True",
                "exited",
            ),
            container(
                "trawl-trial-fleet-admin-run-3",
                PROJECT,
                OURS,
                "True",
                "paused",
            ),
            container("trawl-trial-trawld-1", PROJECT, OURS, "False", "running"),
            container(
                "trawl-trial-fleet-admin-run-4",
                PROJECT,
                THEIRS,
                "True",
                "running",
            ),
        ]
        .join("\n");
        let inv = Inventory::from_listings(&containers, "", "").unwrap();
        let names: Vec<_> = unfinished_oneoffs(&inv, OURS)
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "trawl-trial-fleet-admin-run-1",
                "trawl-trial-fleet-admin-run-3"
            ]
        );
    }

    /// The exact template checked against Docker 29 on the dev host.
    #[test]
    fn the_listing_templates() {
        assert_eq!(
            containers_args().display(),
            r#"docker ps --all --no-trunc --format {"id":{{json .ID}},"name":{{json .Names}},"state":{{json .State}},"project":{{json (.Label "com.docker.compose.project")}},"trial":{{json (.Label "sh.trawl.trial.id")}},"oneoff":{{json (.Label "com.docker.compose.oneoff")}}}"#
        );
        assert_eq!(
            volumes_args().display(),
            r#"docker volume ls --format {"name":{{json .Name}},"project":{{json (.Label "com.docker.compose.project")}},"trial":{{json (.Label "sh.trawl.trial.id")}}}"#
        );
        assert_eq!(
            networks_args().display(),
            r#"docker network ls --no-trunc --format {"id":{{json .ID}},"name":{{json .Name}},"project":{{json (.Label "com.docker.compose.project")}},"trial":{{json (.Label "sh.trawl.trial.id")}}}"#
        );
    }

    #[test]
    fn the_claim_is_created_with_our_label_and_no_compose_label() {
        let args = claim_create_args(OURS, "sha256:aaaa");
        assert_eq!(
            args.display(),
            format!(
                "docker container create --name trawl-trial-claim --label sh.trawl.trial.id={OURS} \
                 sha256:aaaa true"
            )
        );
        assert!(!args.display().contains(PROJECT_LABEL));
        assert!(!args.display().contains("restart"));
        assert!(!args.display().contains("-p"));
    }

    #[test]
    fn an_existing_claim_is_ours_or_refused() {
        let ours = format!(r#"{{"{LABEL_ID}":"{OURS}"}}"#);
        let theirs = format!(r#"{{"{LABEL_ID}":"{THEIRS}","other":"x"}}"#);
        assert_eq!(decide_claim(ours.as_bytes(), OURS), ClaimDecision::Proceed);
        assert_eq!(
            decide_claim(theirs.as_bytes(), OURS),
            ClaimDecision::Refuse {
                holder: Some(THEIRS.into())
            }
        );
        for unlabelled in [
            &b"null"[..],
            b"{}",
            br#"{"sh.trawl.trial.id":""}"#,
            b"garbage",
        ] {
            assert_eq!(
                decide_claim(unlabelled, OURS),
                ClaimDecision::Refuse { holder: None }
            );
        }
    }

    #[tokio::test]
    async fn claim_creates_proceeds_on_ours_and_refuses_theirs() {
        let (_tmp, docker) = stub("case \"$1 $2\" in \"container create\") exit 0 ;; esac; exit 9");
        assert_eq!(
            claim(&docker, OURS, "sha256:aaaa").await.unwrap(),
            Claimed::Created
        );

        let conflict = |holder: &str| {
            format!(
                r#"case "$1 $2" in
  "container create") echo 'Conflict. The container name "/trawl-trial-claim" is already in use' >&2; exit 1 ;;
  "container inspect") printf '{{"{LABEL_ID}":"{holder}"}}' ;;
esac"#
            )
        };
        let (_tmp, docker) = stub(&conflict(OURS));
        assert_eq!(
            claim(&docker, OURS, "sha256:aaaa").await.unwrap(),
            Claimed::Existing
        );

        let (_tmp, docker) = stub(&conflict(THEIRS));
        let err = claim(&docker, OURS, "sha256:aaaa").await.unwrap_err();
        assert!(
            err.to_string().contains(&format!("trial id {THEIRS}")),
            "{err}"
        );

        // No claim exists, so the create's own failure is reported.
        let (_tmp, docker) = stub(
            r#"case "$1 $2" in
  "container create") echo 'No such image: sha256:aaaa' >&2; exit 1 ;;
  "container inspect") echo 'No such container' >&2; exit 1 ;;
esac"#,
        );
        let err = claim(&docker, OURS, "sha256:aaaa").await.unwrap_err();
        assert!(err.to_string().contains("No such image"), "{err}");
    }

    #[tokio::test]
    async fn scan_reads_the_three_listings() {
        let (tmp, _) = stub("");
        let (c, v, n) = our_trial();
        for (file, text) in [("c", &c), ("v", &v), ("n", &n)] {
            std::fs::write(tmp.path().join(file), text).unwrap();
        }
        let (_keep, docker) = stub(&format!(
            r#"d="{dir}"
case "$1" in
  ps) cat "$d/c" ;;
  volume) cat "$d/v" ;;
  network) cat "$d/n" ;;
  *) exit 9 ;;
esac"#,
            dir = tmp.path().display()
        ));
        let inv = Inventory::scan(&docker).await.unwrap();
        assert_eq!(inv, inventory(&(c, v, n)));
    }
}
