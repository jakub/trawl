//! TCP syslog listener with RFC 6587 framing support.
//!
//! Accepts TCP connections and reads syslog messages using either
//! newline-delimited framing (most common) or RFC 6587 octet-counting.

use std::sync::Arc;

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

                tokio::spawn(async move {
                    handle_tcp_connection(
                        stream,
                        source_ip,
                        conn_sender,
                        &conn_config_service_map,
                        &conn_default_service,
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
async fn handle_tcp_connection(
    stream: tokio::net::TcpStream,
    source_ip: std::net::IpAddr,
    sender: SyslogSender,
    source_service_map: &std::collections::HashMap<String, String>,
    default_service: &str,
) {
    let mut reader = BufReader::new(stream);

    loop {
        // Peek at the first byte to detect framing mode
        let buf = match reader.fill_buf().await {
            Ok([]) => break, // Connection closed
            Ok(buf) => buf,
            Err(e) => {
                tracing::debug!(
                    event_type = "syslog_tcp_read_error",
                    source = %source_ip,
                    error = %e,
                    "TCP read error"
                );
                break;
            }
        };

        let first_byte = buf[0];

        let line = if first_byte.is_ascii_digit() {
            // Might be octet-counting: "123 <...message...>"
            match read_octet_counted_or_line(&mut reader).await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(
                        event_type = "syslog_tcp_frame_error",
                        source = %source_ip,
                        error = %e,
                        "TCP framing error"
                    );
                    metrics::counter!(crate::metrics::SYSLOG_PARSE_ERRORS_TOTAL, "transport" => "tcp")
                        .increment(1);
                    break;
                }
            }
        } else {
            // Newline-delimited framing
            match read_line(&mut reader).await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(
                        event_type = "syslog_tcp_read_error",
                        source = %source_ip,
                        error = %e,
                        "TCP line read error"
                    );
                    break;
                }
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
        }
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
