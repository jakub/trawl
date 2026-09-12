// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `public_origins` packaging contract.
//!
//! `[web] public_origins` has no default: an install that does not state
//! its browser-visible origin does not start. That makes every packaged
//! spelling of the knob load-bearing, and a wrong one is not a typo but an
//! outage. Four artifacts state it: the Debian `trawld.toml`, the
//! configuration reference, the operator guides, and the Helm chart. This file feeds each
//! of them to the real parsers rather than eyeballing them.
//!
//! Precedent: `crates/trawl-web/tests/log_filter_contract.rs` and
//! `crates/trawl-config/tests/derivation_packaging_contract.rs`. The chart
//! is read through `helm template` when helm is on PATH, which is the real
//! rendering, values plumbing included; only a missing binary falls back to
//! `values.yaml`. A chart that fails to render when it should is a test
//! failure, never a skip.

use std::path::{Path, PathBuf};
use std::process::Command;

use fleet_auth::PublicOrigins;
use trawl_config::Config;

/// An origin an operator is told to write. Every example in every packaged
/// artifact has to be one `PublicOrigins::parse` accepts, so this is the
/// value the chart cases are rendered with.
const EXAMPLE_ORIGIN: &str = "https://trawl.example.com";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root must exist relative to the crate")
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The double-quoted strings of a TOML array literal, in order.
///
/// Deliberately not a TOML parse: the docs page states the knob inside
/// fenced code blocks, and the packaged files comment most of their knobs
/// out. Same reader as
/// `trawl-config/tests/derivation_packaging_contract.rs`, for the same
/// reason.
fn quoted_strings(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else { break };
        out.push(after[..close].to_owned());
        rest = &after[close + 1..];
    }
    out
}

/// Every `public_origins = [ … ]` declaration in a document, in order.
fn declared_origin_lists(source: &str) -> Vec<Vec<String>> {
    source
        .lines()
        .map(|line| line.trim_start_matches(['#', ' ']))
        .filter(|line| line.starts_with("public_origins = ["))
        .map(quoted_strings)
        .collect()
}

/// Every `publicOrigins:` list in a YAML example, in order: the `- item`
/// lines that follow the key, trimmed.
fn yaml_origin_lists(source: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut lines = source.lines().map(str::trim).peekable();
    while let Some(line) = lines.next() {
        if line != "publicOrigins:" {
            continue;
        }
        let mut entries = Vec::new();
        while let Some(item) = lines.peek().and_then(|next| next.strip_prefix("- ")) {
            entries.push(item.trim().to_owned());
            lines.next();
        }
        out.push(entries);
    }
    out
}

