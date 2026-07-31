// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::cli::App;
use crate::command::{CommandRunner, CommandSpec, require_success};
use crate::config::{AppManifest, TailscaleProfile};
use crate::environment;
use crate::error::{Error, Result};
use crate::topology::TailscaleNode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeMapping {
    pub origin: String,
    pub target: String,
}

pub fn discover(runner: &dyn CommandRunner, overrides: &TailscaleProfile) -> Result<TailscaleNode> {
    let hostname = if let Some(hostname) = overrides.hostname.as_deref() {
        normalize_hostname(hostname)?
    } else {
        let spec = tailscale_spec().args(["status", "--json"]);
        let output = runner.output(&spec)?;
        require_success(
            &spec,
            &output,
            "tailscale status failed; verify tailscaled is running and logged in",
        )?;
        let document: Value = serde_json::from_slice(&output.stdout).map_err(|source| {
            Error::InvalidArgument(format!("tailscale status returned invalid JSON: {source}"))
        })?;
        let hostname = document
            .pointer("/Self/DNSName")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::InvalidArgument("tailscale status did not include Self.DNSName".to_owned())
            })?;
        normalize_hostname(hostname)?
    };
    let ipv4 = if let Some(ipv4) = overrides.ipv4.as_deref() {
        validate_ipv4(ipv4)?
    } else {
        let spec = tailscale_spec().args(["ip", "-4"]);
        let output = runner.output(&spec)?;
        require_success(
            &spec,
            &output,
            "tailscale ip -4 failed; verify this node has a tailnet IPv4 address",
        )?;
        let text = String::from_utf8(output.stdout).map_err(|_| {
            Error::InvalidArgument("tailscale ip -4 returned non-UTF-8 output".to_owned())
        })?;
        let addresses: Vec<_> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        if addresses.len() != 1 {
            return Err(Error::InvalidArgument(format!(
                "tailscale ip -4 returned {} addresses; expected exactly one",
                addresses.len()
            )));
        }
        validate_ipv4(addresses[0].trim())?
    };
    Ok(TailscaleNode { hostname, ipv4 })
}

pub fn inspect_serve(runner: &dyn CommandRunner) -> Result<Vec<ServeMapping>> {
    let spec = tailscale_spec().args(["serve", "status", "--json"]);
    let output = runner.output(&spec)?;
    require_success(
        &spec,
        &output,
        "tailscale serve status failed; verify tailscaled is running and logged in",
    )?;
    parse_serve_status(&output.stdout)
}

pub fn verify(
    runner: &dyn CommandRunner,
    node: &TailscaleNode,
    manifests: &BTreeMap<App, AppManifest>,
) -> Result<()> {
    let mappings = inspect_serve(runner)?;
    for manifest in manifests.values() {
        let expected_origin = format!("{}:{}", node.hostname, manifest.web.tailscale_port);
        let expected_target = format!("http://127.0.0.1:{}", manifest.web.local_port);
        let occupants = mappings_on_port(&mappings, manifest.web.tailscale_port);
        match occupants.as_slice() {
            [mapping] if mapping.origin == expected_origin && mapping.target == expected_target => {
            }
            [mapping] => {
                return Err(Error::InvalidArgument(format!(
                    "Tailscale Serve public port {} is configured as {} -> {}, expected {expected_origin} -> {expected_target}; run `fleet-dev setup {} --exposure tailscale --force`",
                    manifest.web.tailscale_port, mapping.origin, mapping.target, manifest.name
                )));
            }
            [] => {
                return Err(Error::InvalidArgument(format!(
                    "Tailscale Serve mapping {expected_origin} -> {expected_target} is missing; run `fleet-dev setup {} --exposure tailscale`",
                    manifest.name
                )));
            }
            _ => {
                return Err(Error::InvalidArgument(format!(
                    "Tailscale Serve public port {} has multiple conflicting handlers; run `fleet-dev setup {} --exposure tailscale --force`",
                    manifest.web.tailscale_port, manifest.name
                )));
            }
        }
    }
    Ok(())
}

