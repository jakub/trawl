// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The trawl-web log-filter contract.
//!
//! trawl-web is a separate process under its own tracing target, so it must
//! never inherit trawld's target-only filter: that filter names no
//! `trawl_web` target, and a non-empty target-only filter drops everything
//! it does not name. These tests take the filter the Helm chart actually
//! installs into the sidecar container and prove representative trawl-web
//! diagnostics pass it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

fn chart_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../chart/trawl")
        .canonicalize()
        .expect("chart/trawl must exist relative to the crate")
}

/// The `RUST_LOG` the chart installs into the `trawl-web` sidecar.
///
/// Rendered with `helm template` when helm is on PATH — the real thing,
/// values plumbing included. Only a missing helm binary falls back to
/// reading `values.yaml`, so the contract is still enforced everywhere (a
/// test that silently skips proves nothing) and a chart that fails to
/// render is a failure, not a fallback.
fn rendered_sidecar_rust_log() -> String {
    let chart = chart_dir();
    let rendered = Command::new("helm")
        .args(["template", "trawl"])
        .arg(&chart)
        .args(["--show-only", "templates/statefulset.yaml"])
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
            container_env_value(&manifest, "trawl-web", "RUST_LOG")
                .expect("rendered trawl-web container must set RUST_LOG")
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let values = std::fs::read_to_string(chart.join("values.yaml"))
                .expect("chart values.yaml must be readable");
            nested_scalar(&values, "web", "logLevel").expect("values.yaml must define web.logLevel")
        }
        Err(e) => panic!("could not run helm: {e}"),
    }
}

/// Pull `value:` for `env_name` out of the named container's block.
///
/// Container blocks start at `- name: <container>` and run until the next
/// list item at the same indentation, which is enough structure to find one
/// env var without pulling in a YAML parser.
fn container_env_value(manifest: &str, container: &str, env_name: &str) -> Option<String> {
    let start_marker = format!("- name: {container}");
    let mut lines = manifest.lines();
    let start = lines.by_ref().find(|l| l.trim() == start_marker)?;
    let indent = start.len() - start.trim_start().len();

    let mut in_env = false;
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let line_indent = line.len() - line.trim_start().len();
        // Next container (same indentation, new list item) ends this block.
        if line_indent <= indent && trimmed.starts_with("- ") {
            return None;
        }
        if trimmed == format!("- name: {env_name}") {
            in_env = true;
        } else if in_env {
            return trimmed
                .strip_prefix("value:")
                .map(|v| v.trim().trim_matches('"').to_owned());
        }
    }
    None
}

/// Read `parent.key` out of a values file: the first `key:` line indented
/// under a top-level `parent:` mapping.
fn nested_scalar(values: &str, parent: &str, key: &str) -> Option<String> {
    let parent_marker = format!("{parent}:");
    let mut lines = values.lines();
    lines.by_ref().find(|l| *l == parent_marker)?;
    let key_marker = format!("{key}:");
    for line in lines {
        if !line.starts_with([' ', '#']) && !line.trim().is_empty() {
            return None; // left the block
        }
        let trimmed = line.trim_start();
        if let Some(value) = trimmed.strip_prefix(&key_marker) {
            return Some(value.trim().trim_matches('"').to_owned());
        }
    }
    None
}

/// Capture layer recording event targets.
#[derive(Clone, Default)]
struct CaptureLayer {
    targets: Arc<Mutex<Vec<String>>>,
}

impl<S> Layer<S> for CaptureLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        self.targets
            .lock()
            .expect("capture mutex")
            .push(event.metadata().target().to_owned());
    }
}

fn targets_passing(directives: &str) -> Vec<String> {
    use tracing_subscriber::prelude::*;

    let capture = CaptureLayer::default();
    let targets = Arc::clone(&capture.targets);
    let filter = tracing_subscriber::EnvFilter::new(directives);
    let subscriber = tracing_subscriber::registry().with(capture.with_filter(filter));
    let _guard = tracing::subscriber::set_default(subscriber);

    // Representative trawl-web diagnostics: startup, session, upstream —
    // plus the origin rejection fleet_auth::session emits on the proxy's
    // behalf, and dependency noise that must stay out.
    tracing::info!(target: "trawl_web", "trawl-web listening");
    tracing::warn!(target: "trawl_web::middleware::session", "session rejected");
    tracing::warn!(target: "trawl_web::routes::proxy", "upstream request failed");
    tracing::warn!(target: "fleet_auth::session", "origin rejected");
    tracing::info!(target: "hyper::proto", "dependency noise");
    tracing::info!(target: "reqwest::connect", "dependency noise");

    let seen = targets.lock().expect("capture mutex");
    seen.clone()
}

#[test]
fn chart_sidecar_filter_matches_the_binary_default() {
    assert_eq!(
        rendered_sidecar_rust_log(),
        trawl_web::DEFAULT_LOG_FILTER,
        "the chart's web.logLevel and trawl_web::DEFAULT_LOG_FILTER are one \
         cross-packaging contract; update both or neither"
    );
}

#[test]
fn representative_trawl_web_events_pass_the_installed_chart_filter() {
    let installed = rendered_sidecar_rust_log();
    let targets = targets_passing(&installed);

    for expected in [
        "trawl_web",
        "trawl_web::middleware::session",
        "trawl_web::routes::proxy",
        "fleet_auth::session",
    ] {
        assert!(
            targets.iter().any(|t| t == expected),
            "target {expected} must pass the sidecar filter {installed:?}; saw {targets:?}"
        );
    }
    for noise in ["hyper::proto", "reqwest::connect"] {
        assert!(
            !targets.iter().any(|t| t == noise),
            "dependency INFO must NOT pass the sidecar filter; saw {targets:?}"
        );
    }
}

#[test]
fn trawld_filter_would_silence_the_sidecar() {
    // Why the sidecar needs its own value: trawld's filter is target-only
    // and names no trawl_web target, so inheriting it drops every proxy
    // diagnostic.
    let targets =
        targets_passing("trawl_server=info,trawld=info,auth.backend=info,storage.backend=info");
    assert!(
        !targets.iter().any(|t| t.starts_with("trawl_web")),
        "sanity: trawld's filter names no trawl_web target; saw {targets:?}"
    );
}
