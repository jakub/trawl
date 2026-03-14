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

use self::batch::SyslogBatcher;

/// Spawn all syslog listeners and the batcher task.
///
/// Returns join handles that complete when all listeners and the batcher
/// have shut down. Send `true` on `shutdown_tx` to initiate graceful shutdown.
pub fn spawn_syslog(
    config: &SyslogConfig,
    pipeline: Arc<PipelineWriter>,
    shutdown_rx: watch::Receiver<bool>,
) -> Vec<JoinHandle<()>> {
    let batcher = SyslogBatcher::new(config, pipeline);
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
        let udp_sender = sender.clone();
        let udp_shutdown = shutdown_rx.clone();
        let udp_cidrs = cidrs.clone();
        handles.push(tokio::spawn(async move {
            if let Err(e) =
                udp::run_udp_listener(&udp_config, udp_sender, udp_cidrs, udp_shutdown).await
            {
                tracing::error!(event_type = "syslog_udp_error", error = %e, "UDP syslog listener failed");
            }
        }));
    }

    // Spawn TCP listener.
    if config.tcp_enabled {
        let tcp_config = config.clone();
        let tcp_sender = sender;
        let tcp_shutdown = shutdown_rx;
        let tcp_cidrs = cidrs;
        handles.push(tokio::spawn(async move {
            if let Err(e) =
                tcp::run_tcp_listener(&tcp_config, tcp_sender, tcp_cidrs, tcp_shutdown).await
            {
                tracing::error!(event_type = "syslog_tcp_error", error = %e, "TCP syslog listener failed");
            }
        }));
    }

    handles
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

/// Parse CIDR strings into entries. Invalid entries are logged and skipped.
fn parse_cidrs(cidrs: &[String]) -> Arc<[CidrEntry]> {
    let mut entries = Vec::with_capacity(cidrs.len());
    for cidr in cidrs {
        if let Some(entry) = parse_cidr(cidr) {
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

fn parse_cidr(cidr: &str) -> Option<CidrEntry> {
    let (addr_str, prefix_str) = cidr.split_once('/')?;
    let addr: IpAddr = addr_str.parse().ok()?;
    let prefix_len: u8 = prefix_str.parse().ok()?;
    let max_prefix = if addr.is_ipv4() { 32 } else { 128 };
    if prefix_len > max_prefix {
        return None;
    }
    Some(CidrEntry { addr, prefix_len })
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
        assert!(parse_cidr("192.168.0.0").is_none());
    }
}