pub fn setup(
    runner: &dyn CommandRunner,
    node: &TailscaleNode,
    manifests: &BTreeMap<App, AppManifest>,
    force: bool,
) -> Result<()> {
    let mappings = inspect_serve(runner)?;
    let mut actions = Vec::new();

    // Preflight the whole selection before making the first persistent
    // mutation. A later conflict must not leave an earlier app changed.
    for manifest in manifests.values() {
        let expected_origin = format!("{}:{}", node.hostname, manifest.web.tailscale_port);
        let expected_target = format!("http://127.0.0.1:{}", manifest.web.local_port);
        let occupants = mappings_on_port(&mappings, manifest.web.tailscale_port);
        match occupants.as_slice() {
            [mapping] if mapping.origin == expected_origin && mapping.target == expected_target => {
            }
            [_] | [_, _, ..] if !force => {
                let current = summarize_mappings(&occupants);
                return Err(Error::InvalidArgument(format!(
                    "refusing to replace Tailscale Serve public port {}: current {current}, proposed {expected_origin} -> {expected_target}; rerun with --force",
                    manifest.web.tailscale_port
                )));
            }
            [_] | [_, _, ..] => actions.push((
                manifest,
                expected_origin,
                expected_target,
                Some(ServeMapping {
                    origin: format!("port {}", manifest.web.tailscale_port),
                    target: summarize_mappings(&occupants),
                }),
            )),
            [] => actions.push((manifest, expected_origin, expected_target, None)),
        }
    }

    for (manifest, expected_origin, expected_target, previous) in actions {
        if let Some(mapping) = previous {
            eprintln!(
                "fleet-dev: replacing Tailscale Serve {} -> {} with {expected_origin} -> {expected_target}",
                mapping.origin, mapping.target
            );
        } else {
            eprintln!("fleet-dev: creating Tailscale Serve {expected_origin} -> {expected_target}");
        }
        let spec = tailscale_spec().args([
            "serve".to_owned(),
            "--bg".to_owned(),
            "--https".to_owned(),
            manifest.web.tailscale_port.to_string(),
            manifest.web.local_port.to_string(),
        ]);
        let output = runner.output(&spec)?;
        require_success(
            &spec,
            &output,
            format!("failed to configure Tailscale Serve for {}", manifest.name),
        )?;
    }
    Ok(())
}

fn mapping_port(mapping: &ServeMapping) -> Option<u16> {
    mapping
        .origin
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
}

fn mappings_on_port(mappings: &[ServeMapping], port: u16) -> Vec<&ServeMapping> {
    mappings
        .iter()
        .filter(|mapping| mapping_port(mapping) == Some(port))
        .collect()
}

