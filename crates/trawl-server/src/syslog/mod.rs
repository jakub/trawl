// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native syslog listener for receiving logs from network appliances.
//!
//! Appliances like `UniFi` consoles that can't run Vector send syslog
//! (RFC 3164/5424) directly to trawld. This module provides UDP and TCP
//! listeners that parse syslog messages and feed them into the existing
//! WAL → hot buffer → parquet pipeline.
//!
//! The peer settings (`default_timezone`, `sender_timezones`,
//! `source_service_map`, `allow_cidrs`) resolve through one contract,
//! [`SyslogPeers::resolve`], which boot, `trawld --check-config` and the
//! doctor's `server.config` check all run, listener enabled or not.

pub mod batch;
pub mod convert;
pub mod parse;
pub mod tcp;
pub mod udp;
pub mod zone;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::SyslogConfig;
use crate::ingest::pipeline::PipelineWriter;
use crate::state::SyslogStats;

use self::batch::SyslogBatcher;
use self::convert::SyslogDoor;
use self::zone::{SyslogZone, SyslogZones, ZoneRefused};

/// Spawn all syslog listeners and the batcher task.
///
/// `door` carries everything the listeners need to reach
/// `envelope::canonicalize`: the env allowlist and `default_env`, the
/// `trusted_relays` CIDRs (the peer check that decides whether a
/// hostname-less frame is peer-filled or kept host-less) and the
/// boot-resolved derivation policy.
///
/// `peers` is the boot-resolved [`SyslogPeers`]: the folded
/// `source_service_map` and the strictly parsed `allow_cidrs` replace the
/// raw settings in `config`.
///
/// `blocked_poll` is the batcher's retry poll while hot-buffer admission
/// refuses it (`ingest.compaction_interval_secs`); a release of hot-buffer
/// charge wakes it sooner.
///
/// Returns join handles that complete when all listeners and the batcher
/// have shut down. Send `true` on `shutdown_tx` to initiate graceful shutdown.
pub fn spawn_syslog(
    config: &SyslogConfig,
    peers: &SyslogPeers,
    door: Arc<SyslogDoor>,
    pipeline: Arc<PipelineWriter>,
    blocked_poll: Duration,
    syslog_stats: Option<Arc<SyslogStats>>,
    shutdown_rx: watch::Receiver<bool>,
) -> Vec<JoinHandle<()>> {
    let mut config = config.clone();
    config
        .source_service_map
        .clone_from(&peers.source_service_map);
    let config = &config;

    let batcher = SyslogBatcher::new(config, pipeline, blocked_poll, syslog_stats.clone());
    let sender = batcher.sender();

    let mut handles = Vec::new();

    let batcher_shutdown = shutdown_rx.clone();
    handles.push(tokio::spawn(async move {
        batcher.run(batcher_shutdown).await;
    }));

    let cidrs = Arc::clone(&peers.allow_cidrs);

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

    handles
}

/// The syslog peer settings, resolved once under one contract (ADR-0050).
#[derive(Debug, Clone)]
pub struct SyslogPeers {
    /// `source_service_map` with its keys folded to canonical peers.
    pub source_service_map: HashMap<String, String>,
    /// The zone each peer's zone-less timestamps are read in.
    pub zones: SyslogZones,
    /// `allow_cidrs`, every entry parsed. Empty admits every peer.
    pub allow_cidrs: Arc<[CidrEntry]>,
}

impl SyslogPeers {
    /// Parse the zones, fold the keys of `sender_timezones` and
    /// `source_service_map`, and parse `allow_cidrs`, refusing the first
    /// fault. Boot, `trawld --check-config` and the doctor all call this,
    /// whether or not the listener is enabled.
    ///
    /// # Errors
    /// The first [`SyslogPeerFault`], which names the setting and never
    /// its value.
    pub fn resolve(config: &SyslogConfig) -> Result<Self, SyslogPeerFault> {
        let default = match &config.default_timezone {
            None => SyslogZone::UTC,
            Some(zone) => zone
                .parse()
                .map_err(|ZoneRefused| SyslogPeerFault::DefaultZone)?,
        };
        let sender_zones = config
            .sender_timezones
            .iter()
            .map(|(peer, zone)| {
                let zone = zone
                    .parse::<SyslogZone>()
                    .map_err(|ZoneRefused| SyslogPeerFault::SenderZone)?;
                Ok((peer.clone(), zone))
            })
            .collect::<Result<HashMap<_, _>, _>>()?;
        let by_peer = fold_peer_map(&sender_zones, PeerMap::SenderTimezones)?;
        let source_service_map =
            fold_peer_map(&config.source_service_map, PeerMap::SourceServiceMap)?;
        let allow_cidrs = parse_allow_cidrs(&config.allow_cidrs)
            .map_err(|index| SyslogPeerFault::Cidr { index })?;
        Ok(Self {
            source_service_map,
            zones: SyslogZones::new(default, by_peer),
            allow_cidrs,
        })
    }
}

