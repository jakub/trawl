// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the trial verbs print, as pure writers.
//!
//! Every function here takes the trial state and paths and writes to a
//! `&mut impl Write`, so a snapshot test pins the exact text. None of them
//! takes a token or a key prefix as input: the state's key records reach
//! [`render_status`] only as "minted" or "not yet", so no output of these
//! functions can carry one.

use std::io::{self, Write};
use std::path::Path;

use super::keys::TrialKey;
use super::ownership::{Inventory, Kind};
use super::paths::TrialPaths;
use super::sample::{DOCUMENTED_QUERY, documented_row};
use super::state::{ImageRecord, Samples, TrialState};
use super::{CLAIM_NAME, PROFILE};

/// The browser address: `localhost`, the first of the trial's origins.
pub fn browser_url(state: &TrialState) -> String {
    format!("http://localhost:{}", state.ports.web)
}

/// The API address `-p trial` connects to.
pub fn api_url(state: &TrialState) -> String {
    format!("https://127.0.0.1:{}", state.ports.api)
}

/// Quote `text` for a POSIX shell, so a printed command can be pasted.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// What `up` prints on stdout once the trial answered an authenticated
/// query.
pub fn render_summary(
    out: &mut impl Write,
    state: &TrialState,
    paths: &TrialPaths,
) -> io::Result<()> {
    writeln!(out, "The trial is up.")?;
    writeln!(out)?;
    writeln!(out, "  Browser   {}", browser_url(state))?;
    writeln!(out, "  API       {}", api_url(state))?;
    if state.images.trawl_overridden {
        writeln!(
            out,
            "  Image     {} (--image)",
            state.images.trawl.reference
        )?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "Sign in to the browser with the operator key; `trawl trial key` prints it."
    )?;
    writeln!(out, "The key files are readable by your user only:")?;
    writeln!(out)?;
    writeln!(out, "  operator  {}", paths.operator_token_file().display())?;
    writeln!(out, "  ingest    {}", paths.ingest_token_file().display())?;
    writeln!(out)?;
    match &state.samples {
        Samples::Complete {
            first, last, total, ..
        } => {
            writeln!(out, "Sample data: {total} events from {first} to {last}.")?;
            writeln!(out)?;
            writeln!(out, "Run the documented query:")?;
            writeln!(out)?;
            writeln!(
                out,
                "  trawl -p {PROFILE} query {}",
                shell_quote(DOCUMENTED_QUERY)
            )?;
            writeln!(out)?;
            let row = documented_row();
            writeln!(
                out,
                "It returns one row: service {}, errors {}.",
                row["service"].as_str().unwrap_or_default(),
                row["errors"]
            )?;
        }
        Samples::Skipped | Samples::NotRequested => {
            writeln!(
                out,
                "No sample data. `trawl trial up` without --no-sample-data adds it."
            )?;
        }
        Samples::Intent { .. } => {
            writeln!(
                out,
                "Sample data was posted but not verified; `trawl trial status` shows it."
            )?;
        }
    }
    writeln!(out)?;
    writeln!(out, "Next:")?;
    writeln!(out)?;
    for (command, what) in [
        (
            "trawl trial key",
            "print the operator token for the browser sign-in",
        ),
        ("trawl -p trial", "open the terminal UI on the trial"),
        ("trawl trial status", "show the trial's state"),
        (
            "trawl trial stop",
            "stop the containers; `trawl trial up` resumes",
        ),
        (
            "trawl trial down",
            "delete the trial and everything it made",
        ),
    ] {
        writeln!(out, "  {command:<20}{what}")?;
    }
    Ok(())
}

/// The trial's containers as `status` shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Containers {
    /// `(name, state)` of each container carrying the trial id, one-off
    /// containers excluded.
    Listed(Vec<(String, String)>),
    /// Docker could not be asked; the reason.
    Unknown(String),
}

