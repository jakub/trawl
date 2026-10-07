// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The configuration reference documents `trawl-web --doctor` from the names
//! the code uses: every check id, the per-key health rows, the four outcomes,
//! the reason codes only this doctor reports, and each verdict with its exit
//! status. The names are imported, not copied, so a renamed check or a new
//! outcome fails here until the reference names it.
//!
//! The deployment guide gives the command for each install channel. Each
//! command must match what that channel packages: the Debian unit's user,
//! group, environment file and `--config`, the Helm sidecar's container name
//! and `--config`, and the trial's web configuration path.

use std::sync::LazyLock;
use trawl_api::doctor::{Outcome, Verdict, reason};
use trawl_web::doctor::WebCheck;
use trawl_web::doctor::output::HealthKey;

/// docs/scripts/release-pins.mjs owns the release placeholder grammar.
fn development_page(raw: &str) -> String {
    let page = raw
        .replace(" --version {{release.version}}", "")
        .replace("{{release.tag}}", "main");
    assert!(
        !page.contains("{{release."),
        "a release placeholder the docs plugin rejects"
    );
    page
}

static CONFIGURATION: LazyLock<String> = LazyLock::new(|| {
    development_page(include_str!(
        "../../../docs/src/content/docs/reference/configuration.md"
    ))
});
static DEPLOYMENT: LazyLock<String> = LazyLock::new(|| {
    development_page(include_str!(
        "../../../docs/src/content/docs/operate/deployment.md"
    ))
});

const DEBIAN_UNIT: &str = include_str!("../../trawl-server/debian/trawl-web.service");
const HELM_STATEFULSET: &str = include_str!("../../../chart/trawl/templates/statefulset.yaml");
const TRIAL_COMPOSE: &str = include_str!("../../trawl-cli/src/trial/compose.rs");

const DEBIAN_COMMAND: &str = "sudo systemd-run --pipe --wait --collect -p User=trawl-web \
     -p Group=trawl -p EnvironmentFile=-/etc/default/trawl-web \
     trawl-web --doctor --config /etc/trawl/trawld.toml";
const HELM_COMMAND: &str = "kubectl -n trawl exec trawl-0 -c trawl-web -- \
     trawl-web --doctor --config /etc/trawl/trawld.toml";
const TRIAL_COMMAND: &str =
    "docker compose exec trawl-web trawl-web --doctor --config /var/lib/trawl/trial/web.toml";

/// The part of `page` from the heading line `heading` to the next heading
/// of the same or a higher level. The page's one `#` title is not a
/// boundary, so a `#` comment in a code block does not end the section.
fn section(page: &'static str, heading: &str) -> &'static str {
    let level = heading.chars().take_while(|c| *c == '#').count();
    let start = page
        .find(&format!("\n{heading}\n"))
        .unwrap_or_else(|| panic!("the page has no heading {heading:?}"));
    let rest = &page[start + 1..];
    let body_at = heading.len() + 1;
    let end = (2..=level)
        .filter_map(|higher| rest[body_at..].find(&format!("\n{} ", "#".repeat(higher))))
        .min()
        .map_or(rest.len(), |at| at + body_at);
    &rest[..end]
}

/// The section headed "Check the web proxy with `trawl-web --doctor`".
fn doctor_section() -> &'static str {
    section(
        &CONFIGURATION,
        "### Check the web proxy with `trawl-web --doctor`",
    )
}

fn assert_documented(text: &str, what: &str) {
    assert!(
        doctor_section().contains(text),
        "the `trawl-web --doctor` reference does not name {what} {text:?}"
    );
}

/// The name serde gives a value, without its JSON quotes.
fn serde_name(value: impl serde::Serialize) -> String {
    match serde_json::to_value(value).expect("serializes") {
        serde_json::Value::String(name) => name,
        other => panic!("not a unit variant: {other}"),
    }
}

#[test]
fn every_check_id_is_documented() {
    for check in WebCheck::ALL {
        assert_documented(&format!("| `{}` |", check.id()), "the check");
    }
    let health = WebCheck::UpstreamHealth.id();
    assert_documented(&format!("`{health}.<key>`"), "the per-key health rows");
    assert_documented(
        &format!("`{health}.{}`", HealthKey::INVALID),
        "the invalid-name health row",
    );
}

