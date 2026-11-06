// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! TCP syslog listener with RFC 6587 framing support.
//!
//! Accepts TCP connections and reads syslog messages using either
//! newline-delimited framing (most common) or RFC 6587 octet-counting.
//!
//! A failed `accept()`, such as EMFILE when the process is out of file
//! descriptors, does not end the listener: it logs under a throttle, waits
//! out the shared backoff and accepts again on the same socket (see
//! `listener_error`). Only a failed bind is fatal.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};

use crate::config::SyslogConfig;
use crate::state::SyslogStats;

use super::CidrEntry;
use super::batch::SyslogSender;
use super::convert::SyslogDoor;
use super::listener_error::{Incoming, Listener, ListenerErrors};
use crate::shutdown::shutdown_observed;

/// Maximum size for a single TCP syslog message (64 KB).
const MAX_MESSAGE_SIZE: usize = 65_536;

/// Run the TCP syslog listener until shutdown.
///
/// Only a failed bind returns `Err`.
pub async fn run_tcp_listener(
    config: &SyslogConfig,
    door: &Arc<SyslogDoor>,
    sender: SyslogSender,
    cidrs: Arc<[CidrEntry]>,
    stats: Option<Arc<SyslogStats>>,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<(), std::io::Error> {
    let listener = TcpListener::bind(&config.tcp_addr).await?;
    tracing::info!(
        event_type = "syslog_tcp_listening",
        addr = %config.tcp_addr,
        max_connections = config.max_tcp_connections,
        "syslog TCP listener started"
    );
    serve_tcp(
        Incoming::new(listener),
        config,
        door,
        sender,
        cidrs,
        stats,
        shutdown_rx,
    )
    .await
}

impl Incoming<TcpListener> {
    async fn accept(&mut self) -> std::io::Result<(TcpStream, SocketAddr)> {
        if let Some(fault) = self.attempt() {
            return Err(fault);
        }
        self.socket.accept().await
    }
}

/// Accept on a bound listener until shutdown.
async fn serve_tcp(
    mut incoming: Incoming<TcpListener>,
    config: &SyslogConfig,
    door: &Arc<SyslogDoor>,
    sender: SyslogSender,
    cidrs: Arc<[CidrEntry]>,
    stats: Option<Arc<SyslogStats>>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), std::io::Error> {
    let semaphore = Arc::new(Semaphore::new(config.max_tcp_connections));

    let mut errors = ListenerErrors::new(Listener::Tcp, shutdown_rx.clone());

    loop {
        tokio::select! {
            biased;

            () = shutdown_observed(&mut shutdown_rx) => break,

            result = incoming.accept() => {
                // Any error, such as EMFILE when the process is out of file
                // descriptors, ends nothing: log it under the throttle, wait
                // out the backoff, and ask the same socket again.
                let (stream, src_addr) = match result {
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
                        event_type = "syslog_tcp_rejected",
                        source = %source_ip,
                        "TCP connection from non-allowed source"
                    );
                    continue;
                }

                let Ok(permit) = semaphore.clone().try_acquire_owned() else {
                    tracing::warn!(
                        event_type = "syslog_tcp_limit_reached",
                        source = %source_ip,
                        "max TCP connections reached, dropping connection"
                    );
                    continue;
                };

                metrics::gauge!(crate::metrics::SYSLOG_TCP_CONNECTIONS).increment(1.0);
                if let Some(ref s) = stats {
                    s.tcp_connections.fetch_add(1, Ordering::Relaxed);
                }

                let conn_sender = sender.clone();
                let conn_stats = stats.clone();
                let conn_config_service_map = config.source_service_map.clone();
                let conn_default_service = config.default_service.clone();
                let conn_door = Arc::clone(door);
                let idle_timeout = Duration::from_secs(config.tcp_idle_timeout_secs);
                let max_events = config.max_events_per_connection;
                let conn_shutdown = shutdown_rx.clone();

                tokio::spawn(async move {
                    handle_tcp_connection(
                        stream,
                        source_ip,
                        conn_sender,
                        &conn_door,
                        &conn_config_service_map,
                        &conn_default_service,
                        idle_timeout,
                        max_events,
                        conn_stats.as_ref(),
                        conn_shutdown,
                    )
                    .await;

                    metrics::gauge!(crate::metrics::SYSLOG_TCP_CONNECTIONS).decrement(1.0);
                    if let Some(ref s) = conn_stats {
                        s.tcp_connections.fetch_sub(1, Ordering::Relaxed);
                    }
                    drop(permit);
                });
            }
        }
    }

    tracing::info!(
        event_type = "syslog_tcp_shutdown",
        "TCP syslog listener shutting down"
    );
    Ok(())
}