/// What `status` prints on stdout.
pub fn render_status(
    out: &mut impl Write,
    state: &TrialState,
    paths: &TrialPaths,
    containers: &Containers,
) -> io::Result<()> {
    writeln!(out, "Trial {}", state.trial_id)?;
    writeln!(
        out,
        "  created      {} by trawl {}",
        state.created_at, state.cli_version
    )?;
    writeln!(out, "  directory    {}", paths.dir.display())?;
    writeln!(out, "  engine       {}", state.engine_id)?;
    writeln!(out)?;
    writeln!(out, "Addresses")?;
    writeln!(out, "  browser      {}", browser_url(state))?;
    writeln!(
        out,
        "  API          {} (trawl -p {PROFILE})",
        api_url(state)
    )?;
    writeln!(out)?;
    writeln!(out, "Images")?;
    let overridden = if state.images.trawl_overridden {
        " (--image)"
    } else {
        ""
    };
    image(out, "trawl", &state.images.trawl, overridden)?;
    image(out, "postgres", &state.images.postgres, "")?;
    writeln!(out)?;
    writeln!(out, "Certificate")?;
    match &state.tls {
        Some(tls) => writeln!(out, "  SHA-256      {}", tls.sha256_fingerprint)?,
        None => writeln!(out, "  not generated yet")?,
    }
    writeln!(out)?;
    writeln!(out, "Samples")?;
    match &state.samples {
        Samples::NotRequested => writeln!(out, "  not loaded yet")?,
        Samples::Skipped => writeln!(out, "  none (--no-sample-data)")?,
        Samples::Intent {
            anchor, expected, ..
        } => {
            let total: u64 = expected.values().sum();
            writeln!(
                out,
                "  {total} events ending {anchor} were posted, and the result is not verified"
            )?;
        }
        Samples::Complete {
            first, last, total, ..
        } => writeln!(out, "  {total} events from {first} to {last}")?,
    }
    writeln!(out)?;
    writeln!(out, "Setup")?;
    let done = |flag: bool| if flag { "done" } else { "not yet" };
    let phases = &state.phases;
    writeln!(out, "  databases    {}", done(phases.database))?;
    writeln!(out, "  Fleet schema {}", done(phases.fleet_migrated))?;
    writeln!(out, "  certificate  {}", done(phases.tls))?;
    for key in TrialKey::ALL {
        let (label, record) = match key {
            TrialKey::Operator => ("operator key", &state.keys.operator),
            TrialKey::Ingest => ("ingest key", &state.keys.ingest),
        };
        let minted = if record.is_some() {
            "minted"
        } else {
            "not yet"
        };
        writeln!(out, "  {label:<12} {minted}")?;
    }
    writeln!(
        out,
        "  services     {}",
        if phases.services_verified {
            "verified"
        } else {
            "not verified yet"
        }
    )?;
    writeln!(out)?;
    writeln!(out, "Containers")?;
    match containers {
        Containers::Listed(list) if list.is_empty() => writeln!(out, "  none")?,
        Containers::Listed(list) => {
            let width = list.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
            for (name, state) in list {
                writeln!(out, "  {name:<width$}  {state}")?;
            }
        }
        Containers::Unknown(reason) => writeln!(out, "  unknown: {reason}")?,
    }
    Ok(())
}

fn image(out: &mut impl Write, role: &str, record: &ImageRecord, note: &str) -> io::Result<()> {
    writeln!(out, "  {role:<12} {}{note}", record.reference)?;
    writeln!(out, "  {:<12} id {}", "", record.id)?;
    match &record.repo_digest {
        Some(digest) => writeln!(out, "  {:<12} digest {digest}", ""),
        None => writeln!(out, "  {:<12} digest none (a locally built image)", ""),
    }
}

/// The connection variables whose presence changes what `trawl -p trial`
/// does.
pub const CONNECTION_VARIABLES: [&str; 4] = [
    "TRAWL_URL",
    "TRAWL_TOKEN",
    "TRAWL_INSECURE",
    "TRAWL_PROFILE",
];

/// One stderr warning per set connection variable, by name only.
pub fn render_env_warnings(out: &mut impl Write, set: &[&str]) -> io::Result<()> {
    for name in set {
        let effect = match *name {
            "TRAWL_PROFILE" => "commands without -p use that profile, not the trial",
            "TRAWL_INSECURE" => {
                "`trawl -p trial` refuses to run, because the trial is reached only through its pinned certificate"
            }
            _ => "`trawl -p trial` refuses to run until it is unset",
        };
        writeln!(out, "trawl: warning: {name} is set, so {effect}")?;
    }
    Ok(())
}

