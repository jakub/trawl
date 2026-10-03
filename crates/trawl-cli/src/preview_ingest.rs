// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl preview-ingest`: show what trawld's canonicalization would do to
//! a sample, storing nothing of it (ADR-0049).
//!
//! The sample's bytes go to `POST /api/v1/ingest/preview` exactly as read,
//! from a file or stdin. The report prints as a table, or as the server's
//! JSON with `--json`.
//!
//! Exit status: 0 when every event would be accepted; 1 when a complete
//! report has a rejected event (repairs and sender dependence do not fail
//! it); 2 for a usage error, an unreadable sample, a connection or HTTP
//! error, a server that does not ingest (404), or a report that is not
//! complete. [`crate::CliError`]'s own exit is 1, so every failure on this
//! path goes through [`exit_status`].

use std::fmt::Write as _;
use std::io::{self, Read as _, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use trawl_client::{
    ClientError, FieldChangeKind, FieldChangeWire, PreviewEvent, PreviewResponse, SeverityLineage,
    TimeLineage,
};
use trawl_core::sanitize::sanitize_display_text;

use crate::CliError;
use crate::cli::ConnectionParams;

/// Every event would be accepted.
pub const ALL_ACCEPTED: u8 = 0;
/// The report is complete and at least one event would be rejected.
pub const SOME_REJECTED: u8 = 1;
/// No complete report: usage, input, transport or server failure.
pub const NO_REPORT: u8 = 2;

/// What a 404 from the preview route means: the route is mounted only
/// where ingest is enabled, and a server from before ADR-0049 has none.
pub const DOES_NOT_INGEST: &str = "this server does not ingest: ingest is disabled on it, \
     or it predates the ingest preview";

/// `trawl preview-ingest` arguments.
#[derive(Debug, clap::Args)]
pub struct PreviewIngestArgs {
    /// The sample, as NDJSON or a JSON array. `-` or nothing reads stdin.
    #[arg(value_name = "FILE")]
    pub file: Option<PathBuf>,

    /// The sender's address as trawld would see it. Left out, the server
    /// uses the placeholder 192.0.2.1 and marks every event without a
    /// `host` as depending on the real sender.
    #[arg(long, value_name = "IP")]
    pub peer_ip: Option<IpAddr>,

    /// Print the server's report as JSON instead of the table.
    #[arg(long)]
    pub json: bool,
}

/// Read the sample's bytes: the named file, or stdin for `-` or no name.
///
/// # Errors
///
/// A file or stdin that cannot be read, as a usage error naming the path.
pub fn read_sample(file: Option<&Path>) -> Result<Vec<u8>, CliError> {
    let mut body = Vec::new();
    match file {
        None => io::stdin().lock().read_to_end(&mut body).map(|_| ()),
        Some(path) if path == Path::new("-") => {
            io::stdin().lock().read_to_end(&mut body).map(|_| ())
        }
        Some(path) => std::fs::read(path).map(|bytes| body = bytes),
    }
    .map_err(|e| {
        let source = file
            .filter(|path| *path != Path::new("-"))
            .map_or_else(|| "stdin".to_owned(), |path| path.display().to_string());
        CliError::Usage(format!("cannot read {source}: {e}"))
    })?;
    Ok(body)
}

/// Send the sample, check the report is complete, print it, and return
/// [`ALL_ACCEPTED`] or [`SOME_REJECTED`].
///
/// # Errors
///
/// A transport or HTTP failure, a report that does not decode or is not
/// complete, or a failed write. [`exit_status`] maps each to [`NO_REPORT`].
pub async fn run<W: Write>(
    out: &mut W,
    conn: &ConnectionParams,
    body: Vec<u8>,
    args: &PreviewIngestArgs,
) -> Result<u8, CliError> {
    let report = conn.client()?.ingest_preview(body, args.peer_ip).await?;
    check_complete(&report)?;
    if args.json {
        serde_json::to_writer_pretty(&mut *out, &report).map_err(io::Error::other)?;
        writeln!(out)?;
    } else {
        render_table(&report, out)?;
    }
    out.flush()?;
    Ok(if report.rejected > 0 {
        SOME_REJECTED
    } else {
        ALL_ACCEPTED
    })
}

/// The exit status for a preview run's result, reporting a failure on
/// `err`. Every failure is [`NO_REPORT`]: none of them produced a
/// complete report to judge.
pub fn exit_status(result: Result<u8, CliError>, err: &mut impl Write) -> u8 {
    let error = match result {
        Ok(status) => return status,
        Err(error) => error,
    };
    // A closed pipe (`| head`) is not worth a line, but it is still not
    // a complete report.
    if let CliError::Io(io_err) = &error
        && io_err.kind() == io::ErrorKind::BrokenPipe
    {
        return NO_REPORT;
    }
    let _ = match &error {
        CliError::Client(ClientError::Server { status: 404, .. }) => {
            writeln!(err, "trawl: {DOES_NOT_INGEST}")
        }
        CliError::Arg(clap_error) => write!(err, "{}", clap_error.render()),
        // A server error carries the server's own message, so the line is
        // sanitised before it reaches a terminal.
        other => writeln!(err, "trawl: {}", sanitize_display_text(&other.to_string())),
    };
    NO_REPORT
}

/// A report is complete when its counts agree with the events it lists.
/// One that does not is a broken answer, never something to judge.
fn check_complete(report: &PreviewResponse) -> Result<(), CliError> {
    let accepted = report
        .events
        .iter()
        .filter(|event| matches!(event, PreviewEvent::Accepted { .. }))
        .count();
    let rejected = report.events.len() - accepted;
    if accepted == report.accepted && rejected == report.rejected {
        return Ok(());
    }
    Err(CliError::Client(ClientError::Parse(format!(
        "the ingest preview report is incomplete: it counts {} accepted and {} rejected \
         but lists {accepted} accepted and {rejected} rejected events",
        report.accepted, report.rejected
    ))))
}

/// The table's columns, in order.
const COLUMNS: [&str; 9] = [
    "index",
    "outcome",
    "env",
    "service",
    "host",
    "_time",
    "_severity",
    "repairs",
    "reason or changes",
];

/// Render the header line, the per-event table and the footer count.
///
/// Every value is the sample's own text by way of the server, so each
/// goes through display sanitisation before it reaches a terminal.
fn render_table(report: &PreviewResponse, out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "{}", header(report))?;

    let mut table = comfy_table::Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_FULL)
        .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS)
        .set_content_arrangement(comfy_table::ContentArrangement::Dynamic);
    table.set_header(COLUMNS);
    for event in &report.events {
        table.add_row(row(event));
    }
    writeln!(out, "{table}")?;
    writeln!(out, "{}", footer(report))
}