/// A syslog setting keyed by peer address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerMap {
    /// `syslog.sender_timezones`.
    SenderTimezones,
    /// `syslog.source_service_map`.
    SourceServiceMap,
}

impl PeerMap {
    const fn setting(self) -> &'static str {
        match self {
            Self::SenderTimezones => "syslog.sender_timezones",
            Self::SourceServiceMap => "syslog.source_service_map",
        }
    }
}

/// Why the syslog peer settings do not resolve.
///
/// It carries no key, value, service or zone, so neither `Display` nor
/// `Debug` can echo configuration (ADR-0047). The `allow_cidrs` index is
/// the only thing it adds to the setting path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyslogPeerFault {
    /// `syslog.default_timezone` is not a zone.
    DefaultZone,
    /// A `syslog.sender_timezones` value is not a zone.
    SenderZone,
    /// A key of the map is not an IP address.
    KeyNotAddress(PeerMap),
    /// Two keys of the map name one peer with different values.
    FoldConflict(PeerMap),
    /// The `syslog.allow_cidrs` entry at `index` does not parse.
    Cidr {
        /// The entry's zero-based position in the list.
        index: usize,
    },
}

impl SyslogPeerFault {
    /// The setting at fault, without any index.
    #[must_use]
    pub const fn setting(self) -> &'static str {
        match self {
            Self::DefaultZone => "syslog.default_timezone",
            Self::SenderZone => PeerMap::SenderTimezones.setting(),
            Self::KeyNotAddress(map) | Self::FoldConflict(map) => map.setting(),
            Self::Cidr { .. } => "syslog.allow_cidrs",
        }
    }

    /// The `allow_cidrs` index, for the one fault that has one.
    #[must_use]
    pub const fn index(self) -> Option<usize> {
        match self {
            Self::Cidr { index } => Some(index),
            _ => None,
        }
    }

    /// Why the setting is refused.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::DefaultZone | Self::SenderZone => ZoneRefused::REASON,
            Self::KeyNotAddress(_) => "a key is not an IP address",
            Self::FoldConflict(PeerMap::SenderTimezones) => {
                "two keys name one peer with different zones"
            }
            Self::FoldConflict(PeerMap::SourceServiceMap) => {
                "two keys name one peer with different services"
            }
            Self::Cidr { .. } => "not an IP address or CIDR",
        }
    }
}

impl std::fmt::Display for SyslogPeerFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid setting at {}", self.setting())?;
        if let Some(index) = self.index() {
            write!(f, "[{index}]")?;
        }
        write!(f, ": {}", self.reason())
    }
}

impl std::error::Error for SyslogPeerFault {}

/// Canonicalize a peer address at the transport door: a dual-stack listener
/// presents an IPv4 peer as an IPv4-mapped IPv6 address (`::ffff:10.1.2.3`),
/// which would fail the family match in [`CidrEntry::contains`], miss a
/// v4-keyed `source_service_map` entry, and render the mapped spelling into
/// `host`. Every consumer downstream of an accept/recv sees one spelling.
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

/// Fold a peer-keyed map's keys to the [`canonical_peer`] spelling the
/// listeners look up, so a mapped-form key (`::ffff:10.1.2.3`) keeps
/// matching its v4 peer. A key that is not an IP address could never
/// match a peer, so it is refused. Two keys folding to one peer with
/// different values is a contradiction the operator must resolve, so it is
/// refused rather than silently picked; equal values dedup.
fn fold_peer_map<V: Clone + PartialEq>(
    map: &HashMap<String, V>,
    which: PeerMap,
) -> Result<HashMap<String, V>, SyslogPeerFault> {
    // Sorted, so a map with several faults always reports the same one.
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    let mut folded = HashMap::with_capacity(map.len());
    for key in keys {
        let value = &map[key];
        let peer = key
            .parse::<IpAddr>()
            .map_err(|_| SyslogPeerFault::KeyNotAddress(which))?;
        let canonical = canonical_peer(peer).to_string();
        if folded.get(&canonical).is_some_and(|prev| prev != value) {
            return Err(SyslogPeerFault::FoldConflict(which));
        }
        folded.insert(canonical, value.clone());
    }
    Ok(folded)
}

/// A v6 entry wider than the mapped /96 block (`::/0`, `::ffff:0:0/95`, …)
/// covers mapped v4 peers, but peers fold to v4 before matching. Such an
/// entry therefore earns an explicit v4 twin for its intersection with the
/// mapped range, which for any covering prefix < 96 is all of v4.
pub(crate) fn mapped_cover_twin(entry: &CidrEntry) -> Option<CidrEntry> {
    let mapped_base: IpAddr = "::ffff:0.0.0.0".parse().expect("literal address");
    (matches!(entry.addr, IpAddr::V6(_)) && entry.prefix_len < 96 && entry.contains(mapped_base))
        .then_some(CidrEntry {
            addr: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            prefix_len: 0,
        })
}