/// Resolve once shutdown has been signalled, including a signal sent
/// before this receiver was cloned. A closed sender never signals.
async fn shutdown_signalled(shutdown_rx: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown_rx.borrow_and_update() {
            return;
        }
        if shutdown_rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Handle a single TCP syslog connection.
///
/// Reads messages using newline-delimited framing. Also supports
/// RFC 6587 octet-counting if the first byte is a digit.
///
/// Each admitted frame is handed to the batcher with an awaited send. While
/// the batcher is blocked on hot-buffer admission (ADR-0043) that send
/// waits, the connection stays open and unread, and TCP flow control pushes
/// back on the client; nothing is dropped and the client is not
/// disconnected for it.
///
/// The connection is closed when:
/// - the client disconnects or sends EOF
/// - the idle timeout fires (no data received within the timeout); it
///   covers reads only, never the wait for batcher capacity
/// - `max_events` events have been processed
/// - shutdown is signalled (a frame still waiting for capacity is counted
///   dropped)
/// - the batcher has gone away
#[allow(clippy::too_many_arguments)]
async fn handle_tcp_connection(
    stream: tokio::net::TcpStream,
    source_ip: std::net::IpAddr,
    sender: SyslogSender,
    door: &SyslogDoor,
    source_service_map: &std::collections::HashMap<String, String>,
    default_service: &str,
    idle_timeout: Duration,
    max_events: usize,
    stats: Option<&Arc<SyslogStats>>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut reader = BufReader::new(stream);
    let mut event_count: usize = 0;

    loop {
        // Wrap the read in an idle timeout — if the client sends nothing
        // for this long, close the connection to free the permit.
        let read_result = tokio::select! {
            biased;

            () = shutdown_signalled(&mut shutdown_rx) => break,
            read = tokio::time::timeout(idle_timeout, read_message(&mut reader)) => read,
        };

        let line = match read_result {
            Err(_elapsed) => {
                tracing::debug!(
                    event_type = "syslog_tcp_idle_timeout",
                    source = %source_ip,
                    events = event_count,
                    "TCP connection idle timeout, closing"
                );
                break;
            }
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => break, // EOF
            Ok(Err(e)) => {
                tracing::debug!(
                    event_type = "syslog_tcp_read_error",
                    source = %source_ip,
                    error = %e,
                    "TCP read error"
                );
                break;
            }
        };

        if line.is_empty() {
            continue;
        }

        // The one door: parse, then canonicalize under the syslog
        // profile. A refusal is counted as a profile reject and the frame
        // dropped (a TCP sender has no reply channel to be told on), but it
        // still spends the connection's event budget, so a client that
        // somehow provokes refusals cannot hold a permit forever.
        if let Some(event) =
            door.admit(&line, source_ip, source_service_map, default_service, "tcp")
        {
            let sent = tokio::select! {
                biased;

                () = shutdown_signalled(&mut shutdown_rx) => {
                    sender.abandon(stats);
                    break;
                }
                sent = sender.send(event, stats) => sent,
            };
            if !sent {
                tracing::debug!(
                    event_type = "syslog_tcp_batcher_closed",
                    source = %source_ip,
                    "syslog batcher is gone, closing TCP connection"
                );
                break;
            }
        }

        event_count += 1;
        if event_count >= max_events {
            tracing::info!(
                event_type = "syslog_tcp_event_limit",
                source = %source_ip,
                events = event_count,
                "TCP connection reached event limit, closing"
            );
            break;
        }
    }
}

/// Read a single syslog message from a TCP stream.
///
/// Auto-detects framing mode: if the first byte is a digit, tries
/// RFC 6587 octet-counting; otherwise uses newline-delimited framing.
async fn read_message(
    reader: &mut BufReader<tokio::net::TcpStream>,
) -> Result<Option<String>, std::io::Error> {
    // Peek at the first byte to detect framing mode
    let buf = reader.fill_buf().await?;
    if buf.is_empty() {
        return Ok(None); // Connection closed
    }

    let first_byte = buf[0];

    if first_byte.is_ascii_digit() {
        // Might be octet-counting: "123 <...message...>"
        match read_octet_counted_or_line(reader).await {
            Ok(result) => Ok(result),
            Err(e) => {
                metrics::counter!(crate::metrics::SYSLOG_PARSE_ERRORS_TOTAL, "transport" => "tcp")
                    .increment(1);
                Err(e)
            }
        }
    } else {
        read_line(reader).await
    }
}

/// Read a newline-delimited message from the stream.
///
/// If the message exceeds `MAX_MESSAGE_SIZE`, the remainder of the line
/// is drained and discarded. Returns an empty string so the caller skips
/// the oversized message without silently splitting it into fragments.
async fn read_line(
    reader: &mut BufReader<tokio::net::TcpStream>,
) -> Result<Option<String>, std::io::Error> {
    let mut line = String::new();
    let bytes_read = reader
        .take(MAX_MESSAGE_SIZE as u64)
        .read_line(&mut line)
        .await?;
    if bytes_read == 0 {
        return Ok(None);
    }

    // If take() capped the read, the line won't end with '\n' — the
    // message was truncated. Drain the remainder to stay in sync.
    if !line.ends_with('\n') {
        drain_to_newline(reader).await?;
        metrics::counter!(crate::metrics::SYSLOG_PARSE_ERRORS_TOTAL, "transport" => "tcp")
            .increment(1);
        tracing::debug!(
            event_type = "syslog_tcp_oversized",
            max_bytes = MAX_MESSAGE_SIZE,
            "TCP syslog message exceeded size limit, discarding"
        );
        return Ok(Some(String::new()));
    }

    let trimmed = line.trim_end_matches(['\n', '\r']).to_owned();
    Ok(Some(trimmed))
}

/// Drain bytes from the reader until a newline is found or EOF.
///
/// Used to skip the remainder of an oversized line-delimited message.
async fn drain_to_newline(
    reader: &mut BufReader<tokio::net::TcpStream>,
) -> Result<(), std::io::Error> {
    loop {
        let mut discard = String::new();
        let n = reader
            .take(MAX_MESSAGE_SIZE as u64)
            .read_line(&mut discard)
            .await?;
        if n == 0 || discard.ends_with('\n') {
            break;
        }
    }
    Ok(())
}

/// Try to read an octet-counted message (RFC 6587).
///
/// Format: `<length> <syslog-message>` where length is the byte count
/// of the message (not including the length prefix and space).
///
/// Falls back to newline-delimited reading if the prefix doesn't match
/// the octet-counting pattern.
async fn read_octet_counted_or_line(
    reader: &mut BufReader<tokio::net::TcpStream>,
) -> Result<Option<String>, std::io::Error> {
    // Read one byte at a time up to the space or newline that ends the
    // potential length prefix.
    let mut prefix = String::new();

    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Ok(None);
        }

        let byte = buf[0];
        reader.consume(1);

        if byte == b' ' && prefix.chars().all(|c| c.is_ascii_digit()) && !prefix.is_empty() {
            break;
        }

        if byte == b'\n' {
            // It was a newline-delimited message that started with a digit
            let trimmed = prefix.trim_end_matches('\r').to_owned();
            return Ok(Some(trimmed));
        }

        prefix.push(byte as char);

        // If we've read too many characters without a separator, treat as newline-delimited
        if prefix.len() > 10 || (!byte.is_ascii_digit() && byte != b'\r') {
            // Read the rest of the line (bounded to prevent oversized message splitting)
            let mut rest = String::new();
            let remaining_budget = MAX_MESSAGE_SIZE.saturating_sub(prefix.len());
            reader
                .take(remaining_budget as u64)
                .read_line(&mut rest)
                .await?;
            prefix.push_str(&rest);

            // If the combined message was truncated, drain the remainder
            if !prefix.ends_with('\n') {
                drain_to_newline(reader).await?;
                metrics::counter!(crate::metrics::SYSLOG_PARSE_ERRORS_TOTAL, "transport" => "tcp")
                    .increment(1);
                return Ok(Some(String::new()));
            }

            let trimmed = prefix.trim_end_matches(['\n', '\r']).to_owned();
            return Ok(Some(trimmed));
        }
    }

    // Octet-counting mode: parse the length prefix
    let length: usize = prefix
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid octet count"))?;

    if length > MAX_MESSAGE_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("octet-counted message too large: {length} bytes"),
        ));
    }

    let mut msg_buf = vec![0u8; length];
    reader.read_exact(&mut msg_buf).await?;

    let msg = String::from_utf8(msg_buf).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "non-UTF-8 syslog message")
    })?;

    Ok(Some(msg))
}

