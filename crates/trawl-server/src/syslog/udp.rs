// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! UDP syslog listener.
//!
//! Binds a UDP socket and receives syslog datagrams. Each datagram is
//! parsed, canonicalized through the one door under the `syslog` profile
//! ([`convert::SyslogDoor::admit`]), and sent to the batcher.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::net::UdpSocket;
use tokio::sync::watch;

use crate::config::SyslogConfig;
use crate::state::SyslogStats;

use super::CidrEntry;
use super::batch::SyslogSender;
use super::convert::SyslogDoor;

/// Maximum UDP datagram size for syslog (64 KB per RFC).
const UDP_RECV_BUFFER_SIZE: usize = 65_536;

/// Run the UDP syslog listener until shutdown.
pub async fn run_udp_listener(
    config: &SyslogConfig,
    door: &SyslogDoor,
    sender: SyslogSender,
    cidrs: Arc<[CidrEntry]>,
    stats: Option<Arc<SyslogStats>>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), std::io::Error> {
    let socket = UdpSocket::bind(&config.udp_addr).await?;
    tracing::info!(
        event_type = "syslog_udp_listening",
        addr = %config.udp_addr,
        "syslog UDP listener started"
    );

    let mut buf = vec![0u8; UDP_RECV_BUFFER_SIZE];

    loop {
        tokio::select! {
            biased;

            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!(event_type = "syslog_udp_shutdown", "UDP syslog listener shutting down");
                    return Ok(());
                }
            }

            result = socket.recv_from(&mut buf) => {
                let (len, src_addr) = result?;
                let source_ip = super::canonical_peer(src_addr.ip());

                if !super::is_allowed(&cidrs, source_ip) {
                    tracing::trace!(
                        event_type = "syslog_udp_rejected",
                        source = %source_ip,
                        "syslog datagram from non-allowed source"
                    );
                    continue;
                }

                let Ok(raw) = std::str::from_utf8(&buf[..len]) else {
                    metrics::counter!(crate::metrics::SYSLOG_PARSE_ERRORS_TOTAL, "transport" => "udp")
                        .increment(1);
                    if let Some(ref s) = stats {
                        s.parse_errors.fetch_add(1, Ordering::Relaxed);
                    }
                    tracing::debug!(
                        event_type = "syslog_parse_error",
                        source = %source_ip,
                        "non-UTF-8 syslog datagram"
                    );
                    continue;
                };

                // The one door: parse, then canonicalize under the syslog
                // profile. A refusal is counted as a profile reject (a
                // server bug: there is nobody to reject a datagram to) and
                // the frame is dropped.
                let Some(event) = door.admit(
                    raw,
                    source_ip,
                    &config.source_service_map,
                    &config.default_service,
                    "udp",
                ) else {
                    continue;
                };

                // Non-blocking send — drop if batcher is overwhelmed
                if sender.try_send(event).is_err() {
                    metrics::counter!(crate::metrics::SYSLOG_EVENTS_DROPPED_TOTAL).increment(1);
                    if let Some(ref s) = stats {
                        s.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    tracing::debug!(
                        event_type = "syslog_event_dropped",
                        source = %source_ip,
                        "syslog event dropped (batcher full)"
                    );
                }
            }
        }
    }
}