/// Peer, relay classification and arrival, on one line.
fn header(report: &PreviewResponse) -> String {
    let peer = &report.peer;
    let given = if peer.given {
        "given"
    } else {
        "placeholder, no --peer-ip given; an event without a host depends on the sender"
    };
    let relay = if peer.trusted_relay {
        "a trusted relay"
    } else {
        "not a trusted relay"
    };
    format!(
        "peer {} ({given}), {relay}; arrival {}",
        sanitize_display_text(&peer.ip),
        sanitize_display_text(&report.arrival),
    )
}

/// The event count, the outcome split, and how many events the peer
/// left undecided.
fn footer(report: &PreviewResponse) -> String {
    let dependent = report
        .events
        .iter()
        .filter(|event| match event {
            PreviewEvent::Accepted {
                host_depends_on_sender,
                ..
            }
            | PreviewEvent::Rejected {
                host_depends_on_sender,
                ..
            } => *host_depends_on_sender,
        })
        .count();
    let mut line = format!(
        "{} event(s): {} accepted, {} rejected",
        report.events.len(),
        report.accepted,
        report.rejected
    );
    if dependent > 0 {
        let _ = write!(
            line,
            "; {dependent} depend on the sender's address (pass --peer-ip)"
        );
    }
    line
}

/// One event's cells, in [`COLUMNS`] order.
fn row(event: &PreviewEvent) -> Vec<String> {
    match event {
        PreviewEvent::Accepted {
            index,
            event,
            repairs,
            lineage,
            host_depends_on_sender,
            ..
        } => vec![
            index.to_string(),
            "accepted".to_owned(),
            slot(event.get("env")),
            slot(event.get("service")),
            host_cell(slot(event.get("host")), *host_depends_on_sender),
            time_cell(event.get("_time"), &lineage.time),
            severity_cell(event.get("_severity"), &lineage.severity),
            list_cell(repairs.iter().map(|code| sanitize_display_text(code))),
            list_cell(lineage.fields.iter().map(change_text)),
        ],
        PreviewEvent::Rejected {
            index,
            reason,
            message,
            host_depends_on_sender,
            ..
        } => vec![
            index.to_string(),
            "rejected".to_owned(),
            NONE.to_owned(),
            NONE.to_owned(),
            host_cell(NONE.to_owned(), *host_depends_on_sender),
            NONE.to_owned(),
            NONE.to_owned(),
            NONE.to_owned(),
            format!(
                "{}: {}",
                sanitize_display_text(reason),
                sanitize_display_text(message)
            ),
        ],
    }
}

