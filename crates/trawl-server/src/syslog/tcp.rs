//! TCP syslog listener with RFC 6587 framing support.
//!
//! Accepts TCP connections and reads syslog messages using either
//! newline-delimited framing (most common) or RFC 6587 octet-counting.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};

use crate::config::SyslogConfig;

use super::CidrEntry;
use super::batch::{SyslogEvent, SyslogSender};
use super::convert;
use super::parse;

/// Maximum size for a single TCP syslog message (64 KB).
const MAX_MESSAGE_SIZE: usize = 65_536;

/// Run the TCP syslog listener until shutdown.
pub async fn run_tcp_listener(
    config: &SyslogConfig,
    sender: SyslogSender,
    cidrs: Arc<[CidrEntry]>,
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
                let source_ip = src_addr.ip();

                // Check CIDR allowlist
                if !super::is_allowed(&cidrs, source_ip) {
                    tracing::trace!(
                        event_type = "syslog_tcp_rejected",
                        source = %source_ip,
                        "TCP connection from non-allowed source"
                    );
                    continue;
                }

                // Try to acquire a connection permit
                let Ok(permit) = semaphore.clone().try_acquire_owned() else {
                    tracing::warn!(
                        event_type = "syslog_tcp_limit_reached",
                        source = %source_ip,
                        "max TCP connections reached, dropping connection"
                    );
                    continue;
                };

                metrics::gauge!(crate::metrics::SYSLOG_TCP_CONNECTIONS).increment(1.0);

                let conn_sender = sender.clone();
                let conn_config_service_map = config.source_service_map.clone();
                let conn_default_service = config.default_service.clone();
                let idle_timeout = Duration::from_secs(config.tcp_idle_timeout_secs);
                let max_events = config.max_events_per_connection;
                let send_failure_limit = config.consecutive_send_failures_limit;

                tokio::spawn(async move {
                    handle_tcp_connection(
                        stream,
                        source_ip,
                        conn_sender,
                        &conn_config_service_map,
                        &conn_default_service,
                        idle_timeout,
                        max_events,
                        send_failure_limit,
                    )
                    .await;

                    metrics::gauge!(crate::metrics::SYSLOG_TCP_CONNECTIONS).decrement(1.0);
                    drop(permit); // Release the connection permit
                });
            }
        }
    }
}

/// Handle a single TCP syslog connection.
///
/// Reads messages using newline-delimited framing. Also supports
/// RFC 6587 octet-counting if the first byte is a digit.
///
/// The connection is closed when:
/// - the client disconnects or sends EOF
/// - the idle timeout fires (no data received within the timeout)
/// - `max_events` events have been processed
/// - `send_failure_limit` consecutive sends to the batcher fail
#[allow(clippy::too_many_arguments)]
async fn handle_tcp_connection(
    stream: tokio::net::TcpStream,
    source_ip: std::net::IpAddr,
    sender: SyslogSender,
    source_service_map: &std::collections::HashMap<String, String>,
    default_service: &str,
    idle_timeout: Duration,
    max_events: usize,
    send_failure_limit: usize,
) {
    let mut reader = BufReader::new(stream);
    let mut event_count: usize = 0;
    let mut consecutive_send_failures: usize = 0;

    loop {
        // Wrap the read in an idle timeout — if the client sends nothing
        // for this long, close the connection to free the permit.
        let read_result = tokio::time::timeout(idle_timeout, read_message(&mut reader)).await;

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

        let parsed = parse::parse_syslog(&line);
        let (service, map) =
            convert::syslog_to_event(&parsed, source_ip, source_service_map, default_service);

        let event = SyslogEvent {
            service,
            map,
            transport: "tcp",
        };

        if sender.try_send(event).is_err() {
            metrics::counter!(crate::metrics::SYSLOG_EVENTS_DROPPED_TOTAL).increment(1);
            consecutive_send_failures += 1;
            if consecutive_send_failures >= send_failure_limit {
                tracing::warn!(
                    event_type = "syslog_tcp_backpressure_disconnect",
                    source = %source_ip,
                    failures = consecutive_send_failures,
                    "batcher overwhelmed, disconnecting TCP client"
                );
                break;
            }
        } else {
            consecutive_send_failures = 0;
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
    // Read until space or newline to get the potential length prefix
    let mut prefix = String::new();

    // Read characters one at a time to find the separator
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

    // Read exactly `length` bytes
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
        let (tx, _rx) = tokio::sync::mpsc::channel(100);

        let start = tokio::time::Instant::now();
        handle_tcp_connection(
            server,
            "127.0.0.1".parse().unwrap(),
            tx,
            &std::collections::HashMap::new(),
            "syslog",
            Duration::from_millis(50), // very short timeout for test
            100_000,
            100,
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
        let (tx, mut rx) = tokio::sync::mpsc::channel(100);

        // Send 5 events, set limit to 3
        let handle = tokio::spawn(async move {
            handle_tcp_connection(
                server,
                "127.0.0.1".parse().unwrap(),
                tx,
                &std::collections::HashMap::new(),
                "syslog",
                Duration::from_secs(5),
                3, // max 3 events
                100,
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
}