/// Run `helm template` over the chart, returning the rendered manifest or
/// the error text helm refused with.
///
/// `None` means helm is not installed, which is the one case the callers
/// fall back on. Anything else is an answer about the chart.
fn helm_template(template: &str, extra: &[&str]) -> Option<Result<String, String>> {
    let chart = repo_root().join("chart/trawl");
    let output = Command::new("helm")
        .args(["template", "trawl"])
        .arg(&chart)
        .args(["--show-only", &format!("templates/{template}")])
        // Both DSN Secrets are `required`; the names are irrelevant here.
        .args(["--set", "auth.database.existingSecret=test-fleet-dsn"])
        .args(["--set", "storage.database.existingSecret=test-trawl-dsn"])
        .args(extra)
        .output();
    match output {
        Ok(out) if out.status.success() => Some(Ok(
            String::from_utf8(out.stdout).expect("helm output must be UTF-8")
        )),
        Ok(out) => Some(Err(String::from_utf8_lossy(&out.stderr).into_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("could not run helm: {e}"),
    }
}

/// The `web.publicOrigins` default as `values.yaml` states it: the empty
/// list. Only reached when helm is missing, and it still proves the half
/// of the contract a values file can prove — that the chart ships no
/// origin of its own, so the operator must state one.
fn values_declare_an_empty_default() {
    let values = read("chart/trawl/values.yaml");
    let declared = values
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("publicOrigins:"))
        .expect("values.yaml must declare web.publicOrigins");
    assert_eq!(
        declared, "publicOrigins: []",
        "the chart must ship no origin of its own"
    );
}

#[test]
fn the_debian_config_states_origins_that_load() {
    let debian = read("crates/trawl-server/debian/trawld.toml");
    let config: Config = toml::from_str(&debian).expect("the packaged trawld.toml must parse");

    assert!(
        !config.web.public_origins.is_empty(),
        "the .deb must ship an active public_origins: trawl-web does not start without one, \
         and the package enables trawl-web.service on install"
    );
    let origins = PublicOrigins::parse(&config.web.public_origins)
        .expect("the packaged origins must load through the real parser");

    // The packaged bind is loopback, and its two spellings are two
    // origins, so shipping only one would 403 half the operators who
    // browse to their own install.
    let rendered: Vec<String> = origins.iter().map(ToString::to_string).collect();
    for expected in ["http://127.0.0.1:8090", "http://localhost:8090"] {
        assert!(
            rendered.iter().any(|o| o == expected),
            "the packaged list must carry {expected}; got {rendered:?}"
        );
    }
}

#[test]
fn every_documented_example_loads() {
    // A documented origin that does not parse is worse than no example:
    // it gets pasted into a config file and refuses to boot.
    let docs = read("docs/src/content/docs/reference/configuration.md");
    let examples = declared_origin_lists(&docs);
    assert!(
        !examples.is_empty(),
        "the configuration reference must show public_origins at all"
    );
    let access = read("docs/src/content/docs/operate/access.md");
    let deployment = read("docs/src/content/docs/operate/deployment.md");
    let guides = declared_origin_lists(&access)
        .into_iter()
        .chain(declared_origin_lists(&deployment))
        .collect::<Vec<_>>();
    assert!(
        !guides.is_empty(),
        "the operator guides must show public_origins at all"
    );
    let helm = yaml_origin_lists(&deployment);
    assert!(
        !helm.is_empty(),
        "the deployment guide must show web.publicOrigins for Helm"
    );
    for entries in examples.iter().chain(&guides).chain(&helm) {
        assert!(!entries.is_empty(), "an empty example teaches nothing");
        PublicOrigins::parse(entries)
            .unwrap_or_else(|e| panic!("the docs document an unloadable list {entries:?}: {e}"));
    }

    // The Debian example and the docs are one fact, not two: an operator
    // reading either sees the list the package actually ships.
    let debian = read("crates/trawl-server/debian/trawld.toml");
    let packaged = declared_origin_lists(&debian);
    assert_eq!(packaged.len(), 1, "the .deb states the knob exactly once");
    assert!(
        examples.contains(&packaged[0]),
        "the reference must show the list the .deb ships ({:?})",
        packaged[0]
    );
}

#[test]
fn the_chart_renders_the_origins_it_is_given() {
    let Some(configmap) = helm_template(
        "configmap.yaml",
        &[
            "--set",
            "web.enabled=true",
            "--set-string",
            &format!("web.publicOrigins[0]={EXAMPLE_ORIGIN}"),
        ],
    ) else {
        values_declare_an_empty_default();
        return;
    };
    let configmap = configmap.expect("the chart must render with an origin set");
    let line = configmap
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("public_origins = ["))
        .expect("the generated [web] block must carry public_origins");
    assert_eq!(quoted_strings(line), vec![EXAMPLE_ORIGIN.to_owned()]);
    PublicOrigins::parse(quoted_strings(line)).expect("what the chart renders must load");

    // The sidecar gets the same list as an env var, so the allowlist
    // survives a config.raw that replaces the generated TOML wholesale.
    let statefulset = helm_template(
        "statefulset.yaml",
        &[
            "--set",
            "web.enabled=true",
            "--set-string",
            &format!("web.publicOrigins[0]={EXAMPLE_ORIGIN}"),
            "--set-string",
            "config.raw=[server]",
        ],
    )
    .expect("helm was on PATH a moment ago")
    .expect("the chart must render with an origin set");
    assert!(
        statefulset.contains(&format!("value: \"{EXAMPLE_ORIGIN}\"")),
        "the sidecar must receive FLEET_SESSION_PUBLIC_ORIGINS"
    );
    assert!(
        statefulset.contains("name: FLEET_SESSION_PUBLIC_ORIGINS"),
        "the sidecar must receive FLEET_SESSION_PUBLIC_ORIGINS"
    );
}

#[test]
fn the_chart_refuses_an_enabled_sidecar_with_no_origins() {
    let Some(rendered) = helm_template("statefulset.yaml", &["--set", "web.enabled=true"]) else {
        values_declare_an_empty_default();
        return;
    };
    let error = rendered.expect_err(
        "web.enabled with an empty publicOrigins must fail the render: an install that \
         cannot answer a browser is not a deployable one",
    );
    assert!(
        error.contains("web.publicOrigins"),
        "the refusal must name the knob to set; got: {error}"
    );
}

#[test]
fn the_chart_quick_start_forwards_the_port_its_origin_names() {
    // The quick start tells the operator to allow http://localhost:8090
    // and then hands them a port-forward command. If that command does not
    // map 8090 locally, the browser never reaches the UI on the origin the
    // allowlist just authorized, and the first thing a new install does is
    // fail in a way that looks like the CSRF guard misfiring.
    let readme = read("chart/trawl/README.md");
    let quick_start = readme
        .split("## Quick start")
        .nth(1)
        .expect("the README must have a quick start")
        .split("\n## ")
        .next()
        .expect("splitting always yields a first piece");

    let origin = quick_start
        .lines()
        .find_map(|line| line.split("web.publicOrigins[0]=").nth(1))
        .map(|rest| rest.trim().trim_end_matches(['\'', '"', '\\', ' ']))
        .expect("the quick start must set web.publicOrigins");
    PublicOrigins::parse([origin]).expect("the quick start's origin must load");
    let port = origin
        .rsplit_once(':')
        .map(|(_, port)| port)
        .filter(|port| port.chars().all(|c| c.is_ascii_digit()))
        .expect("the quick start states a loopback origin with an explicit port");

    let forward = quick_start
        .lines()
        .find(|line| line.contains("kubectl port-forward"))
        .expect("the quick start must show a port-forward");
    assert!(
        forward
            .split_whitespace()
            .any(|arg| arg.starts_with(&format!("{port}:"))),
        "the quick start allows {origin} but forwards {forward:?}, which never binds {port} \
         locally"
    );
}