#[test]
fn every_outcome_and_reason_code_is_documented() {
    let all = [
        Outcome::Complete,
        Outcome::Failed,
        Outcome::NotConfigured,
        Outcome::NotSampled,
    ];
    for outcome in all {
        // An exhaustive match: a new variant fails to compile until listed.
        match outcome {
            Outcome::Complete | Outcome::Failed | Outcome::NotConfigured | Outcome::NotSampled => {}
        }
        assert_documented(&format!("| `{}` |", serde_name(outcome)), "the outcome");
    }
    // The codes only this doctor reports each have a row of their own.
    for code in [
        reason::CA_NOT_PRESENT,
        reason::EPHEMERAL_EACH_START,
        reason::CONNECTION_REFUSED,
        reason::CERTIFICATE_NOT_TRUSTED,
        reason::REDIRECT_REFUSED,
    ] {
        assert_documented(&format!("| `{code}` |"), "the reason code");
    }
    for code in [
        reason::BLOCKED,
        reason::PERMISSION_DENIED,
        reason::RAN_AS_ROOT,
        reason::TIMED_OUT,
    ] {
        assert_documented(&format!("`{code}`"), "the reason code");
    }
}

#[test]
fn exit_codes_zero_to_three_are_documented() {
    let all = [Verdict::Pass, Verdict::Fail, Verdict::Incomplete];
    for verdict in all {
        // An exhaustive match: a new verdict fails to compile until listed.
        match verdict {
            Verdict::Pass | Verdict::Fail | Verdict::Incomplete => {}
        }
        let row = format!("| `{}` |", verdict.exit_code());
        let line = doctor_section()
            .lines()
            .find(|line| line.starts_with(&row))
            .unwrap_or_else(|| panic!("no exit code row starting {row:?}"));
        assert!(
            line.contains(&format!("`{}`", serde_name(verdict))),
            "the {row:?} row does not name its verdict: {line:?}"
        );
    }
    assert_documented("| `2` | Usage error", "the usage status");
}

#[test]
fn the_deployment_guide_gives_each_channel_its_command() {
    let verify = section(&DEPLOYMENT, "## Verify the installation");
    for command in [DEBIAN_COMMAND, HELM_COMMAND, TRIAL_COMMAND] {
        assert!(
            verify.lines().any(|line| line.trim() == command),
            "\"Verify the installation\" does not give the command {command:?}"
        );
    }
    for doctor in [
        "`trawld --doctor`",
        "`trawl-web --doctor`",
        "`trawl doctor --web-url`",
    ] {
        assert!(
            verify.contains(doctor),
            "\"Verify the installation\" does not say what {doctor} covers"
        );
    }
}

#[test]
fn each_channel_command_matches_its_packaging() {
    for line in [
        "ExecStart=/usr/bin/trawl-web --config /etc/trawl/trawld.toml",
        "User=trawl-web",
        "Group=trawl",
        "EnvironmentFile=-/etc/default/trawl-web",
    ] {
        assert!(
            DEBIAN_UNIT.lines().any(|unit| unit.trim() == line),
            "trawl-web.service no longer has {line:?}; update the Debian command"
        );
    }

    let sidecar = HELM_STATEFULSET
        .find("- name: trawl-web\n")
        .map(|at| &HELM_STATEFULSET[at..])
        .expect("the chart has a container named trawl-web");
    let args = sidecar
        .lines()
        .find(|line| line.trim_start().starts_with("args:"))
        .expect("the trawl-web container has args");
    assert_eq!(
        args.trim(),
        r#"args: ["--config", "/etc/trawl/trawld.toml"]"#,
        "the Helm command's --config must be the sidecar's"
    );

    assert!(
        TRIAL_COMPOSE.contains(r#"pub const WEB_TOML: &str = "/var/lib/trawl/trial/web.toml";"#),
        "the trial's web configuration moved; update the trial command"
    );
}