/// Parse `allow_cidrs`, refusing the whole list at the index of its first
/// malformed entry. Skipping one would fail open: a list whose every entry
/// was skipped is empty, and an empty list admits every peer.
fn parse_allow_cidrs(cidrs: &[String]) -> Result<Arc<[CidrEntry]>, usize> {
    let mut entries = Vec::with_capacity(cidrs.len());
    for (index, cidr) in cidrs.iter().enumerate() {
        let entry = parse_cidr(cidr).ok_or(index)?;
        if let Some(twin) = mapped_cover_twin(&entry) {
            entries.push(twin);
        }
        entries.push(entry);
    }
    Ok(entries.into())
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
/// matches the [`canonical_peer`] the doors see. A prefix shorter than
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

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// The listener is off in every config these tests resolve: the
    /// contract applies whether or not syslog runs.
    fn disabled(edit: impl FnOnce(&mut SyslogConfig)) -> SyslogConfig {
        let mut config = SyslogConfig::default();
        edit(&mut config);
        assert!(!config.enabled);
        config
    }

    #[test]
    fn source_service_map_keys_fold_with_the_peer() {
        // A mapped-form key folds to the spelling the listeners produce,
        // and a genuine v6 key normalizes its rendering (case,
        // compression).
        let peers = SyslogPeers::resolve(&disabled(|c| {
            c.source_service_map = map(&[("::ffff:10.1.2.3", "ap"), ("2001:DB8::1", "router")]);
        }))
        .unwrap();
        let folded = &peers.source_service_map;
        assert_eq!(folded.get("10.1.2.3").map(String::as_str), Some("ap"));
        assert_eq!(
            folded.get("2001:db8::1").map(String::as_str),
            Some("router")
        );
        assert_eq!(folded.len(), 2);
        // A key that is not an address could never match a peer.
        let fault = SyslogPeers::resolve(&disabled(|c| {
            c.source_service_map = map(&[("host.local", "printer")]);
        }))
        .unwrap_err();
        assert_eq!(
            fault,
            SyslogPeerFault::KeyNotAddress(PeerMap::SourceServiceMap)
        );
    }

    /// Every refusal names the setting and a reason, and neither `Display`
    /// nor `Debug` carries the key, value, service or zone at fault.
    #[test]
    fn syslog_peer_settings_refuse_with_the_setting_and_never_the_value() {
        type Edit = fn(&mut SyslogConfig);
        let cases: [(Edit, SyslogPeerFault, &str); 13] = [
            (
                |c| c.default_timezone = Some("local".into()),
                SyslogPeerFault::DefaultZone,
                "invalid setting at syslog.default_timezone: not UTC, an IANA zone name, or a ±HH:MM offset",
            ),
            (
                |c| c.default_timezone = Some("EST".into()),
                SyslogPeerFault::DefaultZone,
                "syslog.default_timezone",
            ),
            (
                |c| c.default_timezone = Some("Sentinel/Zone_q7x".into()),
                SyslogPeerFault::DefaultZone,
                "syslog.default_timezone",
            ),
            (
                |c| c.sender_timezones = map(&[("198.51.100.7", "CET")]),
                SyslogPeerFault::SenderZone,
                "invalid setting at syslog.sender_timezones: not UTC",
            ),
            (
                |c| c.sender_timezones = map(&[("198.51.100.7", "Sentinel/Zone_q7x")]),
                SyslogPeerFault::SenderZone,
                "syslog.sender_timezones",
            ),
            (
                |c| c.sender_timezones = map(&[("sentinel-q7x.local", "UTC")]),
                SyslogPeerFault::KeyNotAddress(PeerMap::SenderTimezones),
                "invalid setting at syslog.sender_timezones: a key is not an IP address",
            ),
            (
                |c| c.sender_timezones = map(&[("198.51.100.0/24", "UTC")]),
                SyslogPeerFault::KeyNotAddress(PeerMap::SenderTimezones),
                "syslog.sender_timezones",
            ),
            (
                |c| {
                    c.sender_timezones = map(&[
                        ("198.51.100.7", "Europe/Warsaw"),
                        ("::ffff:198.51.100.7", "America/Chicago"),
                    ]);
                },
                SyslogPeerFault::FoldConflict(PeerMap::SenderTimezones),
                "invalid setting at syslog.sender_timezones: two keys name one peer with different zones",
            ),
            (
                |c| c.source_service_map = map(&[("sentinel-q7x.local", "printer")]),
                SyslogPeerFault::KeyNotAddress(PeerMap::SourceServiceMap),
                "invalid setting at syslog.source_service_map: a key is not an IP address",
            ),
            (
                |c| {
                    c.source_service_map = map(&[
                        ("198.51.100.7", "sentinel-svc-a"),
                        ("::ffff:198.51.100.7", "sentinel-svc-b"),
                    ]);
                },
                SyslogPeerFault::FoldConflict(PeerMap::SourceServiceMap),
                "invalid setting at syslog.source_service_map: two keys name one peer with different services",
            ),
            (
                |c| c.allow_cidrs = vec!["10.0.0.0/8".into(), "sentinel-q7x/99".into()],
                SyslogPeerFault::Cidr { index: 1 },
                "invalid setting at syslog.allow_cidrs[1]: not an IP address or CIDR",
            ),
            (
                |c| c.allow_cidrs = vec!["198.51.100.0/33".into()],
                SyslogPeerFault::Cidr { index: 0 },
                "syslog.allow_cidrs[0]",
            ),
            // Every entry malformed: refused, never an empty allow-all list.
            (
                |c| c.allow_cidrs = vec!["sentinel-q7x".into(), "also-q7x".into()],
                SyslogPeerFault::Cidr { index: 0 },
                "syslog.allow_cidrs[0]",
            ),
        ];
        for (edit, expected, shown) in cases {
            let fault = SyslogPeers::resolve(&disabled(edit)).unwrap_err();
            assert_eq!(fault, expected);
            let display = fault.to_string();
            assert!(display.contains(shown), "{display}");
            for text in [display, format!("{fault:?}")] {
                for sentinel in [
                    "q7x", "local", "EST", "CET", "198.51", "Warsaw", "Chicago", "svc", "printer",
                ] {
                    assert!(!text.contains(sentinel), "{sentinel} leaked: {text}");
                }
            }
        }
    }

    #[test]
    fn syslog_peer_settings_accept_equal_valued_duplicate_spellings() {
        let peers = SyslogPeers::resolve(&disabled(|c| {
            c.default_timezone = Some("+00:00".into());
            c.sender_timezones = map(&[
                // One peer, three spellings, one normalized zone.
                ("198.51.100.7", "UTC"),
                ("::ffff:198.51.100.7", "+00:00"),
                ("::FFFF:198.51.100.7", "-00:00"),
                ("2001:DB8::1", "Europe/Warsaw"),
                ("2001:db8::1", "Europe/Warsaw"),
            ]);
            c.source_service_map = map(&[("10.1.2.3", "ap"), ("::ffff:10.1.2.3", "ap")]);
        }))
        .unwrap();
        assert_eq!(peers.source_service_map.len(), 1);
        let zones = &peers.zones;
        let warsaw: SyslogZone = "Europe/Warsaw".parse().unwrap();
        assert_eq!(
            zones.for_peer("198.51.100.7".parse().unwrap()),
            SyslogZone::UTC
        );
        assert_eq!(zones.for_peer("2001:db8::1".parse().unwrap()), warsaw);
        assert_eq!(
            zones.for_peer("192.0.2.1".parse().unwrap()),
            SyslogZone::UTC
        );

        // Unset means UTC, and an explicitly empty allowlist stays allow-all.
        let peers = SyslogPeers::resolve(&SyslogConfig::default()).unwrap();
        assert_eq!(peers.zones, SyslogZones::default());
        assert!(peers.allow_cidrs.is_empty());
        assert!(is_allowed(&peers.allow_cidrs, "192.0.2.1".parse().unwrap()));
    }

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
        // The reverse spelling: a mapped-form CIDR entry folds to v4 at
        // parse, so it matches the canonical peer too.
        let mapped_entry = parse_cidr("::ffff:10.0.0.5/128").unwrap();
        assert!(mapped_entry.contains("10.0.0.5".parse().unwrap()));
        let mapped_range = parse_cidr("::ffff:10.0.0.0/104").unwrap();
        assert!(mapped_range.contains("10.1.2.3".parse().unwrap()));
        // A prefix spanning more than the mapped range stays genuine v6 as
        // a single entry, but the list builders add a v4 twin for its
        // intersection with the mapped block, so a config like
        // `::ffff:0:0/95` (or `::/0`) keeps admitting v4 peers.
        let wide = parse_cidr("::ffff:0:0/95").unwrap();
        assert!(!wide.contains("10.0.0.5".parse::<IpAddr>().unwrap()));
        let twin = mapped_cover_twin(&wide).expect("covers the mapped block");
        assert!(twin.contains("10.0.0.5".parse::<IpAddr>().unwrap()));
        let listed = parse_allow_cidrs(&["::ffff:0:0/95".into()]).unwrap();
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
        let cidrs = parse_allow_cidrs(&["192.168.0.0/16".into(), "10.0.0.0/8".into()]).unwrap();
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