fn summarize_mappings(mappings: &[&ServeMapping]) -> String {
    mappings
        .iter()
        .map(|mapping| format!("{} -> {}", mapping.origin, mapping.target))
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_serve_status(bytes: &[u8]) -> Result<Vec<ServeMapping>> {
    let document: Value = serde_json::from_slice(bytes).map_err(|source| {
        Error::InvalidArgument(format!(
            "tailscale serve status returned invalid JSON: {source}"
        ))
    })?;
    let mut mappings = Vec::new();
    if let Some(web) = document.get("Web").and_then(Value::as_object) {
        for (origin, value) in web {
            let target = value
                .get("Handlers")
                .and_then(Value::as_object)
                .and_then(|handlers| {
                    handlers
                        .get("/")
                        .and_then(|root| root.get("Proxy"))
                        .and_then(Value::as_str)
                })
                .map_or_else(
                    || "<non-proxy Serve handler>".to_owned(),
                    |target| target.trim_end_matches('/').to_owned(),
                );
            mappings.push(ServeMapping {
                origin: origin.trim_end_matches('.').to_owned(),
                target,
            });
        }
    }
    let web_ports: std::collections::BTreeSet<_> =
        mappings.iter().filter_map(mapping_port).collect();
    if let Some(tcp) = document.get("TCP").and_then(Value::as_object) {
        for (port_key, value) in tcp {
            // Keys are bare (`"8445"`) today, but the `:port` and `host:port`
            // forms must not silently drop an occupant: this conflict check is
            // the only thing standing between `setup` and clobbering an
            // unrelated Serve handler. Fall back to the *trimmed* key.
            let trimmed = port_key.trim_start_matches(':');
            let port = trimmed
                .rsplit_once(':')
                .map_or(trimmed, |(_, port)| port)
                .parse::<u16>()
                .ok();
            if let Some(port) = port.filter(|port| !web_ports.contains(port)) {
                mappings.push(ServeMapping {
                    origin: format!("tcp:{port}"),
                    target: format!("<TCP Serve handler: {value}>"),
                });
            }
        }
    }
    mappings.sort_by(|left, right| left.origin.cmp(&right.origin));
    Ok(mappings)
}

fn normalize_hostname(hostname: &str) -> Result<String> {
    let hostname = hostname.trim().trim_end_matches('.');
    if hostname.is_empty()
        || hostname.contains(['/', ':'])
        || !hostname.is_ascii()
        || !hostname.contains('.')
    {
        return Err(Error::InvalidArgument(format!(
            "invalid Tailscale MagicDNS hostname {hostname:?}"
        )));
    }
    Ok(hostname.to_ascii_lowercase())
}

fn validate_ipv4(ipv4: &str) -> Result<String> {
    ipv4.trim()
        .parse::<std::net::Ipv4Addr>()
        .map(|address| address.to_string())
        .map_err(|_| Error::InvalidArgument(format!("invalid Tailscale IPv4 address {ipv4:?}")))
}

fn tailscale_spec() -> CommandSpec {
    CommandSpec::new("tailscale")
        .environment(environment::sanitized_base())
        .timeout(std::time::Duration::from_secs(30))
        .report_stderr()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::Mutex;

    use super::*;
    use crate::config::{
        AuthManifest, ConsumerEnvironment, DatabaseManifest, MigrationMode, ProcessManifest,
        WebManifest,
    };

    #[derive(Debug)]
    struct FakeRunner {
        outputs: Mutex<VecDeque<std::process::Output>>,
        seen: Mutex<Vec<CommandSpec>>,
    }

    impl FakeRunner {
        fn new(outputs: impl IntoIterator<Item = &'static [u8]>) -> Self {
            Self {
                outputs: Mutex::new(
                    outputs
                        .into_iter()
                        .map(|stdout| std::process::Output {
                            status: std::process::ExitStatus::from_raw(0),
                            stdout: stdout.to_vec(),
                            stderr: Vec::new(),
                        })
                        .collect(),
                ),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn output(&self, spec: &CommandSpec) -> Result<std::process::Output> {
            self.seen.lock().unwrap().push(spec.clone());
            Ok(self.outputs.lock().unwrap().pop_front().unwrap())
        }
    }

    fn manifest() -> AppManifest {
        AppManifest {
            schema: 1,
            name: App::Trawl,
            web: WebManifest {
                local_port: 8081,
                tailscale_port: 8444,
                backend_port: 8090,
                login_path: "/login".to_owned(),
            },
            database: DatabaseManifest {
                name: "trawl_dev".to_owned(),
                migration_mode: MigrationMode::ApplicationStartup,
                migration_command: Vec::new(),
            },
            resolver: None,
            preparation: None,
            auth: AuthManifest {
                permissions: Vec::new(),
            },
            processes: vec![ProcessManifest {
                name: "web".to_owned(),
                command: vec!["true".to_owned()],
                cwd: None,
                env: BTreeMap::new(),
                resolved_env: BTreeMap::new(),
            }],
            migration: ConsumerEnvironment::default(),
        }
    }

    fn node() -> TailscaleNode {
        TailscaleNode {
            hostname: "fractal.example.ts.net".to_owned(),
            ipv4: "100.64.0.10".to_owned(),
        }
    }

    #[test]
    fn parses_matching_and_unrelated_mappings() {
        let mappings = parse_serve_status(
            br#"{
              "Web": {
                "fractal.example.ts.net:8444": {
                  "Handlers": {"/": {"Proxy": "http://127.0.0.1:8081"}}
                },
                "fractal.example.ts.net:9999": {
                  "Handlers": {"/": {"Text": "hello"}}
                }
              }
            }"#,
        )
        .unwrap();
        assert_eq!(mappings.len(), 2);
        assert_eq!(
            mappings[0],
            ServeMapping {
                origin: "fractal.example.ts.net:8444".to_owned(),
                target: "http://127.0.0.1:8081".to_owned(),
            }
        );
        assert_eq!(mappings[1].origin, "fractal.example.ts.net:9999");
        assert_eq!(mappings[1].target, "<non-proxy Serve handler>");
    }

    #[test]
    fn malformed_status_fails_closed() {
        assert!(parse_serve_status(b"not json").is_err());
    }

    #[test]
    fn normalizes_hostname_and_ip() {
        assert_eq!(
            normalize_hostname("Fractal.Example.ts.net.").unwrap(),
            "fractal.example.ts.net"
        );
        assert_eq!(validate_ipv4("100.64.0.1\n").unwrap(), "100.64.0.1");
    }

    #[test]
    fn verify_reports_the_exact_missing_repair_without_mutating() {
        let runner = FakeRunner::new([b"{}".as_slice()]);
        let error = verify(
            &runner,
            &node(),
            &BTreeMap::from([(App::Trawl, manifest())]),
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("fractal.example.ts.net:8444"));
        assert!(message.contains("http://127.0.0.1:8081"));
        assert!(message.contains("fleet-dev setup trawl"));
        assert_eq!(runner.seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn setup_is_idempotent_and_conflicts_require_force() {
        const MATCHING: &[u8] = br#"{"Web":{"fractal.example.ts.net:8444":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8081"}}}}}"#;
        const CONFLICT: &[u8] = br#"{"Web":{"fractal.example.ts.net:8444":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9999"}}}}}"#;
        let manifests = BTreeMap::from([(App::Trawl, manifest())]);
        let matching = FakeRunner::new([MATCHING]);
        setup(&matching, &node(), &manifests, false).unwrap();
        assert_eq!(matching.seen.lock().unwrap().len(), 1);

        let refused = FakeRunner::new([CONFLICT]);
        assert!(setup(&refused, &node(), &manifests, false).is_err());
        assert_eq!(refused.seen.lock().unwrap().len(), 1);

        let forced = FakeRunner::new([CONFLICT, b""]);
        setup(&forced, &node(), &manifests, true).unwrap();
        let seen = forced.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[1]
                .args
                .iter()
                .map(|value| value.to_string_lossy())
                .collect::<Vec<_>>(),
            ["serve", "--bg", "--https", "8444", "8081"]
        );
        assert!(
            !seen[1]
                .environment
                .contains_key(std::ffi::OsStr::new("OP_SERVICE_ACCOUNT_TOKEN"))
        );
    }

    #[test]
    fn same_public_port_on_an_old_hostname_is_a_conflict() {
        const OLD_HOST: &[u8] = br#"{"Web":{"old.example.ts.net:8444":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8081"}}}}}"#;
        let runner = FakeRunner::new([OLD_HOST]);
        let error = setup(
            &runner,
            &node(),
            &BTreeMap::from([(App::Trawl, manifest())]),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("old.example.ts.net:8444"));
        assert_eq!(runner.seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn non_proxy_web_and_tcp_handlers_occupy_public_ports() {
        let mappings = parse_serve_status(
            br#"{
                "Web": {
                    "fractal.example.ts.net:8444": {
                        "Handlers": {"/": {"Text": "occupied"}}
                    }
                },
                "TCP": {
                    "8445": {"TCPForward": "127.0.0.1:9999"}
                }
            }"#,
        )
        .unwrap();
        assert_eq!(mappings.len(), 2);
        assert_eq!(mapping_port(&mappings[0]), Some(8444));
        assert_eq!(mapping_port(&mappings[1]), Some(8445));

        let runner = FakeRunner::new([
            br#"{"Web":{"fractal.example.ts.net:8444":{"Handlers":{"/":{"Text":"occupied"}}}}}"#
                .as_slice(),
        ]);
        let error = setup(
            &runner,
            &node(),
            &BTreeMap::from([(App::Trawl, manifest())]),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("non-proxy Serve handler"));
        assert_eq!(runner.seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn tcp_keys_are_parsed_in_every_serve_form() {
        let mappings = parse_serve_status(
            br#"{"TCP":{
                "8445":{"TCPForward":"127.0.0.1:1"},
                ":8446":{"TCPForward":"127.0.0.1:2"},
                "fractal.example.ts.net:8447":{"TCPForward":"127.0.0.1:3"}
            }}"#,
        )
        .unwrap();
        let ports: Vec<_> = mappings.iter().filter_map(mapping_port).collect();
        assert_eq!(ports, [8445, 8446, 8447]);
    }

    #[test]
    fn a_colon_prefixed_tcp_occupant_still_blocks_setup() {
        let runner =
            FakeRunner::new([br#"{"TCP":{":8444":{"TCPForward":"127.0.0.1:9"}}}"#.as_slice()]);
        let error = setup(
            &runner,
            &node(),
            &BTreeMap::from([(App::Trawl, manifest())]),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("refusing to replace"));
        assert_eq!(runner.seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn multiple_web_handlers_on_one_port_fail_closed() {
        const MULTIPLE: &[u8] = br#"{"Web":{
            "fractal.example.ts.net:8444":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8081"}}},
            "old.example.ts.net:8444":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9999"}}}
        }}"#;
        let runner = FakeRunner::new([MULTIPLE]);
        let error = verify(
            &runner,
            &node(),
            &BTreeMap::from([(App::Trawl, manifest())]),
        )
        .unwrap_err();
        assert!(error.to_string().contains("multiple conflicting handlers"));
    }

    #[test]
    fn multi_app_setup_preflights_every_conflict_before_mutating() {
        const SECOND_CONFLICT: &[u8] = br#"{"Web":{"old.example.ts.net:8445":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9999"}}}}}"#;
        let mut coastwatch = manifest();
        coastwatch.name = App::Coastwatch;
        coastwatch.web.local_port = 8082;
        coastwatch.web.tailscale_port = 8445;
        coastwatch.web.backend_port = 3002;
        let manifests = BTreeMap::from([(App::Trawl, manifest()), (App::Coastwatch, coastwatch)]);
        let runner = FakeRunner::new([SECOND_CONFLICT]);
        assert!(setup(&runner, &node(), &manifests, false).is_err());
        assert_eq!(
            runner.seen.lock().unwrap().len(),
            1,
            "the missing first mapping must not be created before the second conflict is found"
        );
    }
}
