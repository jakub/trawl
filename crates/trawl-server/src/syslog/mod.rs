// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native syslog listener for receiving logs from network appliances.
//!
//! Appliances like `UniFi` consoles that can't run Vector send syslog
//! (RFC 3164/5424) directly to trawld. This module provides UDP and TCP
//! listeners that parse syslog messages and feed them into the existing
//! WAL → hot buffer → parquet pipeline.

pub mod batch;
pub mod convert;
pub mod parse;
pub mod tcp;
pub mod udp;

use std::net::IpAddr;
use std::sync::Arc;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::SyslogConfig;
use crate::ingest::pipeline::PipelineWriter;
use crate::state::SyslogStats;

use self::batch::SyslogBatcher;
use self::convert::SyslogDoor;

/// Spawn all syslog listeners and the batcher task.
///
/// `door` carries everything the listeners need to reach
/// `envelope::canonicalize`: the env allowlist and `default_env`, the
/// `trusted_relays` CIDRs (the peer check that decides whether a
/// hostname-less frame is peer-filled or kept host-less) and the
/// boot-resolved derivation policy. None of them were threaded here
/// before, which is why the listener hand-rolled its own envelope.
///
/// Returns join handles that complete when all listeners and the batcher
/// have shut down. Send `true` on `shutdown_tx` to initiate graceful shutdown.
pub fn spawn_syslog(
    config: &SyslogConfig,
    door: Arc<SyslogDoor>,
    pipeline: Arc<PipelineWriter>,
    syslog_stats: Option<Arc<SyslogStats>>,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<Vec<JoinHandle<()>>, String> {
    // Canonicalize IP-shaped source_service_map keys so a mapped-form
    // spelling (`::ffff:10.1.2.3` — the only form that matched on a
    // dual-stack bind before peer canonicalization) keeps matching the
    // canonical peer the listeners now hand to derive_service. Two keys
    // folding to one address with different services is a contradiction
    // the operator must resolve — boot-fatal, never a silent pick.
    let mut config = config.clone();
    let mut folded = std::collections::HashMap::with_capacity(config.source_service_map.len());
    for (key, service) in &config.source_service_map {
        let canonical = key
            .parse::<IpAddr>()
            .map_or_else(|_| key.clone(), |ip| canonical_peer(ip).to_string());
        if let Some(prev) = folded.get(&canonical)
            && prev != service
        {
            return Err(format!(
                "syslog.source_service_map: {key:?} folds to {canonical:?}, which \
                 is already mapped to service {prev:?} (this entry says {service:?})"
            ));
        }
        folded.insert(canonical, service.clone());
    }
    config.source_service_map = folded;
    let config = &config;

    let batcher = SyslogBatcher::new(config, pipeline, syslog_stats.clone());
    let sender = batcher.sender();

    let mut handles = Vec::new();

    // Spawn the batcher background task.
    let batcher_shutdown = shutdown_rx.clone();
    handles.push(tokio::spawn(async move {
        batcher.run(batcher_shutdown).await;
    }));

    let cidrs = parse_cidrs(&config.allow_cidrs);

    // Spawn UDP listener.
    if config.udp_enabled {
        let udp_config = config.clone();
        let udp_door = Arc::clone(&door);
        let udp_sender = sender.clone();
        let udp_shutdown = shutdown_rx.clone();
        let udp_cidrs = cidrs.clone();
        let udp_stats = syslog_stats.clone();
        handles.push(tokio::spawn(async move {
            if let Err(e) = udp::run_udp_listener(
                &udp_config,
                &udp_door,
                udp_sender,
                udp_cidrs,
                udp_stats,
                udp_shutdown,
            )
            .await
            {
                tracing::error!(event_type = "syslog_udp_error", error = %e, "UDP syslog listener failed");
            }
        }));
    }

    // Spawn TCP listener.
    if config.tcp_enabled {
        let tcp_config = config.clone();
        let tcp_door = door;
        let tcp_sender = sender;
        let tcp_shutdown = shutdown_rx;
        let tcp_cidrs = cidrs;
        let tcp_stats = syslog_stats;
        handles.push(tokio::spawn(async move {
            if let Err(e) = tcp::run_tcp_listener(
                &tcp_config,
                &tcp_door,
                tcp_sender,
                tcp_cidrs,
                tcp_stats,
                tcp_shutdown,
            )
            .await
            {
                tracing::error!(event_type = "syslog_tcp_error", error = %e, "TCP syslog listener failed");
            }
        }));
    }

    Ok(handles)
}

/// Canonicalize a peer address at the transport door: a dual-stack listener
/// presents an IPv4 peer as an IPv4-mapped IPv6 address (`::ffff:10.1.2.3`),
/// which would fail the family match in [`CidrEntry::contains`], miss a
/// v4-keyed `source_service_map` entry, and render the mapped spelling into
/// `host`. Every consumer downstream of an accept/recv sees ONE spelling.
pub fn canonical_peer(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

/// A parsed CIDR entry for allowlist checking.
#[derive(Debug, Clone)]
pub struct CidrEntry {
    addr: IpAddr,
    prefix_len: u8,
}

impl CidrEntry {
    /// Check if the given IP matches this CIDR entry.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(host)) => {
                if self.prefix_len == 0 {
                    return true;
                }
                let mask = u32::MAX
                    .checked_shl(u32::from(32 - self.prefix_len))
                    .unwrap_or(0);
                u32::from(net) & mask == u32::from(host) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(host)) => {
                if self.prefix_len == 0 {
                    return true;
                }
                let mask = u128::MAX
                    .checked_shl(u32::from(128 - self.prefix_len))
                    .unwrap_or(0);
                u128::from(net) & mask == u128::from(host) & mask
            }
            _ => false, // v4/v6 mismatch
        }
    }
}