#[cfg(test)]
mod tests {
    use super::super::listener_error::Fault;
    use super::super::listener_error::test_support::{
        Capture, Line, STOPS, assert_burst_lines, assert_stops, faults, stop_faults, test_door,
    };
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// Helper: bind a TCP listener on a free port, return the address.
    async fn bind_free() -> (TcpListener, std::net::SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    /// Helper: connect to the listener, return (client, server) stream pair.
    async fn connect(
        listener: &TcpListener,
        addr: std::net::SocketAddr,
    ) -> (tokio::net::TcpStream, tokio::net::TcpStream) {
        let client_fut = tokio::net::TcpStream::connect(addr);
        let accept_fut = listener.accept();
        let (client_result, accept_result) = tokio::join!(client_fut, accept_fut);
        (client_result.unwrap(), accept_result.unwrap().0)
    }

    /// Poll `fut` once: `true` if it is still pending.
    async fn pending_after_one_poll<F: std::future::Future + Unpin>(fut: &mut F) -> bool {
        crate::test_timing::until_polling("one poll", fut, || true)
            .await
            .is_none()
    }

    #[tokio::test]
    async fn read_line_normal_message() {
        let (listener, addr) = bind_free().await;
        let (mut client, server) = connect(&listener, addr).await;
        let mut reader = BufReader::new(server);

        client
            .write_all(b"<13>Mar 12 10:00:00 host sshd: test\n")
            .await
            .unwrap();
        client.shutdown().await.unwrap();

        let result = read_line(&mut reader).await.unwrap();
        assert_eq!(
            result.as_deref(),
            Some("<13>Mar 12 10:00:00 host sshd: test")
        );
    }

    #[tokio::test]
    async fn read_line_oversized_message_returns_empty_and_drains() {
        let (listener, addr) = bind_free().await;
        let (mut client, server) = connect(&listener, addr).await;
        let mut reader = BufReader::new(server);

        // Send a message that exceeds MAX_MESSAGE_SIZE, followed by a normal one
        let oversized = "x".repeat(MAX_MESSAGE_SIZE + 100);
        client
            .write_all(format!("{oversized}\nnormal message\n").as_bytes())
            .await
            .unwrap();
        client.shutdown().await.unwrap();

        // First read should return empty (oversized was discarded)
        let result1 = read_line(&mut reader).await.unwrap();
        assert_eq!(result1.as_deref(), Some(""));

        // Second read should get the normal message
        let result2 = read_line(&mut reader).await.unwrap();
        assert_eq!(result2.as_deref(), Some("normal message"));
    }

    #[tokio::test]
    async fn read_line_eof_returns_none() {
        let (listener, addr) = bind_free().await;
        let (client, server) = connect(&listener, addr).await;
        let mut reader = BufReader::new(server);

        drop(client); // Close connection immediately

        let result = read_line(&mut reader).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn read_octet_counted_message() {
        let (listener, addr) = bind_free().await;
        let (mut client, server) = connect(&listener, addr).await;
        let mut reader = BufReader::new(server);

        // RFC 6587 octet-counted: "11 hello world" means 11 bytes follow the space
        client.write_all(b"11 hello world").await.unwrap();
        client.shutdown().await.unwrap();

        let result = read_octet_counted_or_line(&mut reader).await.unwrap();
        assert_eq!(result.as_deref(), Some("hello world"));
    }

    #[tokio::test]
    async fn read_octet_counted_oversized_returns_error() {
        let (listener, addr) = bind_free().await;
        let (mut client, server) = connect(&listener, addr).await;
        let mut reader = BufReader::new(server);

        // Claim a message of MAX_MESSAGE_SIZE + 1 bytes
        let length = MAX_MESSAGE_SIZE + 1;
        client
            .write_all(format!("{length} ").as_bytes())
            .await
            .unwrap();
        client.shutdown().await.unwrap();

        let result = read_octet_counted_or_line(&mut reader).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn digit_prefixed_line_delimited_fallback() {
        let (listener, addr) = bind_free().await;
        let (mut client, server) = connect(&listener, addr).await;
        let mut reader = BufReader::new(server);

        // A message starting with digits but followed by a newline (not octet-counting)
        client.write_all(b"42\n").await.unwrap();
        client.shutdown().await.unwrap();

        let result = read_octet_counted_or_line(&mut reader).await.unwrap();
        assert_eq!(result.as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn handle_connection_idle_timeout() {
        let (listener, addr) = bind_free().await;
        let (_client, server) = connect(&listener, addr).await;
        let (tx, _rx) = SyslogSender::channel_for_test(100);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        let start = tokio::time::Instant::now();
        handle_tcp_connection(
            server,
            "127.0.0.1".parse().unwrap(),
            tx,
            &test_door(),
            &std::collections::HashMap::new(),
            "syslog",
            Duration::from_millis(50), // very short timeout for test
            100_000,
            None,
            shutdown_rx,
        )
        .await;
        let elapsed = start.elapsed();

        // Should have disconnected after ~50ms idle timeout
        assert!(elapsed >= Duration::from_millis(40));
        assert!(elapsed < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn handle_connection_max_events_limit() {
        let (listener, addr) = bind_free().await;
        let (mut client, server) = connect(&listener, addr).await;
        let (tx, mut rx) = SyslogSender::channel_for_test(100);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        // Send 5 events, set limit to 3
        let handle = tokio::spawn(async move {
            handle_tcp_connection(
                server,
                "127.0.0.1".parse().unwrap(),
                tx,
                &test_door(),
                &std::collections::HashMap::new(),
                "syslog",
                Duration::from_secs(5),
                3, // max 3 events
                None,
                shutdown_rx,
            )
            .await;
        });

        for _ in 0..5 {
            client.write_all(b"<13>test message\n").await.unwrap();
        }

        handle.await.unwrap();

        // Should have received at most 3 events
        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 3);
    }

    /// AC11: while hot-buffer admission refuses the batcher, a TCP sender
    /// is held, not disconnected, past the idle timeout; once compaction
    /// drains space, every frame lands exactly once, in the WAL and in the
    /// hot buffer.
    ///
    /// Time is paused and the test polls the connection itself, so every
    /// step is ordered by an event: the parked channel send, a timer the
    /// runtime has fired, a hot-buffer count. Nothing waits on the clock.
    #[tokio::test(start_paused = true)]
    #[allow(clippy::too_many_lines)] // one scenario, stated in order
    async fn tcp_stalls_without_disconnect_and_ingests_each_frame_once_after_drain() {
        use super::super::batch::{SyslogBatcher, test_support};
        use crate::test_timing::{until, until_polling};

        const EARLY: usize = 10;
        const LATE: usize = 2;
        const N: usize = EARLY + LATE;
        let idle_timeout = Duration::from_millis(200);

        // External ceiling 15 of 16 events, all 15 charged to telemetry.
        let tmp = tempfile::tempdir().unwrap();
        let (pipeline, hot) = test_support::admission_pipeline(tmp.path(), 16, 1 << 20, 15);
        let config = SyslogConfig {
            channel_capacity: 4,
            batch_max_events: 1,
            batch_interval_ms: 10,
            ..SyslogConfig::default()
        };
        // The fallback poll is far away: only a release may wake the batcher.
        let batcher = SyslogBatcher::new(&config, pipeline, Duration::from_secs(3600), None);
        let sender = batcher.sender();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let batcher_task = tokio::spawn(batcher.run(shutdown_rx.clone()));

        let (listener, addr) = bind_free().await;
        let (mut client, server) = connect(&listener, addr).await;

        // The EARLY frames are in the socket before the connection is first
        // polled, so no read has armed an idle timer while they were sent.
        let frame = |i: usize| format!("<13>Mar 12 10:00:00 host app: frame-{i:02}\n");
        for i in 0..EARLY {
            client.write_all(frame(i).as_bytes()).await.unwrap();
        }

        let door = test_door();
        let service_map = std::collections::HashMap::new();
        let conn = handle_tcp_connection(
            server,
            "127.0.0.1".parse().unwrap(),
            sender.clone(),
            &door,
            &service_map,
            "syslog",
            idle_timeout,
            100_000,
            None,
            shutdown_rx.clone(),
        );
        let mut conn = std::pin::pin!(conn);

        // Event: the batcher holds a refused group, and the connection's
        // channel send has been polled Pending, so it is parked on capacity.
        let closed = until_polling("the connection parks in its send", &mut conn, || {
            sender.backpressure_for_test() && sender.send_pending_for_test() >= 1
        })
        .await;
        assert!(closed.is_none(), "the connection closed before it stalled");

        // Well past the idle timeout: the connection waits for capacity,
        // and that wait is not idleness. The barrier is armed by its first
        // poll before the clock moves; once the runtime has fired it, it has
        // also fired any connection timer due by then.
        let mut barrier = std::pin::pin!(tokio::time::sleep(idle_timeout * 3));
        assert!(pending_after_one_poll(&mut barrier).await);
        tokio::time::advance(idle_timeout * 3).await;
        // Event: the runtime fired the barrier timer.
        let fired = until_polling("the barrier fires", &mut barrier, || false).await;
        assert!(fired.is_some());
        assert!(
            pending_after_one_poll(&mut conn).await,
            "the connection is held, not closed"
        );
        let mut probe = [0u8; 1];
        let read = client.try_read(&mut probe);
        assert!(
            read.as_ref()
                .is_err_and(|err| err.kind() == std::io::ErrorKind::WouldBlock),
            "no EOF or reset from the server while stalled: {read:?}"
        );
        for i in EARLY..N {
            client.write_all(frame(i).as_bytes()).await.unwrap();
        }
        assert_eq!(hot.event_count(), 15, "nothing admitted while full");
        assert!(sender.backpressure_for_test());

        // From here on the clock stays where it is: the drain is the only
        // thing that can move the frames.
        hot.drain(&[test_support::FILLER_ID]);
        // Event: the hot buffer holds every frame.
        let closed = until_polling("every frame lands", &mut conn, || hot.event_count() == N).await;
        assert!(closed.is_none(), "the connection closed during the drain");
        assert!(
            pending_after_one_poll(&mut conn).await,
            "still open after the drain"
        );

        let count_frames = |lines: &[String]| -> Vec<usize> {
            (0..N)
                .map(|i| {
                    let needle = format!("frame-{i:02}\"");
                    lines.iter().filter(|line| line.contains(&needle)).count()
                })
                .collect()
        };
        let wal = test_support::wal_lines(tmp.path(), "prod");
        assert_eq!(wal.len(), N, "{wal:?}");
        assert_eq!(count_frames(&wal), vec![1; N], "each frame once in the WAL");
        let scratch = crate::hot_buffer::TestScratch::new();
        let snapshot = hot.snapshot(&scratch).unwrap().expect("resident events");
        let hot_lines: Vec<String> = std::fs::read_to_string(snapshot.path())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(hot_lines.len(), N, "{hot_lines:?}");
        assert_eq!(
            count_frames(&hot_lines),
            vec![1; N],
            "each frame once in the hot buffer"
        );

        // Event: the connection returns on the client's EOF.
        drop(client);
        let ended = until_polling("the connection ends on EOF", &mut conn, || false).await;
        assert!(ended.is_some());
        let _ = shutdown_tx.send(true);
        // Event: the batcher task has returned.
        until("the batcher shuts down", || batcher_task.is_finished()).await;
        batcher_task.await.unwrap();
        assert_eq!(hot.event_count(), N, "shutdown adds nothing");
    }

    /// Shutdown closes a connection that is waiting for batcher capacity.
    #[tokio::test]
    async fn shutdown_releases_a_connection_waiting_for_capacity() {
        let (listener, addr) = bind_free().await;
        let (mut client, server) = connect(&listener, addr).await;
        // Nobody receives: the second frame waits for room.
        let (tx, _rx) = SyslogSender::channel_for_test(1);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let conn = tokio::spawn(async move {
            handle_tcp_connection(
                server,
                "127.0.0.1".parse().unwrap(),
                tx,
                &test_door(),
                &std::collections::HashMap::new(),
                "syslog",
                Duration::from_secs(60),
                100_000,
                None,
                shutdown_rx,
            )
            .await;
        });
        client
            .write_all(b"<13>test one\n<13>test two\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!conn.is_finished());
        let _ = shutdown_tx.send(true);
        tokio::time::timeout(Duration::from_secs(2), conn)
            .await
            .expect("shutdown closes the waiting connection")
            .unwrap();
    }

    /// Spawn `serve_tcp` on a bound loopback listener whose first accepts
    /// answer `faults`. Returns the listener's address, its attempt count,
    /// the batcher's receiving end, the shutdown sender and the task.
    #[allow(clippy::type_complexity)]
    async fn spawn_faulty(
        faults: Vec<Fault>,
        stats: Option<Arc<SyslogStats>>,
    ) -> (
        std::net::SocketAddr,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::sync::mpsc::Receiver<super::super::batch::SyslogEvent>,
        watch::Sender<bool>,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ) {
        let (listener, addr) = bind_free().await;
        let (incoming, attempts) = Incoming::with_faults(listener, faults);
        let (tx, rx) = SyslogSender::channel_for_test(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            serve_tcp(
                incoming,
                &SyslogConfig::default(),
                &Arc::new(test_door()),
                tx,
                Arc::from(Vec::new()),
                stats,
                shutdown_rx,
            )
            .await
        });
        (addr, attempts, rx, shutdown_tx, task)
    }

    /// AC1: after injected accept errors the listener still holds its
    /// socket, and a real connection to that same address delivers its one
    /// line exactly once.
    #[tokio::test]
    async fn tcp_accept_recovers() {
        const FAULTS: usize = 3;
        let hang = crate::test_timing::HANG_GUARD;
        let stats = Arc::new(SyslogStats::default());
        let (addr, attempts, mut rx, shutdown_tx, mut task) =
            spawn_faulty(faults(FAULTS), Some(Arc::clone(&stats))).await;

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        // Not unwrapped yet: a listener that ended resets the connection,
        // and the task's result says why.
        let write = client
            .write_all(b"<13>Mar 12 10:00:00 host app: recover-me\n")
            .await;
        let event = tokio::select! {
            ended = &mut task => panic!("the listener ended on an accept error: {ended:?}"),
            event = tokio::time::timeout(hang, rx.recv()) => event.expect("hang guard"),
        }
        .expect("the batcher channel closed");
        write.unwrap();
        assert_eq!(event.transport, "tcp");
        assert!(
            format!("{:?}", event.map).contains("recover-me"),
            "{:?}",
            event.map
        );
        assert!(
            attempts.load(Ordering::Relaxed) > FAULTS,
            "the socket was asked after the faults"
        );

        // Exactly once: close the client, wait for its handler to finish,
        // stop the listener, and drain the channel until every sender is
        // gone.
        drop(client);
        crate::test_timing::until("the connection handler ends", || {
            stats.tcp_connections.load(Ordering::Relaxed) == 0
        })
        .await;
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
        assert_eq!(more, 0, "the line was delivered more than once");
    }

    /// Drive `serve_tcp` through `n` injected accept errors on the paused
    /// clock, until a connection that waited in the backlog delivers its
    /// line, and return the socket-error lines it logged.
    async fn tcp_burst(n: usize) -> Vec<Line> {
        let capture = Capture::default();
        let _guard = capture.install();
        let (listener, addr) = bind_free().await;
        let (incoming, _attempts) = Incoming::with_faults(listener, faults(n));
        let (tx, mut rx) = SyslogSender::channel_for_test(16);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let config = SyslogConfig::default();
        let door = Arc::new(test_door());
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"<13>Mar 12 10:00:00 host app: after-the-burst\n")
            .await
            .unwrap();
        // The runtime auto-advances the paused clock to each backoff's end.
        tokio::select! {
            ended = serve_tcp(incoming, &config, &door, tx, Arc::from(Vec::new()), None, shutdown_rx) => {
                panic!("the listener ended: {ended:?}")
            }
            event = rx.recv() => assert!(event.is_some()),
        }
        capture.lines()
    }

    /// AC3: in every way shutdown can reach a listener in or out of its
    /// backoff, it returns at once with no further accept.
    #[tokio::test(start_paused = true)]
    async fn listener_shutdown_during_backoff() {
        let config = SyslogConfig::default();
        let door = Arc::new(test_door());
        for stop in STOPS {
            let (listener, _addr) = bind_free().await;
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let (incoming, attempts) =
                Incoming::with_faults(listener, stop_faults(stop, &shutdown_tx));
            let (tx, _rx) = SyslogSender::channel_for_test(1);
            let serve = serve_tcp(
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
        let (_held, addr) = bind_free().await;
        let config = SyslogConfig {
            tcp_addr: addr.to_string(),
            ..SyslogConfig::default()
        };
        let (tx, _rx) = SyslogSender::channel_for_test(1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let door = Arc::new(test_door());
        let run = run_tcp_listener(&config, &door, tx, Arc::from(Vec::new()), None, shutdown_rx);
        let result = tokio::time::timeout(crate::test_timing::HANG_GUARD, run)
            .await
            .expect("the listener ran on a port another socket holds");
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::AddrInUse);
    }

    /// AC5: 650 accept errors, 65 s of backoffs, log four lines.
    #[tokio::test(start_paused = true)]
    async fn listener_error_logs_are_bounded() {
        let lines = tcp_burst(650).await;
        assert_burst_lines(
            &lines,
            "syslog_tcp_accept_failed",
            "syslog_tcp_accept_recovered",
            650,
        );
    }

    /// AC6: the first failure, a summary and the recovery line from the
    /// real loop all carry the pre-auth target, which is never persisted.
    #[tokio::test(start_paused = true)]
    async fn listener_error_events_use_nonpersisted_target() {
        let lines = tcp_burst(302).await;
        assert_burst_lines(
            &lines,
            "syslog_tcp_accept_failed",
            "syslog_tcp_accept_recovered",
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
    /// accepts at once while the first waits out its errors.
    #[tokio::test]
    async fn listener_faults_are_instance_local() {
        let hang = crate::test_timing::HANG_GUARD;
        let (_faulty_addr, faulty_attempts, mut faulty_rx, _faulty_tx, faulty) =
            spawn_faulty(faults(1_000), None).await;
        let (addr, attempts, mut rx, _shutdown_tx, clean) = spawn_faulty(Vec::new(), None).await;
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"<13>Mar 12 10:00:00 host app: clean\n")
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
            "one accept, then the next"
        );
        assert!(faulty_attempts.load(Ordering::Relaxed) < 1_000);
        assert!(faulty_rx.try_recv().is_err());
        assert!(!faulty.is_finished() && !clean.is_finished());
    }
}