/// The cell for a value the event does not have.
const NONE: &str = "-";

/// A canonical slot's value as text: a string as itself, anything else as
/// compact JSON, an absent slot as [`NONE`].
fn slot(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => NONE.to_owned(),
        Some(serde_json::Value::String(text)) => sanitize_display_text(text),
        Some(other) => sanitize_display_text(&other.to_string()),
    }
}

/// The host, marked when it would come from the real sender's address.
fn host_cell(host: String, depends_on_sender: bool) -> String {
    match (depends_on_sender, host.as_str()) {
        (false, _) => host,
        (true, NONE) => "(sender)".to_owned(),
        (true, _) => format!("{host} (sender)"),
    }
}

/// `_time` with its source: the value and the field it came from, or
/// `arrival` when the arrival time (in the header) filled it.
fn time_cell(value: Option<&serde_json::Value>, source: &TimeLineage) -> String {
    match source {
        TimeLineage::Field { field } => {
            format!("{} ({})", slot(value), sanitize_display_text(field))
        }
        TimeLineage::Arrival { unparseable: None } => "arrival".to_owned(),
        TimeLineage::Arrival {
            unparseable: Some(field),
        } => format!("arrival (bad {})", sanitize_display_text(field)),
    }
}

/// `_severity` with its source: the level and the field that fed it, any
/// earlier unmappable source it skipped, or why it is absent.
fn severity_cell(value: Option<&serde_json::Value>, source: &SeverityLineage) -> String {
    match source {
        SeverityLineage::Field {
            field,
            skipped_unmappable,
        } => {
            let level = value
                .and_then(serde_json::Value::as_i64)
                .and_then(trawl_core::severity::token_text)
                .map_or_else(|| slot(value), str::to_owned);
            let mut from = sanitize_display_text(field);
            if !skipped_unmappable.is_empty() {
                from.push_str(", skipped ");
                from.push_str(&sanitize_display_text(&skipped_unmappable.join(", ")));
            }
            format!("{level} ({from})")
        }
        SeverityLineage::Missing => "missing".to_owned(),
        SeverityLineage::Unmapped { sources } => {
            format!("unmapped ({})", sanitize_display_text(&sources.join(", ")))
        }
    }
}

/// One field change, compactly.
fn change_text(change: &FieldChangeWire) -> String {
    let field = sanitize_display_text(&change.field);
    match change.change {
        FieldChangeKind::Renamed => format!(
            "{field} → {}",
            sanitize_display_text(change.to.as_deref().unwrap_or("?"))
        ),
        FieldChangeKind::Dropped => format!("{field} dropped"),
        FieldChangeKind::Truncated => format!("{field} truncated"),
        FieldChangeKind::Stringified => format!("{field} stringified"),
    }
}