/// What `down` prints before it asks: every resource it will delete and
/// the trial directory.
pub fn render_inventory(out: &mut impl Write, inventory: &Inventory, dir: &Path) -> io::Result<()> {
    let mut rows: Vec<(Kind, &str, &str)> = inventory
        .resources
        .iter()
        .map(|r| (r.kind, r.name.as_str(), r.state.as_str()))
        .collect();
    // In the order `down` removes them: containers, networks, volumes,
    // and the claim last.
    let rank = |kind: Kind, name: &str| match kind {
        Kind::Container if name == CLAIM_NAME => 3,
        Kind::Container => 0,
        Kind::Network => 1,
        Kind::Volume => 2,
    };
    rows.sort_by_key(|(kind, name, _)| (rank(*kind, name), *name));
    if rows.is_empty() {
        writeln!(out, "The trial has no Docker resources left.")?;
    } else {
        writeln!(out, "`trawl trial down` deletes these Docker resources:")?;
        writeln!(out)?;
        let width = rows
            .iter()
            .map(|(_, name, _)| name.len())
            .max()
            .unwrap_or(0);
        for (kind, name, state) in rows {
            let kind = kind.to_string();
            if state.is_empty() {
                writeln!(out, "  {kind:<10}{name}")?;
            } else {
                writeln!(out, "  {kind:<10}{name:<width$}  {state}")?;
            }
        }
    }
    writeln!(out)?;
    writeln!(
        out,
        "and the trial directory, with its state and token files:"
    )?;
    writeln!(out)?;
    writeln!(out, "  {}", dir.display())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trial::ownership::Resource;
    use crate::trial::state::tests::fixture;

    fn paths() -> TrialPaths {
        TrialPaths::resolve(Some(Path::new("/state")), None).unwrap()
    }

    fn text(write: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> String {
        let mut out = Vec::new();
        write(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn containers() -> Containers {
        Containers::Listed(vec![
            ("trawl-trial-postgres-1".into(), "running".into()),
            ("trawl-trial-trawld-1".into(), "running".into()),
            ("trawl-trial-trawl-web-1".into(), "exited".into()),
            ("trawl-trial-claim".into(), "created".into()),
        ])
    }

    #[test]
    fn the_status_of_a_fixture_trial() {
        let state = fixture(crate::trial::DEFAULT_API_PORT);
        insta::assert_snapshot!(text(|out| render_status(
            out,
            &state,
            &paths(),
            &containers()
        )));
    }

    #[test]
    fn status_when_docker_cannot_be_asked() {
        let mut state = fixture(crate::trial::DEFAULT_API_PORT);
        state.samples = Samples::Intent {
            seed: 203,
            anchor: "2026-09-25T12:00:00.000Z".into(),
            expected: [("web".to_owned(), 700), ("tutorial".to_owned(), 3)].into(),
        };
        state.images.trawl_overridden = true;
        state.tls = None;
        let status = text(|out| {
            render_status(
                out,
                &state,
                &paths(),
                &Containers::Unknown("Docker Engine is not reachable".into()),
            )
        });
        assert!(
            status.contains("703 events ending 2026-09-25T12:00:00.000Z were posted"),
            "{status}"
        );
        assert!(status.contains("(--image)"), "{status}");
        assert!(status.contains("not generated yet"), "{status}");
        assert!(
            status.contains("unknown: Docker Engine is not reachable"),
            "{status}"
        );
    }

    #[test]
    fn the_summary_of_a_fixture_trial() {
        let state = fixture(crate::trial::DEFAULT_API_PORT);
        insta::assert_snapshot!(text(|out| render_summary(out, &state, &paths())));
    }

    /// No output carries a key prefix, and the summary names the token
    /// files, never a token.
    #[test]
    fn no_output_carries_a_prefix_or_a_token() {
        let state = fixture(crate::trial::DEFAULT_API_PORT);
        let prefix = &state.keys.operator.as_ref().unwrap().prefix;
        let outputs = [
            text(|out| render_summary(out, &state, &paths())),
            text(|out| render_status(out, &state, &paths(), &containers())),
        ];
        for output in &outputs {
            assert!(!output.contains(prefix.as_str()), "{output}");
            assert!(!output.contains("flt_"), "{output}");
        }
        assert!(outputs[0].contains("/state/trawl/trial/operator.token"));
        assert!(outputs[0].contains("/state/trawl/trial/ingest.token"));
    }

    #[test]
    fn the_summary_follows_the_ports_and_the_samples() {
        let mut state = fixture(25514);
        state.ports.web = 28090;
        let summary = text(|out| render_summary(out, &state, &paths()));
        assert!(summary.contains("http://localhost:28090"), "{summary}");
        assert!(summary.contains("https://127.0.0.1:25514"), "{summary}");
        assert!(
            summary.contains(&format!("query '{DOCUMENTED_QUERY}'")),
            "{summary}"
        );

        state.samples = Samples::Skipped;
        let summary = text(|out| render_summary(out, &state, &paths()));
        assert!(!summary.contains(DOCUMENTED_QUERY), "{summary}");
        assert!(summary.contains("No sample data"), "{summary}");
    }

    #[test]
    fn env_warnings_name_variables_only() {
        let warnings = text(|out| render_env_warnings(out, &["TRAWL_URL", "TRAWL_PROFILE"]));
        assert_eq!(warnings.lines().count(), 2);
        assert!(warnings.starts_with("trawl: warning: TRAWL_URL is set, so"));
        assert!(text(|out| render_env_warnings(out, &[])).is_empty());
    }

    #[test]
    fn the_inventory_lists_the_claim_last() {
        let resource = |kind, name: &str, state: &str| Resource {
            kind,
            id: format!("{name}-id"),
            name: name.into(),
            project: None,
            trial_id: Some("x".into()),
            oneoff: false,
            state: state.into(),
        };
        let inventory = Inventory {
            resources: vec![
                resource(Kind::Container, CLAIM_NAME, "created"),
                resource(Kind::Volume, "trawl-trial_postgres", ""),
                resource(Kind::Container, "trawl-trial-postgres-1", "running"),
                resource(Kind::Network, "trawl-trial_default", ""),
            ],
        };
        insta::assert_snapshot!(text(|out| render_inventory(
            out,
            &inventory,
            Path::new("/state/trawl/trial")
        )));
    }

    #[test]
    fn shell_quote_survives_a_quote() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