/// A v6 entry wider than the mapped /96 block that covers it (`::/0`,
/// `::ffff:0:0/95`, …) used to admit every mapped v4 peer on a dual-stack
/// bind. Peers now fold to v4 before matching, so such an entry earns an
/// explicit v4 twin for its intersection with the mapped range — which,
/// for any covering prefix < 96, is all of v4.
pub(crate) fn mapped_cover_twin(entry: &CidrEntry) -> Option<CidrEntry> {
    let mapped_base: IpAddr = "::ffff:0.0.0.0".parse().expect("literal address");
    (matches!(entry.addr, IpAddr::V6(_)) && entry.prefix_len < 96 && entry.contains(mapped_base))
        .then_some(CidrEntry {
            addr: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            prefix_len: 0,
        })
}

/// Parse CIDR strings into entries. Invalid entries are logged and skipped.
fn parse_cidrs(cidrs: &[String]) -> Arc<[CidrEntry]> {
    let mut entries = Vec::with_capacity(cidrs.len());
    for cidr in cidrs {
        if let Some(entry) = parse_cidr(cidr) {
            if let Some(twin) = mapped_cover_twin(&entry) {
                entries.push(twin);
            }
            entries.push(entry);
        } else {
            tracing::warn!(
                event_type = "syslog_config_warning",
                cidr = %cidr,
                "invalid CIDR notation, skipping"
            );
        }
    }
    entries.into()
}

pub(crate) fn parse_cidr(cidr: &str) -> Option<CidrEntry> {
    if let Some((addr_str, prefix_str)) = cidr.split_once('/') {
        let addr: IpAddr = addr_str.parse().ok()?;
        let prefix_len: u8 = prefix_str.parse().ok()?;
        let max_prefix = if addr.is_ipv4() { 32 } else { 128 };
        if prefix_len > max_prefix {
            return None;
        }
        Some(canonicalize_entry(CidrEntry { addr, prefix_len }))
    } else {
        // Bare IP without prefix — treat as host address (/32 or /128)
        let addr: IpAddr = cidr.parse().ok()?;
        let prefix_len = if addr.is_ipv4() { 32 } else { 128 };
        Some(canonicalize_entry(CidrEntry { addr, prefix_len }))
    }
}

/// Fold an IPv4-mapped entry (`::ffff:10.0.0.5/128`) into its v4 form so it
/// matches the [`canonical_peer`] the doors now see. A prefix shorter than
/// /96 spans more than the mapped range and is kept as genuine v6.
fn canonicalize_entry(entry: CidrEntry) -> CidrEntry {
    if let IpAddr::V6(v6) = entry.addr
        && entry.prefix_len >= 96
        && let Some(v4) = v6.to_ipv4_mapped()
    {
        return CidrEntry {
            addr: IpAddr::V4(v4),
            prefix_len: entry.prefix_len - 96,
        };
    }
    entry
}