/// A list as one line per item, or [`NONE`] when it is empty.
fn list_cell(items: impl Iterator<Item = String>) -> String {
    let lines: Vec<String> = items.collect();
    if lines.is_empty() {
        NONE.to_owned()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use trawl_client::{
        ErrorCode, ErrorEnvelope, PreviewDerivation, PreviewLineage, PreviewPeer,
        SeveritySourceSpec,
    };

    use super::*;

    fn accepted() -> PreviewEvent {
        PreviewEvent::Accepted {
            index: 0,
            input: json!({"service": "api", "Level": "warn", "ts": "2026-10-01T00:00:00Z"}),
            event: json!({
                "env": "default",
                "service": "api",
                "host": "10.0.0.7",
                "_time": "2026-10-01T00:00:00.000000Z",
                "_severity": 13,
            })
            .as_object()
            .unwrap()
            .clone(),
            repairs: vec!["env.defaulted".into(), "host.from_peer".into()],
            lineage: PreviewLineage {
                time: TimeLineage::Field { field: "ts".into() },
                severity: SeverityLineage::Field {
                    field: "level".into(),
                    skipped_unmappable: vec!["severity".into()],
                },
                fields: vec![
                    FieldChangeWire {
                        field: "Level".into(),
                        change: FieldChangeKind::Renamed,
                        to: Some("level".into()),
                        code: Some("field.name_case_folded".into()),
                    },
                    FieldChangeWire {
                        field: "ctx".into(),
                        change: FieldChangeKind::Stringified,
                        to: None,
                        code: None,
                    },
                ],
            },
            host_depends_on_sender: true,
        }
    }

    fn rejected() -> PreviewEvent {
        PreviewEvent::Rejected {
            index: 2,
            input: None,
            reason: "invalid_json".into(),
            message: "invalid JSON".into(),
            host_depends_on_sender: false,
        }
    }

    fn report(events: Vec<PreviewEvent>, given: bool) -> PreviewResponse {
        let accepted = events
            .iter()
            .filter(|e| matches!(e, PreviewEvent::Accepted { .. }))
            .count();
        PreviewResponse {
            producer: "http".into(),
            peer: PreviewPeer {
                ip: if given { "10.0.0.7" } else { "192.0.2.1" }.into(),
                given,
                trusted_relay: false,
            },
            arrival: "2026-10-01T12:00:00.000000Z".into(),
            derivation: PreviewDerivation {
                time_from: vec!["_time".into(), "ts".into()],
                severity_from: vec![SeveritySourceSpec {
                    field: "level".into(),
                    dialect: "otel".into(),
                }],
            },
            accepted,
            rejected: events.len() - accepted,
            events,
        }
    }

    fn server_error(status: u16) -> Result<u8, CliError> {
        Err(CliError::Client(ClientError::Server {
            status,
            error: ErrorEnvelope::simple(ErrorCode::Forbidden, "insufficient permissions"),
        }))
    }

    fn status_and_stderr(result: Result<u8, CliError>) -> (u8, String) {
        let mut err = Vec::new();
        let status = exit_status(result, &mut err);
        (status, String::from_utf8(err).unwrap())
    }

    #[test]
    fn a_complete_report_keeps_its_status_and_every_failure_is_2() {
        assert_eq!(status_and_stderr(Ok(ALL_ACCEPTED)), (0, String::new()));
        assert_eq!(status_and_stderr(Ok(SOME_REJECTED)), (1, String::new()));

        let (status, stderr) = status_and_stderr(server_error(404));
        assert_eq!(status, NO_REPORT);
        assert_eq!(stderr, format!("trawl: {DOES_NOT_INGEST}\n"));

        let (status, stderr) = status_and_stderr(server_error(403));
        assert_eq!(status, NO_REPORT);
        assert!(stderr.contains("HTTP 403"), "{stderr}");

        let (status, stderr) =
            status_and_stderr(Err(CliError::Usage("cannot read nope: gone".into())));
        assert_eq!(
            (status, stderr.as_str()),
            (2, "trawl: cannot read nope: gone\n")
        );

        let broken = io::Error::new(io::ErrorKind::BrokenPipe, "closed");
        assert_eq!(
            status_and_stderr(Err(CliError::Io(broken))),
            (NO_REPORT, String::new())
        );
    }

    /// A server's error message is the server's text, so it reaches the
    /// terminal only after sanitisation: no OSC 52 clipboard write, no
    /// colour, no line forged under it.
    #[test]
    fn server_error_text_is_sanitised() {
        let hostile = Err(CliError::Client(ClientError::Server {
            status: 403,
            error: ErrorEnvelope::simple(
                ErrorCode::Forbidden,
                "denied\u{1b}]52;c;cHduZWQ=\u{7}\u{1b}[31mred\u{9b}2J\r\ntrawl: forged\u{202e}",
            ),
        }));
        let (status, stderr) = status_and_stderr(hostile);
        assert_eq!(status, NO_REPORT);
        let line = stderr.strip_suffix('\n').expect("one line");
        assert!(
            !line
                .chars()
                .any(trawl_core::sanitize::is_unsafe_display_char),
            "{stderr:?}"
        );
        assert!(
            line.starts_with("trawl: server error (HTTP 403): denied"),
            "{stderr:?}"
        );
    }

    /// Counts that disagree with the listed events are a broken answer,
    /// never a verdict.
    #[test]
    fn an_incomplete_report_is_refused() {
        check_complete(&report(vec![accepted(), rejected()], false)).expect("complete");

        let mut short = report(vec![accepted(), rejected()], false);
        short.events.pop();
        let err = check_complete(&short).expect_err("one event missing");
        assert!(
            matches!(&err, CliError::Client(ClientError::Parse(m)) if m.contains("incomplete")),
            "{err:?}"
        );
        assert_eq!(status_and_stderr(Err(err)).0, NO_REPORT);

        let mut miscounted = report(vec![accepted()], false);
        miscounted.accepted = 0;
        miscounted.rejected = 1;
        assert!(check_complete(&miscounted).is_err());
    }

    #[test]
    fn an_accepted_row_shows_values_with_their_sources() {
        assert_eq!(
            row(&accepted()),
            vec![
                "0",
                "accepted",
                "default",
                "api",
                "10.0.0.7 (sender)",
                "2026-10-01T00:00:00.000000Z (ts)",
                "warn (level, skipped severity)",
                "env.defaulted\nhost.from_peer",
                "Level → level\nctx stringified",
            ]
        );
    }

    #[test]
    fn a_rejected_row_shows_the_reason_and_no_event() {
        assert_eq!(
            row(&rejected()),
            vec![
                "2",
                "rejected",
                "-",
                "-",
                "-",
                "-",
                "-",
                "-",
                "invalid_json: invalid JSON"
            ]
        );
        let PreviewEvent::Rejected {
            index,
            input,
            reason,
            message,
            ..
        } = rejected()
        else {
            unreachable!()
        };
        let dependent = PreviewEvent::Rejected {
            index,
            input,
            reason,
            message,
            host_depends_on_sender: true,
        };
        assert_eq!(row(&dependent)[4], "(sender)");
    }

    #[test]
    fn time_and_severity_name_arrival_and_absence() {
        assert_eq!(
            time_cell(None, &TimeLineage::Arrival { unparseable: None }),
            "arrival"
        );
        assert_eq!(
            time_cell(
                None,
                &TimeLineage::Arrival {
                    unparseable: Some("ts".into())
                }
            ),
            "arrival (bad ts)"
        );
        assert_eq!(severity_cell(None, &SeverityLineage::Missing), "missing");
        assert_eq!(
            severity_cell(
                None,
                &SeverityLineage::Unmapped {
                    sources: vec!["level".into(), "severity".into()]
                }
            ),
            "unmapped (level, severity)"
        );
        // A level with no OTel token shows its number.
        assert_eq!(
            severity_cell(
                Some(&json!(99)),
                &SeverityLineage::Field {
                    field: "level".into(),
                    skipped_unmappable: vec![]
                }
            ),
            "99 (level)"
        );
    }

    #[test]
    fn the_header_says_when_no_peer_was_given() {
        let placeholder = header(&report(vec![], false));
        assert_eq!(
            placeholder,
            "peer 192.0.2.1 (placeholder, no --peer-ip given; an event without a host \
             depends on the sender), not a trusted relay; arrival 2026-10-01T12:00:00.000000Z"
        );
        let given = header(&report(vec![], true));
        assert!(given.starts_with("peer 10.0.0.7 (given), "), "{given}");
    }

    #[test]
    fn the_footer_counts_outcomes_and_sender_dependence() {
        assert_eq!(
            footer(&report(vec![accepted(), rejected()], false)),
            "2 event(s): 1 accepted, 1 rejected; 1 depend on the sender's address (pass --peer-ip)"
        );
        assert_eq!(
            footer(&report(vec![rejected()], true)),
            "1 event(s): 0 accepted, 1 rejected"
        );
    }

    /// Sample text reaches a terminal only after sanitisation.
    #[test]
    fn sample_text_is_sanitised() {
        let PreviewEvent::Accepted {
            index,
            input,
            mut event,
            repairs,
            lineage,
            ..
        } = accepted()
        else {
            unreachable!()
        };
        event.insert("service".into(), json!("api\u{1b}[31mred"));
        let hostile = PreviewEvent::Accepted {
            index,
            input,
            event,
            repairs,
            lineage,
            host_depends_on_sender: false,
        };
        let cells = row(&hostile);
        assert!(!cells[3].contains('\u{1b}'), "{:?}", cells[3]);
    }
}
