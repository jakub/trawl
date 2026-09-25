// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! TCP syslog listener with RFC 6587 framing support.
//!
//! Accepts TCP connections and reads syslog messages using either
//! newline-delimited framing (most common) or RFC 6587 octet-counting.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};

use crate::config::SyslogConfig;
use crate::state::SyslogStats;

use super::CidrEntry;
use super::batch::SyslogSender;
use super::convert::SyslogDoor;

/// Maximum size for a single TCP syslog message (64 KB).
const MAX_MESSAGE_SIZE: usize = 65_536;

/// Run the TCP syslog listener until shutdown.
pub async fn run_tcp_listener(
    config: &SyslogConfig,
    door: &Arc<SyslogDoor>,
    sender: SyslogSender,
    cidrs: Arc<[CidrEntry]>,
    stats: Option<Arc<SyslogStats>>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), std::io::Error> {
    let listener = TcpListener::bind(&config.tcp_addr).await?;
    let semaphore = Arc::new(Semaphore::new(config.max_tcp_connections));

    tracing::info!(
        event_type = "syslog_tcp_listening",
        addr = %config.tcp_addr,
        max_connections = config.max_tcp_connections,
        "syslog TCP listener started"
    );

    loop {
        tokio::select! {
            biased;

            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!(event_type = "syslog_tcp_shutdown", "TCP syslog listener shutting down");
                    return Ok(());
                }
            }

            result = listener.accept() => {
                let (stream, src_addr) = result?;
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
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// The one door, with the packaged derivation policy and no relays.
    fn test_door() -> SyslogDoor {
        SyslogDoor {
            envs: vec!["prod".to_owned()].into(),
            default_env: "prod".into(),
            trusted_relays: Vec::new().into(),
            derivation: Arc::new(crate::ingest::producer::Derivation::defaults()),
        }
    }

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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tcp_stalls_without_disconnect_and_ingests_each_frame_once_after_drain() {
        use super::super::batch::{SyslogBatcher, test_support};

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
        let conn_sender = sender.clone();
        let conn_shutdown = shutdown_rx.clone();
        let conn = tokio::spawn(async move {
            handle_tcp_connection(
                server,
                "127.0.0.1".parse().unwrap(),
                conn_sender,
                &test_door(),
                &std::collections::HashMap::new(),
                "syslog",
                idle_timeout,
                100_000,
                None,
                conn_shutdown,
            )
            .await;
        });

        let frame = |i: usize| format!("<13>Mar 12 10:00:00 host app: frame-{i:02}\n");
        for i in 0..EARLY {
            client.write_all(frame(i).as_bytes()).await.unwrap();
        }
        test_support::eventually("the batcher blocks", || sender.backpressure_for_test()).await;

        // Well past the idle timeout: the connection waits for capacity,
        // and that wait is not idleness.
        tokio::time::sleep(idle_timeout * 3).await;
        assert!(!conn.is_finished(), "the connection is held, not closed");
        let mut probe = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_millis(100), client.read(&mut probe)).await;
        assert!(
            read.is_err(),
            "no EOF or reset from the server while stalled: {read:?}"
        );
        for i in EARLY..N {
            client.write_all(frame(i).as_bytes()).await.unwrap();
        }
        assert_eq!(hot.event_count(), 15, "nothing admitted while full");
        assert!(sender.backpressure_for_test());

        hot.drain(&[test_support::FILLER_ID]);
        test_support::eventually("every frame lands", || hot.event_count() == N).await;
        assert!(!conn.is_finished(), "still open after the drain");

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
        let snapshot = hot.snapshot().expect("resident events");
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

        drop(client);
        let _ = shutdown_tx.send(true);
        conn.await.unwrap();
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
}