/// Check if a source IP is allowed by the CIDR allowlist.
/// Empty allowlist means all IPs are accepted.
pub fn is_allowed(cidrs: &[CidrEntry], ip: IpAddr) -> bool {
    if cidrs.is_empty() {
        return true;
    }
    cidrs.iter().any(|c| c.contains(ip))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_mapped_peer_canonicalizes_to_v4() {
        // A dual-stack listener hands us `::ffff:10.1.2.3` for a v4 peer; a
        // v4-configured trusted relay must still recognize it (and `host`
        // attribution must render the v4 spelling, not the mapped one).
        let mapped: IpAddr = "::ffff:10.1.2.3".parse().unwrap();
        let canonical = canonical_peer(mapped);
        assert_eq!(canonical, "10.1.2.3".parse::<IpAddr>().unwrap());
        let relay = parse_cidr("10.0.0.0/8").unwrap();
        assert!(
            !relay.contains(mapped),
            "family mismatch is the bug's shape"
        );
        assert!(relay.contains(canonical));
        // The reverse spelling: a mapped-form CIDR entry (the only form
        // that matched on a dual-stack bind before canonicalization)
        // folds to v4 at parse, so it matches the canonical peer too.
        let mapped_entry = parse_cidr("::ffff:10.0.0.5/128").unwrap();
        assert!(mapped_entry.contains("10.0.0.5".parse().unwrap()));
        let mapped_range = parse_cidr("::ffff:10.0.0.0/104").unwrap();
        assert!(mapped_range.contains("10.1.2.3".parse().unwrap()));
        // A prefix spanning more than the mapped range stays genuine v6 as
        // a single entry, but the LIST builders add a v4 twin for its
        // intersection with the mapped block — so a pre-fold config like
        // `::ffff:0:0/95` (or `::/0`) keeps admitting v4 peers.
        let wide = parse_cidr("::ffff:0:0/95").unwrap();
        assert!(!wide.contains("10.0.0.5".parse::<IpAddr>().unwrap()));
        let twin = mapped_cover_twin(&wide).expect("covers the mapped block");
        assert!(twin.contains("10.0.0.5".parse::<IpAddr>().unwrap()));
        let listed = parse_cidrs(&["::ffff:0:0/95".into()]);
        assert!(is_allowed(&listed, "10.0.0.5".parse().unwrap()));
        // A narrow or non-covering v6 entry earns no twin.
        assert!(mapped_cover_twin(&parse_cidr("2001:db8::/32").unwrap()).is_none());
        assert!(mapped_cover_twin(&parse_cidr("::ffff:10.0.0.0/104").unwrap()).is_none());
        // Genuine v6 peers pass through untouched.
        let v6: IpAddr = "2001:db8::1".parse().unwrap();
        assert_eq!(canonical_peer(v6), v6);
    }

    #[test]
    fn cidr_contains_ipv4() {
        let entry = parse_cidr("192.168.0.0/16").unwrap();
        assert!(entry.contains("192.168.1.1".parse().unwrap()));
        assert!(entry.contains("192.168.255.255".parse().unwrap()));
        assert!(!entry.contains("10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn cidr_contains_ipv4_host() {
        let entry = parse_cidr("10.0.0.5/32").unwrap();
        assert!(entry.contains("10.0.0.5".parse().unwrap()));
        assert!(!entry.contains("10.0.0.6".parse().unwrap()));
    }

    #[test]
    fn cidr_contains_ipv6() {
        let entry = parse_cidr("fd00::/8").unwrap();
        assert!(entry.contains("fd00::1".parse().unwrap()));
        assert!(entry.contains("fdff::1".parse().unwrap()));
        assert!(!entry.contains("fe80::1".parse().unwrap()));
    }

    #[test]
    fn is_allowed_empty_allows_all() {
        assert!(is_allowed(&[], "1.2.3.4".parse().unwrap()));
    }

    #[test]
    fn is_allowed_checks_list() {
        let cidrs = parse_cidrs(&["192.168.0.0/16".into(), "10.0.0.0/8".into()]);
        assert!(is_allowed(&cidrs, "192.168.1.1".parse().unwrap()));
        assert!(is_allowed(&cidrs, "10.5.5.5".parse().unwrap()));
        assert!(!is_allowed(&cidrs, "172.16.0.1".parse().unwrap()));
    }

    #[test]
    fn parse_cidr_invalid() {
        assert!(parse_cidr("not-a-cidr").is_none());
        assert!(parse_cidr("192.168.0.0/33").is_none());
        assert!(parse_cidr("/24").is_none());
    }

    #[test]
    fn parse_cidr_bare_ipv4() {
        let entry = parse_cidr("10.0.0.5").unwrap();
        assert_eq!(entry.prefix_len, 32);
        assert!(entry.contains("10.0.0.5".parse().unwrap()));
        assert!(!entry.contains("10.0.0.6".parse().unwrap()));
    }

    #[test]
    fn parse_cidr_bare_ipv6() {
        let entry = parse_cidr("fd00::1").unwrap();
        assert_eq!(entry.prefix_len, 128);
        assert!(entry.contains("fd00::1".parse().unwrap()));
        assert!(!entry.contains("fd00::2".parse().unwrap()));
    }
}
