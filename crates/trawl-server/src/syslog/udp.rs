// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! UDP syslog listener.
//!
//! Binds a UDP socket and receives syslog datagrams. Each datagram is
//! parsed, canonicalized through the one door under the `syslog` profile
//! ([`convert::SyslogDoor::admit`]), and sent to the batcher.
//!
//! A failed `recv_from` does not end the listener: it logs under a
//! throttle, waits out the shared backoff and receives again on the same
//! socket (see `listener_error`). Datagrams the kernel drops meanwhile are
//! lost. Only a failed bind is fatal.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::net::UdpSocket;
use tokio::sync::watch;

use crate::config::SyslogConfig;
use crate::state::SyslogStats;

use super::CidrEntry;
use super::batch::SyslogSender;
use super::convert::SyslogDoor;
use super::listener_error::{Incoming, Listener, ListenerErrors};
use crate::shutdown::shutdown_observed;

/// Maximum UDP datagram size for syslog (64 KB per RFC).
const UDP_RECV_BUFFER_SIZE: usize = 65_536;

/// Run the UDP syslog listener until shutdown.
///
/// Only a failed bind returns `Err`.
pub async fn run_udp_listener(
    config: &SyslogConfig,
    door: &SyslogDoor,
    sender: SyslogSender,
    cidrs: Arc<[CidrEntry]>,
    stats: Option<Arc<SyslogStats>>,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<(), std::io::Error> {
    let socket = UdpSocket::bind(&config.udp_addr).await?;
    tracing::info!(
        event_type = "syslog_udp_listening",
        addr = %config.udp_addr,
        "syslog UDP listener started"
    );
    serve_udp(
        Incoming::new(socket),
        config,
        door,
        sender,
        cidrs,
        stats,
        shutdown_rx,
    )
    .await
}

impl Incoming<UdpSocket> {
    async fn recv_from(&mut self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        if let Some(fault) = self.attempt() {
            return Err(fault);
        }
        self.socket.recv_from(buf).await
    }
}

/// Receive on a bound socket until shutdown.
async fn serve_udp(
    mut incoming: Incoming<UdpSocket>,
    config: &SyslogConfig,
    door: &SyslogDoor,
    sender: SyslogSender,
    cidrs: Arc<[CidrEntry]>,
    stats: Option<Arc<SyslogStats>>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), std::io::Error> {
    let mut buf = vec![0u8; UDP_RECV_BUFFER_SIZE];

    let mut errors = ListenerErrors::new(Listener::Udp, shutdown_rx.clone());

    loop {
        tokio::select! {
            biased;

            () = shutdown_observed(&mut shutdown_rx) => break,

            result = incoming.recv_from(&mut buf) => {
                // Any error, such as EMFILE when the process is out of file
                // descriptors, ends nothing: log it under the throttle, wait
                // out the backoff, and ask the same socket again.
                let (len, src_addr) = match result {
                    Ok(done) => done,
                    Err(error) => {
                        if errors.back_off(&error).await.is_break() {
                            break;
                        }
                        continue;
                    }
                };
                errors.succeeded();
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

                // Non-blocking send: drop if the queue is full. The drop is
                // labelled `backpressure` while the batcher is blocked on
                // hot-buffer admission, `queue_full` otherwise.
                if !sender.try_enqueue(event, stats.as_ref()) {
                    tracing::debug!(
                        event_type = "syslog_event_dropped",
                        source = %source_ip,
                        "syslog event dropped (batcher full)"
                    );
                }
            }
        }
    }

    tracing::info!(
        event_type = "syslog_udp_shutdown",
        "UDP syslog listener shutting down"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::listener_error::Fault;
    use super::super::listener_error::test_support::{
        Capture, Line, STOPS, assert_burst_lines, assert_stops, faults, stop_faults, test_door,
    };
    use super::*;

    /// Spawn `serve_udp` on a bound loopback socket whose first receives
    /// answer `faults`. Returns the socket's address, its attempt count,
    /// the batcher's receiving end, the shutdown sender and the task.
    #[allow(clippy::type_complexity)]
    async fn spawn_faulty(
        faults: Vec<Fault>,
    ) -> (
        SocketAddr,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::sync::mpsc::Receiver<super::super::batch::SyslogEvent>,
        watch::Sender<bool>,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ) {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let (incoming, attempts) = Incoming::with_faults(socket, faults);
        let (tx, rx) = SyslogSender::channel_for_test(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            serve_udp(
                incoming,
                &SyslogConfig::default(),
                &test_door(),
                tx,
                Arc::from(Vec::new()),
                None,
                shutdown_rx,
            )
            .await
        });
        (addr, attempts, rx, shutdown_tx, task)
    }

    /// AC2: after injected receive errors the listener still holds its
    /// socket, and a real datagram to that same address delivers its one
    /// event exactly once.
    #[tokio::test]
    async fn udp_receive_recovers() {
        const FAULTS: usize = 3;
        let hang = crate::test_timing::HANG_GUARD;
        let (addr, attempts, mut rx, shutdown_tx, mut task) = spawn_faulty(faults(FAULTS)).await;

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client
            .send_to(b"<13>Mar 12 10:00:00 host app: recover-me", addr)
            .await
            .unwrap();
        let event = tokio::select! {
            ended = &mut task => panic!("the listener ended on a receive error: {ended:?}"),
            event = tokio::time::timeout(hang, rx.recv()) => event.expect("hang guard"),
        }
        .expect("the batcher channel closed");
        assert_eq!(event.transport, "udp");
        assert!(
            format!("{:?}", event.map).contains("recover-me"),
            "{:?}",
            event.map
        );
        assert!(
            attempts.load(Ordering::Relaxed) > FAULTS,
            "the socket was asked after the faults"
        );

        // Exactly once: stop the listener, its sender's only holder, and
        // drain the channel until it closes.
        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(hang, task)
            .await
            .expect("hang guard")
            .unwrap()
            .unwrap();
        let mut more = 0;
        while tokio::time::timeout(hang, rx.recv())
            .await
            .expect("hang guard")
            .is_some()
        {
            more += 1;
        }
        assert_eq!(more, 0, "the datagram was delivered more than once");
    }

    /// Drive `serve_udp` through `n` injected receive errors on the paused
    /// clock, until a datagram that waited in the socket buffer is
    /// delivered, and return the socket-error lines it logged.
    async fn udp_burst(n: usize) -> Vec<Line> {
        let capture = Capture::default();
        let _guard = capture.install();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let (incoming, _attempts) = Incoming::with_faults(socket, faults(n));
        let (tx, mut rx) = SyslogSender::channel_for_test(16);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let config = SyslogConfig::default();
        let door = test_door();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client
            .send_to(b"<13>Mar 12 10:00:00 host app: after-the-burst", addr)
            .await
            .unwrap();
        // The runtime auto-advances the paused clock to each backoff's end.
        tokio::select! {
            ended = serve_udp(incoming, &config, &door, tx, Arc::from(Vec::new()), None, shutdown_rx) => {
                panic!("the listener ended: {ended:?}")
            }
            event = rx.recv() => assert!(event.is_some()),
        }
        capture.lines()
    }

    /// AC3: in every way shutdown can reach a listener in or out of its
    /// backoff, it returns at once with no further receive.
    #[tokio::test(start_paused = true)]
    async fn listener_shutdown_during_backoff() {
        let config = SyslogConfig::default();
        let door = test_door();
        for stop in STOPS {
            let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let (incoming, attempts) =
                Incoming::with_faults(socket, stop_faults(stop, &shutdown_tx));
            let (tx, _rx) = SyslogSender::channel_for_test(1);
            let serve = serve_udp(
                incoming,
                &config,
                &door,
                tx,
                Arc::from(Vec::new()),
                None,
                shutdown_rx,
            );
            assert_stops(stop, serve, &attempts, shutdown_tx).await;
        }
    }

    /// AC4: a bind to a port a real socket holds is the one fatal error.
    #[tokio::test]
    async fn listener_bind_failure_is_fatal() {
        let held = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let config = SyslogConfig {
            udp_addr: held.local_addr().unwrap().to_string(),
            ..SyslogConfig::default()
        };
        let (tx, _rx) = SyslogSender::channel_for_test(1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let door = test_door();
        let run = run_udp_listener(&config, &door, tx, Arc::from(Vec::new()), None, shutdown_rx);
        let result = tokio::time::timeout(crate::test_timing::HANG_GUARD, run)
            .await
            .expect("the listener ran on a port another socket holds");
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::AddrInUse);
    }

    /// AC5: 650 receive errors, 65 s of backoffs, log four lines.
    #[tokio::test(start_paused = true)]
    async fn listener_error_logs_are_bounded() {
        let lines = udp_burst(650).await;
        assert_burst_lines(
            &lines,
            "syslog_udp_recv_failed",
            "syslog_udp_recv_recovered",
            650,
        );
    }

    /// AC6: the first failure, a summary and the recovery line from the
    /// real loop all carry the pre-auth target, which is never persisted.
    #[tokio::test(start_paused = true)]
    async fn listener_error_events_use_nonpersisted_target() {
        let lines = udp_burst(302).await;
        assert_burst_lines(
            &lines,
            "syslog_udp_recv_failed",
            "syslog_udp_recv_recovered",
            302,
        );
        for line in &lines {
            assert_eq!(
                line.target,
                crate::telemetry::PREAUTH_TRANSPORT_TARGET,
                "{line:?}"
            );
            assert!(
                !crate::telemetry::is_persisted_target(&line.target),
                "{line:?}"
            );
        }
    }

    /// Faults belong to one listener: a second listener in the same process
    /// receives at once while the first waits out its errors.
    #[tokio::test]
    async fn listener_faults_are_instance_local() {
        let hang = crate::test_timing::HANG_GUARD;
        let (_faulty_addr, faulty_attempts, mut faulty_rx, _faulty_tx, faulty) =
            spawn_faulty(faults(1_000)).await;
        let (addr, attempts, mut rx, _shutdown_tx, clean) = spawn_faulty(Vec::new()).await;
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client
            .send_to(b"<13>Mar 12 10:00:00 host app: clean", addr)
            .await
            .unwrap();
        let event = tokio::time::timeout(hang, rx.recv())
            .await
            .expect("hang guard")
            .expect("the clean listener delivers");
        assert!(format!("{:?}", event.map).contains("clean"));
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            2,
            "one receive, then the next"
        );
        assert!(faulty_attempts.load(Ordering::Relaxed) < 1_000);
        assert!(faulty_rx.try_recv().is_err());
        assert!(!faulty.is_finished() && !clean.is_finished());
    }
}
