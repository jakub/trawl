// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The derivation-source packaging contract (ADR-0013).
//!
//! `severity_from` and `time_from` decide what `_severity` and `_time`
//! read, and the answer must not depend on how trawl was installed. Five
//! places state that default — the code, the annotated `trawld.reference.toml`, the
//! Debian example, the Helm chart and the configuration reference — and
//! this test makes them one fact instead of five. Precedent:
//! `crates/trawl-web/tests/log_filter_contract.rs`.
//!
//! Reading convention: the packaged TOMLs and the docs page state the
//! default first and any typed-form example after it, so "the first
//! declaration in the file" is well-defined. The chart is live YAML, and
//! is read through `helm template` when helm is on PATH — the real
//! rendering, list plumbing included — falling back to `values.yaml`
//! only when the binary is missing. A chart that fails to render is a
//! failure, never a skip.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use trawl_config::{
    Config, DEFAULT_SEVERITY_FROM, DEFAULT_TIME_FROM, DerivationSourceSpec, IngestConfig,
};

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

/// The double-quoted strings in a TOML array literal, in order.
///
/// Deliberately not a TOML parse: the packaged files state their defaults
/// as commented-out lines, which is the whole idiom of those files (every
/// knob is shown at its default, commented out). The default lines hold
/// bare strings only — the typed `{ field = … }` form appears only in the
/// examples this function is never pointed at.
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

/// The first declaration of `key` in a TOML file, commented or not.
fn toml_default(source: &str, key: &str, origin: &str) -> Vec<String> {
    let marker = format!("{key} = [");
    let line = source
        .lines()
        .map(|l| l.trim_start_matches(['#', ' ']))
        .find(|l| l.starts_with(&marker))
        .unwrap_or_else(|| panic!("{origin} must state a default for {key}"));
    quoted_strings(line)
}

/// Every declaration of `key` in a TOML file — the default and each
/// example — with the leading comment marker stripped, so an example can
/// be handed back to the real deserializer.
fn toml_declarations<'a>(source: &'a str, key: &str) -> Vec<&'a str> {
    let marker = format!("{key} = [");
    source
        .lines()
        .map(|l| l.trim_start_matches(['#', ' ']))
        .filter(|l| l.starts_with(&marker))
        .collect()
}

/// The items of a simple YAML list nested under `parent:` → `key:`.
fn yaml_list(values: &str, parent: &str, key: &str) -> Vec<String> {
    let parent_marker = format!("{parent}:");
    let key_marker = format!("{key}:");
    let mut lines = values
        .lines()
        .skip_while(|l| l.trim() != parent_marker)
        .skip_while(|l| l.trim() != key_marker);
    lines.next().expect("the key line must exist");

    let mut out = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if let Some(item) = trimmed.strip_prefix("- ") {
            out.push(item.trim().trim_matches('"').to_owned());
        } else if !trimmed.starts_with('#') {
            break;
        }
    }
    out
}

