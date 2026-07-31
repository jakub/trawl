// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use serde::{Deserialize, Serialize};

use crate::cli::{App, Exposure};
use crate::config::{MachineProfile, WebManifest};
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AppTopology {
    pub app: App,
    pub browser_origin: String,
    pub login_url: String,
    pub backend_authority: String,
    pub api_bind: String,
    pub spa_bind: Vec<String>,
    pub spa_port: u16,
    pub cookie_domain: Option<String>,
    pub cookie_path: String,
    pub cookie_secure: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailscaleNode {
    pub hostname: String,
    pub ipv4: String,
}

pub fn resolve(
    app: App,
    web: &WebManifest,
    profile: &MachineProfile,
    tailscale: Option<&TailscaleNode>,
) -> Result<AppTopology> {
    match profile.exposure {
        Exposure::Localhost => {
            let host = profile.host.trim_end_matches('.');
            if host.is_empty() {
                return Err(Error::InvalidArgument(
                    "localhost exposure requires a non-empty host".to_owned(),
                ));
            }
            let loopback_address = host
                .parse::<std::net::Ipv4Addr>()
                .ok()
                .filter(std::net::Ipv4Addr::is_loopback);
            let loopback = loopback_address.is_some();
            if host != "localhost" && !host.ends_with(".localhost") && !loopback {
                return Err(Error::InvalidArgument(format!(
                    "localhost exposure host {host:?} must be localhost, a .localhost name, or a loopback IPv4 address"
                )));
            }
            let api_host =
                loopback_address.map_or("127.0.0.1".to_owned(), |value| value.to_string());
            let mut spa_bind = vec![api_host.clone()];
            if loopback_address.is_none() {
                spa_bind.push("::1".to_owned());
            }
            Ok(AppTopology {
                app,
                browser_origin: format!("http://{host}:{}", web.local_port),
                login_url: format!("http://{host}:{}{}", web.local_port, web.login_path),
                backend_authority: format!("{host}:{}", web.backend_port),
                api_bind: format!("{api_host}:{}", web.backend_port),
                spa_bind,
                spa_port: web.local_port,
                cookie_domain: None,
                cookie_path: "/".to_owned(),
                cookie_secure: false,
            })
        }
        Exposure::Tailscale => {
            let node = tailscale.ok_or_else(|| {
                Error::InvalidArgument(
                    "Tailscale exposure requires successful daemon discovery".to_owned(),
                )
            })?;
            let host = node.hostname.trim_end_matches('.');
            if host.is_empty() || node.ipv4.parse::<std::net::Ipv4Addr>().is_err() {
                return Err(Error::InvalidArgument(
                    "Tailscale discovery returned an invalid hostname or IPv4 address".to_owned(),
                ));
            }
            Ok(AppTopology {
                app,
                browser_origin: format!("https://{host}:{}", web.tailscale_port),
                login_url: format!("https://{host}:{}{}", web.tailscale_port, web.login_path),
                backend_authority: format!("{host}:{}", web.backend_port),
                // Not loopback: Trunk stamps the proxy backend authority into
                // `Host`, and fleet-auth's present-only guard requires it to
                // equal the browser Origin host, so the backend must answer on
                // the MagicDNS name. The cost is that every tailnet peer can
                // reach it directly, bypassing Serve. See ADR-0010.
                api_bind: format!("{}:{}", node.ipv4, web.backend_port),
                // Serve terminates TLS and reaches Trunk over loopback.
                spa_bind: vec!["127.0.0.1".to_owned(), "::1".to_owned()],
                spa_port: web.local_port,
                cookie_domain: None,
                cookie_path: "/".to_owned(),
                cookie_secure: true,
            })
        }
    }
}

pub fn validate_unique(topologies: &[AppTopology]) -> Result<()> {
    let mut sockets = std::collections::BTreeMap::new();
    for topology in topologies {
        let mut app_sockets = Vec::with_capacity(topology.spa_bind.len() + 1);
        app_sockets.push(topology.api_bind.clone());
        app_sockets.extend(topology.spa_bind.iter().map(|address| {
            if address.contains(':') {
                format!("[{address}]:{}", topology.spa_port)
            } else {
                format!("{address}:{}", topology.spa_port)
            }
        }));
        for socket in app_sockets {
            if let Some(previous) = sockets.insert(socket.clone(), topology.app) {
                return Err(Error::InvalidArgument(format!(
                    "{previous} and {} both bind development socket {socket}",
                    topology.app
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::DatabaseMode;
    use crate::config::{OpProfile, PathsProfile, TailscaleProfile};

    fn profile(exposure: Exposure) -> MachineProfile {
        MachineProfile {
            exposure,
            database: DatabaseMode::Docker,
            host: "localhost".to_owned(),
            op: OpProfile::default(),
            paths: PathsProfile::default(),
            cnpg: None,
            tailscale: TailscaleProfile::default(),
        }
    }

    fn web() -> WebManifest {
        WebManifest {
            local_port: 8081,
            tailscale_port: 8444,
            backend_port: 8090,
            login_path: "/login".to_owned(),
        }
    }

    #[test]
    fn localhost_contract() {
        let topology = resolve(App::Trawl, &web(), &profile(Exposure::Localhost), None).unwrap();
        assert_eq!(topology.browser_origin, "http://localhost:8081");
        assert_eq!(topology.backend_authority, "localhost:8090");
        assert_eq!(topology.api_bind, "127.0.0.1:8090");
        assert!(!topology.cookie_secure);
        assert_eq!(topology.cookie_domain, None);
    }

    #[test]
    fn localhost_exposure_rejects_non_loopback_hosts() {
        let mut profile = profile(Exposure::Localhost);
        profile.host = "example.com".to_owned();
        assert!(resolve(App::Trawl, &web(), &profile, None).is_err());
        profile.host = "trawl.localhost".to_owned();
        assert!(resolve(App::Trawl, &web(), &profile, None).is_ok());
        profile.host = "127.0.0.2".to_owned();
        let topology = resolve(App::Trawl, &web(), &profile, None).unwrap();
        assert_eq!(topology.api_bind, "127.0.0.2:8090");
        assert_eq!(topology.spa_bind, ["127.0.0.2"]);
    }

    #[test]
    fn tailscale_contract() {
        let topology = resolve(
            App::Trawl,
            &web(),
            &profile(Exposure::Tailscale),
            Some(&TailscaleNode {
                hostname: "fractal.example.ts.net.".to_owned(),
                ipv4: "100.64.0.10".to_owned(),
            }),
        )
        .unwrap();
        assert_eq!(
            topology.browser_origin,
            "https://fractal.example.ts.net:8444"
        );
        assert_eq!(topology.backend_authority, "fractal.example.ts.net:8090");
        assert!(topology.cookie_secure);
        assert_eq!(topology.cookie_domain, None);
    }

    #[test]
    fn both_apps_keep_origin_and_proxy_on_the_same_tailscale_hostname() {
        let node = TailscaleNode {
            hostname: "fractal.reverse-manta.ts.net".to_owned(),
            ipv4: "100.64.0.10".to_owned(),
        };
        let mut coastwatch = web();
        coastwatch.local_port = 8082;
        coastwatch.tailscale_port = 8445;
        coastwatch.backend_port = 3002;
        for topology in [
            resolve(
                App::Trawl,
                &web(),
                &profile(Exposure::Tailscale),
                Some(&node),
            )
            .unwrap(),
            resolve(
                App::Coastwatch,
                &coastwatch,
                &profile(Exposure::Tailscale),
                Some(&node),
            )
            .unwrap(),
        ] {
            let origin = url::Url::parse(&topology.browser_origin).unwrap();
            let backend =
                url::Url::parse(&format!("http://{}", topology.backend_authority)).unwrap();
            assert_eq!(origin.host_str(), backend.host_str());
            assert_eq!(topology.cookie_domain, None);
            assert_eq!(topology.cookie_path, "/");
            assert!(topology.cookie_secure);
        }
    }

    #[test]
    fn cross_category_and_same_app_bind_collisions_are_rejected() {
        let profile = profile(Exposure::Localhost);
        let trawl = resolve(App::Trawl, &web(), &profile, None).unwrap();
        let mut collision = web();
        collision.local_port = trawl.api_bind.rsplit_once(':').unwrap().1.parse().unwrap();
        collision.backend_port = 3002;
        let coastwatch = resolve(App::Coastwatch, &collision, &profile, None).unwrap();
        assert!(validate_unique(&[trawl, coastwatch]).is_err());

        let mut self_collision = web();
        self_collision.backend_port = self_collision.local_port;
        let topology = resolve(App::Trawl, &self_collision, &profile, None).unwrap();
        assert!(validate_unique(&[topology]).is_err());
    }
}