/// What the chart actually installs, rendered when possible.
fn chart_defaults(key: &str, values_key: &str) -> Vec<String> {
    let chart = repo_root().join("chart/trawl");
    let rendered = Command::new("helm")
        .args(["template", "trawl"])
        .arg(&chart)
        .args(["--show-only", "templates/configmap.yaml"])
        .args(["--set-string", "image.tag=source-render-test"])
        // Both DSN Secrets are `required`; the names are irrelevant here.
        .args(["--set", "auth.database.existingSecret=test-fleet-dsn"])
        .args(["--set", "storage.database.existingSecret=test-trawl-dsn"])
        // The web sidecar refuses to render without a browser-origin
        // allowlist (ADR-0016); the value is irrelevant here.
        .args([
            "--set-string",
            "web.publicOrigins[0]=https://trawl.example.com",
        ])
        .output();
    match rendered {
        Ok(out) => {
            assert!(
                out.status.success(),
                "helm template failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let manifest = String::from_utf8(out.stdout).expect("helm output must be UTF-8");
            toml_default(&manifest, key, "the rendered chart ConfigMap")
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            yaml_list(&read("chart/trawl/values.yaml"), "ingest", values_key)
        }
        Err(e) => panic!("could not run helm: {e}"),
    }
}

/// The bare-string spellings of a configured list, for comparison with
/// the packaged defaults.
fn spellings(specs: &[DerivationSourceSpec]) -> Vec<String> {
    specs.iter().map(|s| s.field().to_owned()).collect()
}

#[test]
fn the_code_default_is_what_ingest_config_deserializes_to() {
    // The `#[serde(default = …)]` plumbing and the published constants
    // are two statements of one list; an omitted `[ingest]` block must
    // land on exactly the constants the packaging quotes.
    let config: IngestConfig = toml::from_str("").expect("an empty ingest block must load");
    assert_eq!(spellings(&config.severity_from), DEFAULT_SEVERITY_FROM);
    assert_eq!(spellings(&config.time_from), DEFAULT_TIME_FROM);
    assert!(
        config
            .severity_from
            .iter()
            .chain(&config.time_from)
            .all(|s| s.dialect().is_none()),
        "the packaged defaults spell no dialect — 'unset' is not 'explicitly otel'"
    );
}

#[test]
fn standalone_starter_uses_defaults_without_development_credentials() {
    let config: Config = toml::from_str(&read("config/trawld.toml"))
        .expect("the standalone starter must load as a shared server/proxy config");
    assert!(config.auth.database_url.is_none());
    assert!(config.storage.database_url.is_none());
    assert_eq!(config.server.http_addr, "127.0.0.1:5514");
    assert_eq!(config.web.bind_addr.as_deref(), Some("127.0.0.1:8090"));
    assert!(!config.web.public_origins.is_empty());
    assert!(config.web.cookie_secret_path.is_some());
    assert_eq!(
        spellings(&config.ingest.severity_from),
        DEFAULT_SEVERITY_FROM
    );
    assert_eq!(spellings(&config.ingest.time_from), DEFAULT_TIME_FROM);
}

#[test]
fn every_packaging_artifact_states_the_same_derivation_defaults() {
    let expected_severity: Vec<String> = DEFAULT_SEVERITY_FROM
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let expected_time: Vec<String> = DEFAULT_TIME_FROM.iter().map(|s| (*s).to_owned()).collect();

    let example = read("config/trawld.reference.toml");
    let debian = read("crates/trawl-server/debian/trawld.toml");
    let docs = read("docs/src/content/docs/reference/configuration.md");

    let artifacts: Vec<(&str, Vec<String>, Vec<String>)> = vec![
        (
            "config/trawld.reference.toml",
            toml_default(&example, "severity_from", "config/trawld.reference.toml"),
            toml_default(&example, "time_from", "config/trawld.reference.toml"),
        ),
        (
            "crates/trawl-server/debian/trawld.toml",
            toml_default(&debian, "severity_from", "the Debian example"),
            toml_default(&debian, "time_from", "the Debian example"),
        ),
        (
            "chart/trawl",
            chart_defaults("severity_from", "severityFrom"),
            chart_defaults("time_from", "timeFrom"),
        ),
        (
            "docs/reference/configuration.md",
            toml_default(&docs, "severity_from", "the configuration reference"),
            toml_default(&docs, "time_from", "the configuration reference"),
        ),
    ];

    for (origin, severity, time) in artifacts {
        assert_eq!(
            severity, expected_severity,
            "{origin} disagrees with trawl_config::DEFAULT_SEVERITY_FROM — \
             the derivation defaults are ONE cross-packaging contract; \
             update every artifact or none"
        );
        assert_eq!(
            time, expected_time,
            "{origin} disagrees with trawl_config::DEFAULT_TIME_FROM"
        );
    }
}

/// The typed-form examples are the syslog-over-HTTP forwarder's whole
/// documentation, so a stale or mistyped one is worse than none: it
/// would be copied into a `trawld.toml` and refuse to boot.
#[test]
fn every_documented_typed_example_actually_loads() {
    let sources = [
        (
            "config/trawld.reference.toml",
            read("config/trawld.reference.toml"),
        ),
        (
            "crates/trawl-server/debian/trawld.toml",
            read("crates/trawl-server/debian/trawld.toml"),
        ),
        (
            "docs/reference/configuration.md",
            read("docs/src/content/docs/reference/configuration.md"),
        ),
    ];

    let mut typed_examples = 0;
    for (origin, source) in &sources {
        for key in ["severity_from", "time_from"] {
            for declaration in toml_declarations(source, key) {
                let parsed: BTreeMap<String, Vec<DerivationSourceSpec>> =
                    toml::from_str(declaration).unwrap_or_else(|e| {
                        panic!("{origin} states an unloadable {key}: {declaration}\n{e}")
                    });
                let entries = &parsed[key];
                assert!(
                    !entries.is_empty(),
                    "{origin}: an empty {key} example teaches nothing"
                );
                typed_examples += entries
                    .iter()
                    .filter(|e| matches!(e, DerivationSourceSpec::Typed { .. }))
                    .count();
            }
        }
        assert!(
            source.contains(r#"{ field = "syslog_severity", dialect = "syslog" }"#),
            "{origin} must show the syslog-over-HTTP forwarder example — \
             it is the only reason the typed form exists"
        );
    }
    assert!(
        typed_examples >= sources.len(),
        "each artifact must carry at least one typed example; saw {typed_examples}"
    );
}
