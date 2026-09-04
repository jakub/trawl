// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl schema` subcommands: the field catalog's read commands plus repin.
//!
//! `fields`/`field`/`conflicts` hit the server's catalog routes and render
//! through the generic driver formatter ([`crate::cli::render_driver_results`]).
//! `fields --data <glob>` is the embedded path: a plain `DESCRIBE` over local
//! parquet with no server and no postgres.

use std::io::{self, IsTerminal, Write};

use serde_json::Value as Json;
use trawl_client::{CatalogConflictsResponse, CatalogFieldResponse, CatalogFieldsResponse};

use crate::CliError;
use crate::cli::{ConnectionParams, OutputFormat, render_driver_results};

/// Parse a `--last` window like `30m`, `2h`, `7d`, `1w` into seconds.
pub fn parse_last(input: &str) -> Result<u64, CliError> {
    parse_window(input, "--last")
}

/// The `--last` grammar with the failing option named by the caller, so
/// `gc-pins --older-than 30y` complains about `--older-than`, not `--last`.
pub fn parse_window(input: &str, option: &str) -> Result<u64, CliError> {
    let input = input.trim();
    // Split off the last char, not the last byte: `split_at` panics off a
    // char boundary, and the unit position is exactly where a multi-byte
    // char lands (`--last 7µ` must be a usage error, not a crash).
    let (num, unit) = match input.char_indices().next_back() {
        Some((idx, _)) => input.split_at(idx),
        None => ("", ""),
    };
    let n: u64 = num
        .parse()
        .map_err(|_| CliError::Usage(format!("invalid {option} window: {input:?}")))?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => {
            return Err(CliError::Usage(format!(
                "invalid {option} window: {input:?} (units: s, m, h, d, w)"
            )));
        }
    };
    Ok(n.saturating_mul(mult))
}

/// Resolve the output format like `trawl query` does: table on a TTY,
/// ndjson on a pipe.
fn resolve_format(format: Option<OutputFormat>) -> Result<OutputFormat, CliError> {
    let format = format.unwrap_or_else(|| {
        if io::stdout().is_terminal() {
            OutputFormat::Table
        } else {
            OutputFormat::Json
        }
    });
    if format == OutputFormat::Parquet {
        return Err(CliError::Usage(
            "parquet output is not supported for schema commands".into(),
        ));
    }
    Ok(format)
}

/// `CatalogFieldsResponse` → generic (columns, rows).
pub fn fields_to_rows(resp: &CatalogFieldsResponse) -> (Vec<String>, Vec<Vec<Json>>) {
    let columns = [
        "field",
        "type",
        "pinned_from",
        "services",
        "rows",
        "first_seen",
        "last_seen",
        "conflicts",
        "rows_nulled",
        "degraded",
    ]
    .map(str::to_owned)
    .to_vec();
    let rows = resp
        .fields
        .iter()
        .map(|f| {
            vec![
                Json::from(f.name.clone()),
                Json::from(f.data_type.clone()),
                f.pinned_from.clone().map_or(Json::Null, Json::from),
                Json::from(f.service_count),
                Json::from(f.row_count),
                f.first_seen.clone().map_or(Json::Null, Json::from),
                f.last_seen.clone().map_or(Json::Null, Json::from),
                Json::from(f.conflict_count),
                Json::from(f.rows_nulled),
                Json::from(f.verdict.is_some()),
            ]
        })
        .collect();
    (columns, rows)
}

/// `CatalogConflictsResponse` → generic (columns, rows).
pub fn conflicts_to_rows(resp: &CatalogConflictsResponse) -> (Vec<String>, Vec<Vec<Json>>) {
    let columns = [
        "at",
        "field",
        "service",
        "observed_type",
        "expected_type",
        "rows_nulled",
    ]
    .map(str::to_owned)
    .to_vec();
    let rows = resp
        .conflicts
        .iter()
        .map(|c| {
            vec![
                Json::from(c.at.clone()),
                Json::from(c.field.clone()),
                Json::from(c.service.clone()),
                Json::from(c.observed_type.clone()),
                Json::from(c.expected_type.clone()),
                Json::from(c.rows_nulled),
            ]
        })
        .collect();
    (columns, rows)
}

/// The field detail's per-service observations → generic (columns, rows).
pub fn field_services_to_rows(resp: &CatalogFieldResponse) -> (Vec<String>, Vec<Vec<Json>>) {
    let columns = ["service", "first_seen", "last_seen", "rows"]
        .map(str::to_owned)
        .to_vec();
    let rows = resp
        .services
        .iter()
        .map(|s| {
            vec![
                Json::from(s.service.clone()),
                Json::from(s.first_seen.clone()),
                Json::from(s.last_seen.clone()),
                Json::from(s.row_count),
            ]
        })
        .collect();
    (columns, rows)
}

/// The field detail's conflict evidence → generic (columns, rows).
pub fn field_conflicts_to_rows(resp: &CatalogFieldResponse) -> (Vec<String>, Vec<Vec<Json>>) {
    let columns = [
        "at",
        "service",
        "observed_type",
        "expected_type",
        "rows_nulled",
    ]
    .map(str::to_owned)
    .to_vec();
    let rows = resp
        .conflicts
        .iter()
        .map(|c| {
            vec![
                Json::from(c.at.clone()),
                Json::from(c.service.clone()),
                Json::from(c.observed_type.clone()),
                Json::from(c.expected_type.clone()),
                Json::from(c.rows_nulled),
            ]
        })
        .collect();
    (columns, rows)
}

/// Embedded `fields --data`: DESCRIBE local parquet → (columns, rows).
///
/// Name + type only — catalog metadata (observations, conflicts, pin
/// provenance) lives on the server.
pub fn describe_data_to_rows(glob: &str) -> Result<(Vec<String>, Vec<Vec<Json>>), CliError> {
    let executor = trawl_engine::executor::Executor::new()?;
    let schema = executor.describe_schema(glob)?;
    let columns = ["field", "type"].map(str::to_owned).to_vec();
    let rows = schema
        .columns
        .into_iter()
        .map(|c| vec![Json::from(c.name), Json::from(c.data_type)])
        .collect();
    Ok((columns, rows))
}

fn make_client(conn: &ConnectionParams) -> Result<trawl_client::HttpClient, CliError> {
    let client = if conn.insecure {
        trawl_client::HttpClient::new_insecure(&conn.url, &conn.token)?
    } else {
        trawl_client::HttpClient::new(&conn.url, &conn.token)?
    };
    Ok(client)
}

/// Write a human preamble/section line for `run_field`. The labels exist
/// for the table view; on a machine format (auto-selected on a pipe) they
/// would interleave with the ndjson/CSV stream, so they go to stderr —
/// like `run_fields`' pin summary — and stdout stays parseable.
fn label<W: Write>(out: &mut W, human: bool, text: &str) -> Result<(), CliError> {
    if human {
        writeln!(out, "{text}")?;
    } else {
        eprintln!("{text}");
    }
    Ok(())
}

fn render<W: Write>(
    out: &mut W,
    columns: &[String],
    rows: &[Vec<Json>],
    format: OutputFormat,
) -> Result<(), CliError> {
    render_driver_results(columns, rows, format, out)?;
    out.flush()?;
    Ok(())
}

/// `trawl schema fields [--service] [--last] [--limit] [--data]`.
pub async fn run_fields<W: Write>(
    out: &mut W,
    conn: Option<ConnectionParams>,
    data: Option<&str>,
    service: Option<&str>,
    last: Option<&str>,
    limit: Option<usize>,
    format: Option<OutputFormat>,
) -> Result<(), CliError> {
    let format = resolve_format(format)?;

    // Embedded mode: DESCRIBE over local parquet, no server needed.
    if let Some(glob) = data {
        let (columns, rows) = describe_data_to_rows(glob)?;
        eprintln!(
            "note: --data lists names and physical types only; catalog metadata \
             (pins, observations, conflicts) requires a server"
        );
        return render(out, &columns, &rows, format);
    }

    let conn = conn.ok_or_else(|| {
        CliError::Usage("provide --url (daemon mode) or --data (embedded mode)".into())
    })?;
    let since_secs = last.map(parse_last).transpose()?;
    let client = make_client(&conn)?;
    let resp = client.catalog_fields(service, since_secs, limit).await?;

    let (columns, rows) = fields_to_rows(&resp);
    render(out, &columns, &rows, format)?;
    let degraded = resp.fields.iter().filter(|f| f.verdict.is_some()).count();
    eprintln!(
        "{}/{} pins used{}{}",
        resp.pinned_total,
        resp.pin_capacity,
        if degraded > 0 {
            format!(", {degraded} degraded (see: trawl schema field <name>)")
        } else {
            String::new()
        },
        if resp.truncated {
            " (listing truncated — raise --limit)"
        } else {
            ""
        }
    );
    Ok(())
}

/// `trawl schema field <name> [--limit] [--after]`.
pub async fn run_field<W: Write>(
    out: &mut W,
    conn: Option<ConnectionParams>,
    name: &str,
    limit: Option<usize>,
    after: Option<&str>,
    format: Option<OutputFormat>,
) -> Result<(), CliError> {
    let format = resolve_format(format)?;
    let conn = conn.ok_or_else(|| CliError::Usage("schema field requires a server".into()))?;
    let client = make_client(&conn)?;
    let resp = client.catalog_field(name, limit, after).await?;

    let human = format == OutputFormat::Table;
    label(out, human, &format!("field:       {}", resp.name))?;
    label(out, human, &format!("type:        {}", resp.data_type))?;
    label(
        out,
        human,
        &format!(
            "pinned from: {}",
            resp.pinned_from.as_deref().unwrap_or("(unknown)")
        ),
    )?;
    label(out, human, &format!("pinned at:   {}", resp.pinned_at))?;

    if let Some(v) = &resp.verdict {
        render_verdict(out, human, &resp.name, v)?;
    }

    // A standing ack renders beside the verdict either way: suppressed
    // means "acked and quiet", a verdict above it means new evidence
    // re-raised the badge past the acknowledged high-water.
    if let Some(ack) = &resp.ack {
        label(
            out,
            human,
            &format!(
                "\nacknowledged: through episode {} by {} at {}{}",
                ack.evidence_through,
                trawl_core::sanitize::sanitize_display_text(&ack.acked_by),
                ack.acked_at,
                match &ack.note {
                    Some(n) => format!(" ({})", trawl_core::sanitize::sanitize_display_text(n)),
                    None => String::new(),
                }
            ),
        )?;
    }

    label(out, human, "\nservices:")?;
    let (columns, rows) = field_services_to_rows(&resp);
    render(out, &columns, &rows, format)?;
    if let Some(cursor) = &resp.services_cursor {
        eprintln!("(more services — rerun with --after {cursor})");
    }

    if !resp.conflicts.is_empty() {
        label(out, human, "\nrecent conflicts:")?;
        let (columns, rows) = field_conflicts_to_rows(&resp);
        render(out, &columns, &rows, format)?;
    }
    Ok(())
}

/// The case file for a degraded pin: the analyzer's facts, the values the
/// pin is shelving, and the one command that fixes it.
///
/// The wire carries structured facts, never stored prose (ADR-0011), so the
/// words are written here. "rows shelved" is the lifetime total from the
/// durable aggregates, a different number from the `rows_nulled` column in
/// the conflict table below it, which sums only the evidence rows still
/// inside the per-field recency window, so the two are labelled apart on
/// purpose.
fn render_verdict<W: Write>(
    out: &mut W,
    human: bool,
    field: &str,
    v: &trawl_client::DegradedVerdict,
) -> Result<(), CliError> {
    // A field name is a client-chosen JSON key that ingest polices for
    // length and case only: a name carrying `;` and a shell command, or a
    // bidi override, is legal, and the last line here is written to be
    // pasted into a shell. Every rendering of the name is neutralised.
    let shown = trawl_core::sanitize::sanitize_display_text(field);
    label(out, human, "\ndegraded pin:")?;
    label(out, human, &format!("  since:          {}", v.since))?;
    label(out, human, &format!("  senders:        {}", v.services))?;
    label(out, human, &format!("  episodes:       {}", v.episodes))?;
    label(
        out,
        human,
        &format!("  rows shelved:   {} (lifetime)", v.rows_shelved),
    )?;
    if !v.samples.is_empty() {
        label(out, human, "  sample values:")?;
        for sample in &v.samples {
            label(out, human, &format!("    - {sample}"))?;
        }
    }
    label(out, human, &format!("  suggested:      {}", v.suggested_to))?;

    // The remedy line is printed only for a name a command line can carry
    // as itself. Two ways it cannot, both reachable because ingest polices
    // field names for length and case and nothing else:
    //
    // - it does not survive the neutralisation above, so the sanitised
    //   spelling is a different string; pasted, it would repin some other
    //   field, or nothing, or (with enough bad luck) a real field whose name
    //   genuinely contains U+FFFD;
    // - it starts with `-`, so the argument parser reads it as a flag
    //   however it is quoted (executed: `unexpected argument '-x' found`).
    //
    // A `--` terminator would answer the second, but its interaction with
    // the flags that follow is untested, and an offered command that does
    // not work is worse than none.
    if shown == field && !field.starts_with('-') {
        label(
            out,
            human,
            &format!(
                "  trawl schema repin {} --to {} --dry-run",
                shell_quote(&shown),
                v.suggested_to.to_ascii_lowercase()
            ),
        )
    } else {
        label(
            out,
            human,
            "  this field's name cannot be safely embedded in a command line \
             (it does not survive display sanitisation, or it begins with `-` \
             and would be read as a flag), so no repin command is shown — take \
             the exact name from `trawl schema field <name> -f json` (or GET \
             /api/v1/schema/field) before running `trawl schema repin`",
        )
    }
}

/// `arg` as one POSIX shell word.
///
/// Bare when the name is already a shell-inert token — which every ordinary
/// field name is, and the common case must stay copy-pasteable-looking —
/// otherwise single-quoted, with embedded quotes closed and reopened
/// (`'\''`), the one escape that works inside single quotes. Single quotes
/// rather than double because nothing inside them expands: no `$`, no
/// backtick, no backslash.
fn shell_quote(arg: &str) -> String {
    let bare = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if bare {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// `trawl schema conflicts [--field] [--service] [--last] [--limit]`.
pub async fn run_conflicts<W: Write>(
    out: &mut W,
    conn: Option<ConnectionParams>,
    field: Option<&str>,
    service: Option<&str>,
    last: Option<&str>,
    limit: Option<usize>,
    format: Option<OutputFormat>,
) -> Result<(), CliError> {
    let format = resolve_format(format)?;
    let conn = conn.ok_or_else(|| CliError::Usage("schema conflicts requires a server".into()))?;
    let since_secs = last.map(parse_last).transpose()?;
    let client = make_client(&conn)?;
    let resp = client
        .catalog_conflicts(field, service, since_secs, limit)
        .await?;

    let (columns, rows) = conflicts_to_rows(&resp);
    render(out, &columns, &rows, format)?;
    if resp.truncated {
        eprintln!("(listing truncated — raise --limit)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use trawl_client::{CatalogConflictRow, CatalogFieldServiceRow, CatalogFieldSummary};

    fn sample_fields() -> CatalogFieldsResponse {
        CatalogFieldsResponse {
            fields: vec![CatalogFieldSummary {
                name: "duration".into(),
                data_type: "BIGINT".into(),
                pinned_from: Some("nginx".into()),
                pinned_at: "2026-08-01T10:00:00Z".into(),
                service_count: 2,
                row_count: 5,
                first_seen: Some("2026-08-01T10:00:00Z".into()),
                last_seen: Some("2026-08-02T10:00:00Z".into()),
                conflict_count: 1,
                rows_nulled: 3,
                verdict: None,
            }],
            pinned_total: 11,
            pin_capacity: 10_000,
            truncated: false,
        }
    }

    fn sample_verdict() -> trawl_client::DegradedVerdict {
        trawl_client::DegradedVerdict {
            since: "2026-08-01T10:00:00Z".into(),
            services: 2,
            episodes: 7,
            rows_shelved: 240,
            samples: vec!["n/a".into(), "pending".into()],
            suggested_to: "VARCHAR".into(),
        }
    }

    fn sample_conflicts() -> CatalogConflictsResponse {
        CatalogConflictsResponse {
            conflicts: vec![CatalogConflictRow {
                field: "duration".into(),
                service: "envoy".into(),
                observed_type: "VARCHAR".into(),
                expected_type: "BIGINT".into(),
                rows_nulled: 3,
                samples: vec!["n/a".into()],
                at: "2026-08-02T11:00:00Z".into(),
            }],
            truncated: true,
        }
    }

    #[test]
    fn a_window_error_names_the_option_that_failed() {
        let err = parse_window("30y", "--older-than").unwrap_err().to_string();
        assert!(err.contains("--older-than"), "{err}");
        assert!(!err.contains("--last"), "{err}");
        let err = parse_last("30y").unwrap_err().to_string();
        assert!(err.contains("--last"), "{err}");
    }

    #[test]
    fn parse_last_understands_dsl_units() {
        assert_eq!(parse_last("30s").unwrap(), 30);
        assert_eq!(parse_last("30m").unwrap(), 1800);
        assert_eq!(parse_last("2h").unwrap(), 7200);
        assert_eq!(parse_last("7d").unwrap(), 604_800);
        assert_eq!(parse_last("1w").unwrap(), 604_800);
        assert!(parse_last("7").is_err());
        assert!(parse_last("d7").is_err());
        assert!(parse_last("").is_err());
        assert!(parse_last("7y").is_err());
        // Multi-byte trailing chars are usage errors, not char-boundary
        // panics out of `split_at`.
        assert!(parse_last("7µ").is_err());
        assert!(parse_last("µ").is_err());
        assert!(parse_last("7é").is_err());
    }

    #[test]
    fn fields_converter_maps_every_aggregate() {
        let (columns, rows) = fields_to_rows(&sample_fields());
        assert_eq!(columns[0], "field");
        assert_eq!(columns[1], "type");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Json::from("duration"));
        assert_eq!(rows[0][1], Json::from("BIGINT"));
        assert_eq!(rows[0][3], Json::from(2u64), "service_count");
        assert_eq!(rows[0][4], Json::from(5u64), "row_count");
        assert_eq!(rows[0][7], Json::from(1u64), "conflict_count");
        assert_eq!(rows[0][8], Json::from(3u64), "rows_nulled");
    }

    #[test]
    fn conflicts_converter_orders_columns() {
        let (columns, rows) = conflicts_to_rows(&sample_conflicts());
        assert_eq!(
            columns,
            vec![
                "at",
                "field",
                "service",
                "observed_type",
                "expected_type",
                "rows_nulled"
            ]
        );
        assert_eq!(rows[0][1], Json::from("duration"));
        assert_eq!(rows[0][3], Json::from("VARCHAR"));
        assert_eq!(rows[0][5], Json::from(3u64));
    }

    /// The degraded column is the listing's badge, and the summary counts
    /// what it badged.
    #[test]
    fn fields_to_rows_carries_the_degraded_flag() {
        let mut resp = sample_fields();
        let (cols, rows) = fields_to_rows(&resp);
        assert_eq!(cols.last().unwrap(), "degraded");
        assert_eq!(rows[0].last().unwrap(), &Json::from(false));

        resp.fields[0].verdict = Some(sample_verdict());
        let (_, rows) = fields_to_rows(&resp);
        assert_eq!(rows[0].last().unwrap(), &Json::from(true));
    }

    /// The case file states the facts, shows the values, and ends with the
    /// exact command that fixes it — with the lifetime total labelled apart
    /// from the windowed `rows_nulled` in the conflict table below it.
    #[test]
    fn the_case_file_renders_facts_samples_and_the_remedy() {
        let mut buf = Vec::new();
        render_verdict(&mut buf, true, "duration", &sample_verdict()).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("degraded pin:"), "{text}");
        assert!(
            text.contains("since:          2026-08-01T10:00:00Z"),
            "{text}"
        );
        assert!(text.contains("senders:        2"), "{text}");
        assert!(text.contains("episodes:       7"), "{text}");
        assert!(text.contains("rows shelved:   240 (lifetime)"), "{text}");
        assert!(text.contains("    - n/a"), "{text}");
        assert!(text.contains("suggested:      VARCHAR"), "{text}");
        assert!(
            text.contains("trawl schema repin duration --to varchar --dry-run"),
            "the remedy is copy-pasteable: {text}"
        );
    }

    /// A field name is client-chosen text that ingest polices for length and
    /// case only, and the remedy line is written to be pasted into a shell.
    /// A name that survives sanitisation unchanged is quoted and printed.
    #[test]
    fn the_remedy_line_quotes_a_hostile_field_name() {
        let mut buf = Vec::new();
        render_verdict(&mut buf, true, "x; touch pwned", &sample_verdict()).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("trawl schema repin 'x; touch pwned' --to varchar --dry-run"),
            "a command-injecting name is one shell word: {text}"
        );

        let mut buf = Vec::new();
        render_verdict(&mut buf, true, "it's ok", &sample_verdict()).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("repin 'it'\\''s ok' --to varchar"),
            "an embedded quote closes and reopens: {text}"
        );
    }

    /// A name the sanitiser changes cannot be named by a command: the
    /// printed spelling is a different string, and repinning the wrong
    /// field rewrites the wrong corpus. The verdict facts still render.
    #[test]
    fn a_name_that_cannot_be_printed_gets_no_command() {
        let mut buf = Vec::new();
        render_verdict(&mut buf, true, "bad\u{202e}name", &sample_verdict()).unwrap();
        let text = String::from_utf8(buf).unwrap();

        assert!(text.contains("rows shelved:   240 (lifetime)"), "{text}");
        assert!(
            !text.contains("trawl schema repin bad"),
            "no command may name the sanitised spelling: {text}"
        );
        assert!(
            !text.contains("--dry-run"),
            "no runnable command at all: {text}"
        );
        assert!(
            text.contains("cannot be safely embedded in a command line"),
            "the operator is told why, and where to get the real name: {text}"
        );
        assert!(
            !text.contains('\u{202e}'),
            "and the override still never reaches the terminal: {text}"
        );
    }

    /// A dash-leading name is printable and shell-quotable but still cannot
    /// be a command argument: the parser reads `'-x'` as a flag, quotes and
    /// all (`unexpected argument '-x' found`). It takes the same note
    /// branch, rather than an offered command that does not run.
    #[test]
    fn a_flag_shaped_field_name_gets_no_command() {
        for name in ["-x", "--to"] {
            let mut buf = Vec::new();
            render_verdict(&mut buf, true, name, &sample_verdict()).unwrap();
            let text = String::from_utf8(buf).unwrap();

            assert!(text.contains("episodes:       7"), "{name}: {text}");
            assert!(
                !text.contains("--dry-run"),
                "{name}: a command clap would reject must not be offered: {text}"
            );
            assert!(
                text.contains("cannot be safely embedded in a command line"),
                "{name}: {text}"
            );
        }
    }

    #[test]
    fn shell_quoting_leaves_ordinary_names_bare() {
        for name in ["duration", "http.status_code", "a-b_c", "9lives"] {
            assert_eq!(shell_quote(name), name);
        }
        assert_eq!(shell_quote(""), "''", "an empty word still needs quotes");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("$(id)"), "'$(id)'");
    }

    #[test]
    fn field_detail_converters_split_services_and_conflicts() {
        let resp = CatalogFieldResponse {
            name: "duration".into(),
            data_type: "BIGINT".into(),
            pinned_from: Some("nginx".into()),
            pinned_at: "2026-08-01T10:00:00Z".into(),
            services: vec![CatalogFieldServiceRow {
                service: "nginx".into(),
                first_seen: "2026-08-01T10:00:00Z".into(),
                last_seen: "2026-08-02T10:00:00Z".into(),
                row_count: 5,
            }],
            services_cursor: None,
            conflicts: sample_conflicts().conflicts,
            verdict: None,
            ack: None,
        };
        let (cols, rows) = field_services_to_rows(&resp);
        assert_eq!(cols, vec!["service", "first_seen", "last_seen", "rows"]);
        assert_eq!(rows[0][0], Json::from("nginx"));
        let (cols, rows) = field_conflicts_to_rows(&resp);
        assert_eq!(cols[1], "service");
        assert_eq!(rows[0][1], Json::from("envoy"));
    }

    /// Populated output: the seeded responses render non-empty tables
    /// through the shared driver formatter.
    #[test]
    fn seeded_responses_render_populated_output() {
        let (columns, rows) = fields_to_rows(&sample_fields());
        let mut out = Vec::new();
        render_driver_results(&columns, &rows, OutputFormat::Table, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("duration"), "{text}");
        assert!(text.contains("BIGINT"), "{text}");
        assert!(text.contains("1 row(s)"), "{text}");

        let (columns, rows) = conflicts_to_rows(&sample_conflicts());
        let mut out = Vec::new();
        render_driver_results(&columns, &rows, OutputFormat::Json, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let parsed: Json = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(parsed["expected_type"], "BIGINT");
        assert_eq!(parsed["rows_nulled"], 3);
    }

    /// Embedded mode: `fields --data` DESCRIBEs local parquet with no
    /// server and no postgres.
    #[test]
    fn describe_data_lists_local_parquet_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.parquet");
        let executor = trawl_engine::executor::Executor::new().unwrap();
        let result = trawl_engine::value::QueryResult {
            columns: vec![
                trawl_engine::value::Column {
                    name: "service".into(),
                },
                trawl_engine::value::Column {
                    name: "duration".into(),
                },
            ],
            rows: vec![vec![
                trawl_engine::value::Value::String("nginx".into()),
                trawl_engine::value::Value::Integer(42),
            ]],
        };
        executor
            .write_query_result_to_parquet(&result, &path)
            .unwrap();

        let glob = format!("{}/*.parquet", dir.path().display());
        let (columns, rows) = describe_data_to_rows(&glob).unwrap();
        assert_eq!(columns, vec!["field", "type"]);
        let names: Vec<&str> = rows.iter().map(|r| r[0].as_str().unwrap()).collect();
        assert!(names.contains(&"service"), "{names:?}");
        assert!(names.contains(&"duration"), "{names:?}");
        // Every column carries a non-empty physical type.
        assert!(rows.iter().all(|r| !r[1].as_str().unwrap().is_empty()));
    }
}

// -- repin --------------------------------------------------------------------

/// The `trawl schema repin` flag bundle.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)] // four independent CLI switches
pub struct RepinFlags {
    /// The asserted numeral dialect, for a `SEVERITY` target only.
    pub dialect: Option<crate::cli::SeverityDialect>,
    /// Scan and report only.
    pub dry_run: bool,
    /// Accept a lossy projection / run a resurrection-only pass.
    pub force: bool,
    /// Skip the interactive confirmation.
    pub yes: bool,
    /// Poll the job to completion.
    pub wait: bool,
    /// The most rows the forced rewrite may null, as stated on the command
    /// line. `None` leaves the number to the server's scan.
    pub max_nulled_rows: Option<u64>,
    /// The same bound for dialect-ambiguous numerals.
    pub max_ambiguous_rows: Option<u64>,
}

/// The ceilings a forced repin is held to, both resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundCeilings {
    /// Rows the rewrite may null.
    pub max_nulled: u64,
    /// Dialect-ambiguous numerals the rewrite may carry.
    pub max_ambiguous: u64,
}

/// How a run reaches the numbers its forced execution is bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CeilingPlan {
    /// Nothing to bind: an unforced repin accepts no loss at all, and a dry
    /// run mutates nothing, so neither has terms to state.
    NotApplicable,
    /// The command line stated both numbers. Execute with them as they
    /// stand, and print them: no preview scan is needed to know them.
    Stated(BoundCeilings),
    /// A forced execution that stated fewer than both numbers. The missing
    /// half only exists once a scan has counted the corpus, so the run does
    /// a forced dry run first, prints the ceilings that scan resolved, and
    /// then executes stating those same numbers.
    Preview,
}

/// Decide how a `schema repin` invocation reaches its ceilings, before any
/// network access.
///
/// The rule is that an operator never reads a number the execution is not
/// actually held to. A scan-derived default is resolved per job, and a
/// forced execution runs its own scan, so printing the preview's number and
/// then letting the execution derive a fresh one would print a number
/// nothing enforces. Restating the preview's numbers as explicit ceilings
/// makes the printed line the enforced bound by construction, at the cost of
/// a second scan on this path only.
#[must_use]
pub fn ceiling_plan(flags: &RepinFlags) -> CeilingPlan {
    if flags.dry_run || !flags.force {
        return CeilingPlan::NotApplicable;
    }
    match (flags.max_nulled_rows, flags.max_ambiguous_rows) {
        (Some(max_nulled), Some(max_ambiguous)) => CeilingPlan::Stated(BoundCeilings {
            max_nulled,
            max_ambiguous,
        }),
        _ => CeilingPlan::Preview,
    }
}

/// The ceilings a preview report resolved, when it resolved both.
///
/// A report missing either half is a job whose scan recorded no plan (or a
/// server older than the ceiling columns). There is nothing honest to print
/// or to restate there, so the caller falls back to whatever the flags said
/// and lets the execution resolve the rest.
#[must_use]
pub fn accepted_ceilings(job: &trawl_client::RepinJobResponse) -> Option<BoundCeilings> {
    Some(BoundCeilings {
        max_nulled: job.accepted_max_nulled_rows?,
        max_ambiguous: job.accepted_max_ambiguous_rows?,
    })
}

/// The sentence the CLI prints before a forced execution starts.
#[must_use]
fn ceiling_notice(bound: BoundCeilings, from_preview: bool) -> String {
    format!(
        "force accepts up to {} unreadable row(s) and {} dialect-ambiguous \
         numeral(s){}. The repin refuses at the cutover if the finished \
         rewrite is worse than that.",
        bound.max_nulled,
        bound.max_ambiguous,
        if from_preview {
            ", resolved from a preview scan"
        } else {
            ""
        }
    )
}

/// One repin job → generic key/value (columns, rows) for the driver
/// formatter — the job is a single record, so it renders as one row.
///
/// Every fact rides the row (dialect, ambiguity, samples, liveness, the
/// force verdict and its reason), because a scripted caller reading
/// `-f json` or `-f csv` must not have to parse the prose the case file
/// writes for a human. `requires_force` is null rather than `false` while
/// the scan has yet to record a plan: absent is "not known yet".
///
/// The samples are an array in JSON and a joined string everywhere else.
/// A spreadsheet's formula-injection rule fires on a cell's first
/// character, so a joined cell puts the first sample there where
/// `csv_escape_string`'s `'` prefix can neutralise it; inside a rendered
/// JSON array it would sit behind a `[` and be invisible to that rule.
pub fn repin_job_to_rows(
    job: &trawl_client::RepinJobResponse,
    format: OutputFormat,
) -> (Vec<String>, Vec<Vec<Json>>) {
    (repin_job_columns(), vec![repin_job_cells(job, format)])
}

/// The job row's column names. Shared with the cancel receipt, which
/// carries the same columns so a machine format is one record with one
/// header (#109).
fn repin_job_columns() -> Vec<String> {
    [
        "id",
        "field",
        "from",
        "to",
        "dialect",
        "status",
        "files_total",
        "files_done",
        "rows_carrying",
        "projected_nulls",
        "resurrectable",
        "ambiguous_numerals",
        "rows_rewritten",
        "rows_nulled",
        "rows_resurrected",
        "unmapped_samples",
        "field_last_seen",
        "field_last_service",
        "requires_force",
        "requires_force_reason",
        "cancel_requested_at",
        "cancelled_by",
        "max_nulled_rows",
        "max_ambiguous_rows",
        "accepted_max_nulled_rows",
        "accepted_max_ambiguous_rows",
        "error",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// One job's cells, in [`repin_job_columns`] order.
fn repin_job_cells(job: &trawl_client::RepinJobResponse, format: OutputFormat) -> Vec<Json> {
    vec![
        Json::from(job.id),
        Json::from(job.field.clone()),
        Json::from(job.from_type.clone()),
        Json::from(job.to_type.clone()),
        job.dialect.clone().map_or(Json::Null, Json::from),
        Json::from(job.status.clone()),
        Json::from(job.files_total),
        Json::from(job.files_done),
        Json::from(job.rows_carrying),
        Json::from(job.projected_nulls),
        Json::from(job.resurrectable),
        Json::from(job.ambiguous_numerals),
        Json::from(job.rows_rewritten),
        Json::from(job.rows_nulled),
        Json::from(job.rows_resurrected),
        if format == OutputFormat::Json {
            Json::Array(
                job.unmapped_samples
                    .iter()
                    .map(|s| Json::from(s.clone()))
                    .collect(),
            )
        } else if job.unmapped_samples.is_empty() {
            Json::Null
        } else {
            Json::from(job.unmapped_samples.join(", "))
        },
        job.liveness
            .as_ref()
            .map_or(Json::Null, |l| Json::from(l.last_seen.clone())),
        job.liveness
            .as_ref()
            .map_or(Json::Null, |l| Json::from(l.service.clone())),
        job.requires_force.map_or(Json::Null, Json::from),
        job.requires_force_reason
            .clone()
            .map_or(Json::Null, Json::from),
        // Persisted facts (#109): on a `running` row they say a cancel is
        // in flight, on a `failed` one they say the process died between
        // the request and any boundary that could observe it.
        job.cancel_requested_at
            .clone()
            .map_or(Json::Null, Json::from),
        job.cancelled_by.clone().map_or(Json::Null, Json::from),
        // The request's own numbers beside the ones the job is held to: a
        // stated ceiling and a resolved one differ whenever the operator
        // stated none, and a machine consumer that cannot see both cannot
        // tell a default apart from an instruction.
        job.max_nulled_rows.map_or(Json::Null, Json::from),
        job.max_ambiguous_rows.map_or(Json::Null, Json::from),
        job.accepted_max_nulled_rows.map_or(Json::Null, Json::from),
        job.accepted_max_ambiguous_rows
            .map_or(Json::Null, Json::from),
        job.error.clone().map_or(Json::Null, Json::from),
    ]
}

/// The repin report's case file: the evidence a plan's numbers cannot
/// carry. What the new pin cannot read, whether the corpus is
/// dialect-ambiguous, whether anything is still writing the field, and why
/// force is required.
///
/// Facts from the job row, phrased here (ADR-0011: the server ships facts,
/// the consumer writes the words). Every value is sender-chosen text and
/// goes through display sanitisation, because samples are attacker text by
/// definition and this lands in a terminal.
fn render_repin_case_file<W: Write>(
    out: &mut W,
    human: bool,
    job: &trawl_client::RepinJobResponse,
) -> Result<(), CliError> {
    if let Some(dialect) = &job.dialect {
        label(
            out,
            human,
            &format!(
                "\nnumeral dialect: {} (words are dialect-free; this reads NUMERALS)",
                trawl_core::sanitize::sanitize_display_text(dialect)
            ),
        )?;
    }
    if job.ambiguous_numerals > 0 {
        label(
            out,
            human,
            &format!(
                "  ambiguous numerals: {} row(s) carry 1-7, which OTel reads as \
                 trace/debug and syslog PRI reads as err/crit",
                job.ambiguous_numerals
            ),
        )?;
    }
    if !job.unmapped_samples.is_empty() {
        label(out, human, "\nvalues the new pin cannot read:")?;
        for sample in &job.unmapped_samples {
            label(
                out,
                human,
                &format!(
                    "  - {}",
                    trawl_core::sanitize::sanitize_display_text(sample)
                ),
            )?;
        }
        label(
            out,
            human,
            "  (originals stay findable in _raw, whatever this repin writes)",
        )?;
    }

    // A repin translates history only: a live sender keeps arriving in the
    // ingest-time reading, so a syslog rewrite leaves a discontinuity at the
    // cutover instant, and the fix for the live half is config, not another
    // repin (ADR-0013 ruling 10).
    if let Some(live) = &job.liveness {
        let service = trawl_core::sanitize::sanitize_display_text(&live.service);
        label(
            out,
            human,
            &format!(
                "\nwarning: {} is STILL being written (last seen {} by service \
                 {service}). A repin rewrites HISTORY: after the cutover, live \
                 events keep taking the INGEST-time reading, so under \
                 --dialect syslog a historical `3` becomes 17 (err) while the \
                 next live `3` conforms as OTel 3 (trace3) — one column, two \
                 meanings, split at the cutover instant. If that sender speaks \
                 syslog PRI, declare it in [ingest] severity_from (dialect = \
                 \"syslog\") so live events read the same way, then repin the \
                 history.",
                trawl_core::sanitize::sanitize_display_text(&job.field),
                live.last_seen
            ),
        )?;
    }

    // What this job is actually held to, once its scan has resolved the
    // numbers. An unforced job has none: it accepts no loss at all.
    if let Some(bound) = accepted_ceilings(job) {
        label(
            out,
            human,
            &format!(
                "\naccepted ceilings: {} unreadable row(s), {} dialect-ambiguous \
                 numeral(s)",
                bound.max_nulled, bound.max_ambiguous
            ),
        )?;
    }

    match (job.requires_force, &job.requires_force_reason) {
        // A job that already carried force was not refused for the want of
        // it: it was refused by the ceilings it accepted, and the remedy is
        // a higher number, not the flag it passed.
        (Some(true), Some(reason)) => label(
            out,
            human,
            &format!(
                "\n{}: {}",
                if job.force {
                    "over its accepted ceilings (raise --max-nulled-rows / \
                     --max-ambiguous-rows to accept more)"
                } else {
                    "requires --force"
                },
                trawl_core::sanitize::sanitize_display_text(reason)
            ),
        )?,
        // The scan has not measured the corpus yet, so there is no verdict
        // to report — saying "no force needed" here would be a promise the
        // finished scan may contradict.
        (None, _) => label(
            out,
            human,
            "\nforce verdict: not known yet (the scan has not recorded its plan)",
        )?,
        _ => {}
    }
    Ok(())
}

/// Resolve the ceilings a forced execution states, printing them first.
///
/// A forced run that did not state both numbers scans the corpus first, so
/// what gets printed is what the execution then carries. Deriving the
/// numbers twice would print one and enforce another: the corpus grows
/// between the two scans, and the default is a function of the scan.
async fn bind_ceilings(
    out: &mut impl Write,
    client: &trawl_client::HttpClient,
    field: &str,
    to: &str,
    dialect: Option<&str>,
    flags: RepinFlags,
    human: bool,
) -> Result<Option<BoundCeilings>, CliError> {
    let plan = ceiling_plan(&flags);
    let bound = match plan {
        CeilingPlan::NotApplicable => None,
        CeilingPlan::Stated(bound) => Some(bound),
        CeilingPlan::Preview => {
            let preview = client
                .schema_repin(
                    field,
                    to,
                    dialect,
                    true,
                    true,
                    trawl_client::RepinCeilings {
                        max_nulled_rows: flags.max_nulled_rows,
                        max_ambiguous_rows: flags.max_ambiguous_rows,
                    },
                )
                .await?;
            match preview {
                trawl_client::RepinStart::Report(job) => accepted_ceilings(&job),
                // A forced scan resolves its own ceilings, so a refusal here
                // is one no ceiling covers. Report it instead of executing:
                // the execution would refuse the same way, after a rewrite.
                // A cancelled or failed preview produced no plan either, and
                // its own status is what the sentence names (#109).
                trawl_client::RepinStart::Refused(job)
                | trawl_client::RepinStart::Started(job)
                | trawl_client::RepinStart::Cancelled(job)
                | trawl_client::RepinStart::Failed(job) => {
                    render_repin_case_file(out, human, &job)?;
                    return Err(CliError::Usage(format!(
                        "repin preview did not produce a plan (job {} is {})",
                        job.id, job.status
                    )));
                }
            }
        }
    };
    if let Some(bound) = bound {
        label(
            out,
            human,
            &ceiling_notice(bound, plan == CeilingPlan::Preview),
        )?;
    } else if plan == CeilingPlan::Preview {
        // The preview reported no resolved ceilings, so there is no number
        // to print or to restate. The execution derives its own, exactly as
        // a repin did before ceilings existed.
        label(
            out,
            human,
            "note: the server reported no resolved ceilings; the repin runs \
             under the ones its own scan derives",
        )?;
    }
    Ok(bound)
}

/// `trawl schema repin <field> --to <type>`.
///
/// An executing repin rewrites the archive, so it confirms interactively —
/// and off a TTY it refuses without `--yes` rather than assuming (a piped
/// or scripted invocation must state its intent). Dry runs never prompt.
///
/// A forced execution prints the ceilings it accepts before it starts. When
/// the command line stated both, those are the numbers; otherwise the run
/// takes a forced dry run first and restates the ceilings that scan
/// resolved, so the printed line is the bound the job is held to rather
/// than a default a second scan might land somewhere else.
pub async fn run_repin(
    out: &mut impl Write,
    conn: ConnectionParams,
    field: &str,
    to: &str,
    flags: RepinFlags,
    format: Option<OutputFormat>,
) -> Result<(), CliError> {
    let format = resolve_format(format)?;
    // Refused here as well as server-side, because this one is a typo an
    // operator can fix without spending a job claim: the dialect reads
    // numerals onto the severity ladder, so no other target has anywhere to
    // put it.
    if flags.dialect.is_some() && !to.eq_ignore_ascii_case("severity") {
        return Err(CliError::Usage(format!(
            "--dialect applies to --to severity only (got --to {to})"
        )));
    }
    if !flags.dry_run && !flags.yes {
        if io::stdin().is_terminal() && io::stdout().is_terminal() {
            eprint!(
                "repin {field:?} to {to}: this rewrites the standing corpus \
                 (dry-run first with --dry-run). Proceed? [y/N] "
            );
            io::stderr().flush().ok();
            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            if !matches!(answer.trim(), "y" | "Y" | "yes") {
                return Err(CliError::Usage("repin aborted".into()));
            }
        } else {
            return Err(CliError::Usage(
                "repin rewrites the standing corpus; non-interactive \
                 invocations must pass --yes (or use --dry-run)"
                    .into(),
            ));
        }
    }

    let client = make_client(&conn)?;
    let dialect = flags.dialect.map(crate::cli::SeverityDialect::token);
    let human = format == OutputFormat::Table;

    let bound = bind_ceilings(out, &client, field, to, dialect, flags, human).await?;

    let outcome = client
        .schema_repin(
            field,
            to,
            dialect,
            flags.dry_run,
            flags.force,
            trawl_client::RepinCeilings {
                max_nulled_rows: bound.map_or(flags.max_nulled_rows, |b| Some(b.max_nulled)),
                max_ambiguous_rows: bound
                    .map_or(flags.max_ambiguous_rows, |b| Some(b.max_ambiguous)),
            },
        )
        .await?;
    let (started_as, job) = match outcome {
        trawl_client::RepinStart::Report(job) => ("dry run", job),
        // The client separates a cancelled row from a report, so the
        // verdict word is decided here rather than by sniffing the status
        // string afterwards (#109).
        trawl_client::RepinStart::Cancelled(job) => ("cancelled", job),
        // A 200 that is neither: the row's own status is the verdict, and
        // `repin_completion` turns it into a non-zero exit.
        trawl_client::RepinStart::Failed(job) => ("did not complete", job),
        trawl_client::RepinStart::Started(job) => ("started", job),
        trawl_client::RepinStart::Refused(job) => (refusal_verdict(job.force), job),
    };

    let mut job = job;
    if flags.wait && job.status == "running" {
        job = wait_for_terminal(&client, job).await?;
    }

    if format == OutputFormat::Table {
        let verdict = repin_verdict_word(started_as, &job.status, job.force);
        writeln!(out, "repin {}: {verdict}", job.field)?;
    }
    let (columns, rows) = repin_job_to_rows(&job, format);
    render_driver_results(&columns, &rows, format, out)?;
    // The case file goes to stdout for a human and to stderr for a machine
    // format (`label`), so a piped `-f json` stays one parseable record while
    // the operator still reads the evidence.
    render_repin_case_file(out, format == OutputFormat::Table, &job)?;
    repin_completion(&job)
}

/// The word the table header prints.
///
/// The status the row ended on outranks the verdict the start request
/// carried: a job started here and then cancelled, refused at the cutover
/// gate or failed while `--wait` polled it would otherwise still print as
/// "started".
fn repin_verdict_word<'a>(started_as: &'a str, status: &str, forced: bool) -> &'a str {
    match status {
        "cancelled" => "cancelled",
        "failed" => "failed",
        "blocked" => "blocked",
        // One status, two facts: `refusal_verdict` is the same split the
        // start arm uses, so a job refused at the cutover gate after
        // `--wait` prints the word its flags earned.
        "refused_needs_force" => refusal_verdict(forced),
        _ => started_as,
    }
}

/// Did the repin this invocation asked for actually happen?
///
/// Exit code is the only thing a script reads, so every terminal status
/// that is not a completed rewrite (or the dry run's report, which is also
/// `succeeded`) has to be an error here. `running` is the one non-terminal
/// pass: without `--wait` the job is deliberately left in flight and the
/// operator polls `repin-status`.
///
/// The catch-all arm is the point (#109 review R2-3). `failed` and
/// `blocked` used to fall past the cancelled and refused checks into
/// `Ok(())`, so a `--wait` that watched a job fail still exited 0, and a
/// cancel whose request row could not be written — which downgrades the
/// job to `failed` on a 200 — printed "dry run" and exited 0 as well.
fn repin_completion(job: &trawl_client::RepinJobResponse) -> Result<(), CliError> {
    match job.status.as_str() {
        "succeeded" | "running" => Ok(()),
        "refused_needs_force" => {
            // A pre-scan refusal reports its projection; a cutover refusal
            // reports what the finished rewrite actually nulled. The remedy
            // branches on the flags the job itself carried (#111): telling a
            // forced job to pass --force is advice it has already taken.
            let lost = if job.rows_nulled > 0 {
                job.rows_nulled
            } else {
                job.projected_nulls
            };
            Err(CliError::Usage(repin_refusal(
                job.requires_force_reason.as_deref(),
                job.force,
                lost,
            )))
        }
        "cancelled" => {
            let by = job.cancelled_by.as_deref().unwrap_or("an operator");
            Err(CliError::Usage(format!(
                "repin cancelled by {}: the corpus was left untouched and the \
                 pin is unchanged",
                trawl_core::sanitize::sanitize_display_text(by)
            )))
        }
        other => {
            // The status and the error text both come off the wire, so both
            // are sanitized before they reach a terminal.
            let detail = job.error.as_deref().map_or_else(String::new, |e| {
                format!(": {}", trawl_core::sanitize::sanitize_display_text(e))
            });
            Err(CliError::Usage(format!(
                "repin ended {}{detail}. The rewrite this command asked for \
                 did not complete; check `trawl schema repin-status` and the \
                 server log",
                trawl_core::sanitize::sanitize_display_text(other)
            )))
        }
    }
}

/// The one-line verdict above a refused job's row.
///
/// `refused_needs_force` is one status covering two different facts. An
/// unforced job accepted no loss at all, so force is what it wants. A forced
/// one accepted a number and the finished rewrite came in over it, and
/// telling that operator they need force reads as though the flag they
/// passed did nothing.
fn refusal_verdict(forced: bool) -> &'static str {
    if forced {
        "refused: over its ceilings"
    } else {
        "refused: needs --force"
    }
}

/// The sentence a refused repin ends on: what the server refused, and what
/// the operator does about it.
///
/// The remedy branches on the flags this invocation actually carried. An
/// unforced run is refused because it accepted no loss at all, so the answer
/// is `--force`; a forced one already passed it and was refused by a
/// ceiling, so telling it to pass force again is advice it has taken. The
/// server's own reason is authoritative when it sent one — the two gates and
/// the wire all ask one function, and it names ambiguity as well as loss —
/// and only the fallback has to guess at the shape of the loss.
fn repin_refusal(reason: Option<&str>, forced: bool, lost: u64) -> String {
    let remedy = if forced {
        "review the loss and raise --max-nulled-rows/--max-ambiguous-rows to \
         accept it"
    } else {
        "re-run with --force to accept it"
    };
    match reason {
        // The reason comes off the wire and lands on a terminal, so it
        // sanitises like every other server sentence this file renders.
        Some(reason) => {
            let reason = trawl_core::sanitize::sanitize_display_text(reason);
            format!("repin refused: {reason} — {remedy}")
        }
        None => format!(
            "repin would null {lost} stored value(s); {remedy} (originals \
             remain findable in _raw)"
        ),
    }
}

/// Poll the status surface until the job leaves `running`.
async fn wait_for_terminal(
    client: &trawl_client::HttpClient,
    job: trawl_client::RepinJobResponse,
) -> Result<trawl_client::RepinJobResponse, CliError> {
    let id = job.id;
    let mut latest = job;
    while latest.status == "running" {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let status = client.schema_repin_status().await?;
        latest = same_job(id, status.job)?;
    }
    Ok(latest)
}

/// One poll of the status surface, checked against the job `--wait` is
/// following.
///
/// The status route answers with the running job, else the newest one, and
/// there is no way to ask it for a job by id. So a poll that comes back
/// naming a different job is not an answer about ours: our job terminalized
/// (cancelled by another operator, say, or failed) and a second job claimed
/// the freed slot before this poll ran.
///
/// Returning the last copy we held would be worse than useless, because
/// that copy still says `running` — the caller's cancelled and refused
/// checks would both read false and the command would exit 0 over a rewrite
/// that never happened. The outcome is genuinely unknown here, so it is an
/// error that says so and names the job it is about (#109 review F1).
fn same_job(
    id: i64,
    seen: Option<trawl_client::RepinJobResponse>,
) -> Result<trawl_client::RepinJobResponse, CliError> {
    match seen {
        Some(job) if job.id == id => Ok(job),
        other => {
            let now = other.map_or_else(
                || "the status surface now reports no job at all".to_owned(),
                |job| format!("repin job {} now occupies the status surface", job.id),
            );
            Err(CliError::Usage(format!(
                "repin job {id} could not be followed to its end: {now}, and \
                 the status surface cannot be asked for a job by id. This \
                 job's outcome is unknown — it was not observed to succeed. \
                 Check `trawl schema repin-status` and the server log for \
                 job {id}"
            )))
        }
    }
}

/// `trawl schema repin-status`.
pub async fn run_repin_status(
    out: &mut impl Write,
    conn: ConnectionParams,
    format: Option<OutputFormat>,
) -> Result<(), CliError> {
    let format = resolve_format(format)?;
    let client = make_client(&conn)?;
    let status = client.schema_repin_status().await?;
    match status.job {
        Some(job) => {
            let (columns, rows) = repin_job_to_rows(&job, format);
            render_driver_results(&columns, &rows, format, out)?;
            render_repin_case_file(out, format == OutputFormat::Table, &job)?;
        }
        None => writeln!(out, "no repin job has ever run")?,
    }
    Ok(())
}

/// `trawl schema repin-cancel` (#109).
///
/// No confirmation prompt, unlike the trigger: cancelling only ever leaves
/// the corpus as it already is, so the destructive direction is the one
/// that needs a human's word.
///
/// Exit code carries the verdict, because a script that asks for a stop has
/// to know whether it got one: accepted exits 0, and both refusals — the
/// job is past its point of no return, or nothing was running — exit
/// non-zero through the usual error path.
pub async fn run_repin_cancel(
    out: &mut impl Write,
    conn: ConnectionParams,
    format: Option<OutputFormat>,
) -> Result<(), CliError> {
    let format = resolve_format(format)?;
    let client = make_client(&conn)?;
    let (accepted, receipt) = match client.schema_repin_cancel().await? {
        trawl_client::RepinCancel::Cancelling(r) => (true, r),
        trawl_client::RepinCancel::PastPointOfNoReturn(r)
        | trawl_client::RepinCancel::NoJobRunning(r) => (false, r),
    };
    // The server ships facts and the consumer writes the words, so the
    // detail is printed as the server phrased it — through display
    // sanitisation, like every other server sentence this command renders.
    let detail = trawl_core::sanitize::sanitize_display_text(&receipt.detail);
    if format == OutputFormat::Table {
        // A human reads the sentence first and the job's numbers second, so
        // the table keeps two blocks.
        writeln!(out, "repin cancel: {detail}")?;
        if let Some(job) = &receipt.job {
            let (columns, rows) = repin_job_to_rows(job, format);
            render_driver_results(&columns, &rows, format, out)?;
        }
    } else {
        // One record, one header: the verdict and the job it is about are
        // one answer, and a machine format that emitted them as two tables
        // would put two headers of different widths in one csv stream.
        // The job columns are null when the server attached no row.
        let (columns, rows) = cancel_receipt_to_rows(&receipt, format);
        render_driver_results(&columns, &rows, format, out)?;
    }
    if accepted {
        return Ok(());
    }
    Err(CliError::Usage(format!("repin cancel refused: {detail}")))
}

/// A cancel receipt → exactly one record: the verdict, the server's
/// sentence, and the job columns the verdict is about.
///
/// One record rather than two tables, because `-f csv` is a single stream:
/// a receipt table followed by a job table would put two headers of
/// different widths in it, which no csv reader accepts. A verdict with no
/// job attached — a 404, or a store that could not serve the row — leaves
/// the job columns null, so the shape does not depend on what the server
/// managed to look up.
fn cancel_receipt_to_rows(
    receipt: &trawl_client::RepinCancelResponse,
    format: OutputFormat,
) -> (Vec<String>, Vec<Vec<Json>>) {
    let outcome = match receipt.outcome {
        trawl_client::RepinCancelOutcome::Cancelling => "cancelling",
        trawl_client::RepinCancelOutcome::PastPointOfNoReturn => "past_point_of_no_return",
        trawl_client::RepinCancelOutcome::NoJobRunning => "no_job_running",
    };
    let job_columns = repin_job_columns();
    let mut columns = vec!["outcome".to_owned(), "detail".to_owned()];
    let mut cells = vec![
        Json::from(outcome),
        Json::from(trawl_core::sanitize::sanitize_display_text(&receipt.detail)),
    ];
    cells.extend(receipt.job.as_ref().map_or_else(
        || vec![Json::Null; job_columns.len()],
        |job| repin_job_cells(job, format),
    ));
    columns.extend(job_columns);
    (columns, vec![cells])
}

// -- degraded-badge acknowledgement -------------------------------------------

/// One acknowledgement → generic (columns, rows): a single record, like the
/// repin job row, so `-f json` is one parseable object.
///
/// The note is operator prose that came back off the wire, so it goes
/// through display sanitisation before it reaches a terminal.
pub fn ack_to_rows(
    field: &str,
    ack: &trawl_client::FieldAck,
    format: OutputFormat,
) -> (Vec<String>, Vec<Vec<Json>>) {
    let columns = ["field", "acked_at", "acked_by", "evidence_through", "note"]
        .map(str::to_owned)
        .to_vec();
    let rows = vec![vec![
        // Machine formats carry the exact catalog key (the operator's own
        // argument, matched by scripted callers); the table cell is display
        // text on a terminal and sanitises like every other rendered value.
        Json::from(if format == OutputFormat::Table {
            trawl_core::sanitize::sanitize_display_text(field)
        } else {
            field.to_owned()
        }),
        Json::from(ack.acked_at.clone()),
        Json::from(trawl_core::sanitize::sanitize_display_text(&ack.acked_by)),
        Json::from(ack.evidence_through),
        ack.note.as_deref().map_or(Json::Null, |n| {
            Json::from(trawl_core::sanitize::sanitize_display_text(n))
        }),
    ]];
    (columns, rows)
}

/// `trawl schema ack <field> [--note <text>] [--clear]`.
///
/// Acknowledging a degraded badge says "I have seen this evidence", not "the
/// pin is fine": the ack covers the conflict episodes that exist when the
/// server writes it, so the next episode raises the badge again. A repin of
/// the field clears it outright.
///
/// `--clear` withdraws the acknowledgement. It prints one line rather than a
/// record: the DELETE has no body, and a null-filled row would be a record
/// the server never sent.
pub async fn run_ack<W: Write>(
    out: &mut W,
    conn: ConnectionParams,
    field: &str,
    note: Option<&str>,
    clear: bool,
    format: Option<OutputFormat>,
) -> Result<(), CliError> {
    let format = resolve_format(format)?;
    let human = format == OutputFormat::Table;
    let client = make_client(&conn)?;

    if clear {
        client.schema_field_ack_clear(field).await?;
        label(
            out,
            human,
            &format!(
                "acknowledgement cleared: {} (the badge returns if the \
                 evidence still indicts the pin)",
                trawl_core::sanitize::sanitize_display_text(field)
            ),
        )?;
        return Ok(());
    }

    let ack = client.schema_field_ack(field, note).await?;
    let (columns, rows) = ack_to_rows(field, &ack, format);
    render(out, &columns, &rows, format)?;
    label(
        out,
        human,
        &format!(
            "acknowledged through {} conflict episode(s); the next episode \
             raises the badge again",
            ack.evidence_through
        ),
    )?;
    Ok(())
}

#[cfg(test)]
mod ack_tests {
    use super::*;

    fn sample_ack() -> trawl_client::FieldAck {
        trawl_client::FieldAck {
            acked_at: "2026-09-02T09:00:00Z".into(),
            acked_by: "tkl_abc123".into(),
            note: Some("sender ships a fix on Friday".into()),
            evidence_through: 7,
        }
    }

    /// The ack renders as one record in every format the schema family
    /// honours: a scripted caller reads it without parsing prose.
    #[test]
    fn the_ack_renders_as_one_record_in_every_format() {
        let (columns, rows) = ack_to_rows("duration", &sample_ack(), OutputFormat::Json);
        assert_eq!(rows.len(), 1, "an ack is one record");
        for format in [OutputFormat::Table, OutputFormat::Json, OutputFormat::Csv] {
            let mut out = Vec::new();
            render_driver_results(&columns, &rows, format, &mut out).unwrap();
            let text = String::from_utf8(out).unwrap();
            assert!(text.contains("duration"), "{format:?}: {text}");
            assert!(text.contains("tkl_abc123"), "{format:?}: {text}");
        }

        let mut out = Vec::new();
        render_driver_results(&columns, &rows, OutputFormat::Json, &mut out).unwrap();
        let parsed: Json =
            serde_json::from_str(String::from_utf8(out).unwrap().lines().next().unwrap()).unwrap();
        assert_eq!(parsed["evidence_through"], 7);
        assert_eq!(parsed["note"], "sender ships a fix on Friday");

        // No note is null, never an empty string: the operator wrote
        // nothing, and an empty note would read as one they left blank.
        let mut bare = sample_ack();
        bare.note = None;
        let (columns, rows) = ack_to_rows("duration", &bare, OutputFormat::Json);
        let mut out = Vec::new();
        render_driver_results(&columns, &rows, OutputFormat::Json, &mut out).unwrap();
        let parsed: Json =
            serde_json::from_str(String::from_utf8(out).unwrap().lines().next().unwrap()).unwrap();
        assert_eq!(parsed["note"], Json::Null);
    }

    /// The note is sender-adjacent text in a terminal: an operator can paste
    /// anything into it, and it comes back over the wire, so it is
    /// sanitised on display like every other value this file prints.
    #[test]
    fn a_hostile_note_is_sanitised_before_it_reaches_the_terminal() {
        let mut ack = sample_ack();
        ack.note = Some("boom\u{1b}[2Jgone".into());
        let (_, rows) = ack_to_rows("duration", &ack, OutputFormat::Json);
        let note = rows[0][4].as_str().unwrap();
        assert!(!note.contains('\u{1b}'), "escape survived: {note:?}");
    }

    /// The field column splits by format: the table cell is terminal
    /// display and sanitises; machine formats carry the exact catalog key
    /// a scripted caller matches on.
    #[test]
    fn the_table_field_cell_sanitises_and_the_machine_cell_does_not() {
        let hostile = "du\u{1b}[2Jration";
        let (_, table) = ack_to_rows(hostile, &sample_ack(), OutputFormat::Table);
        let (_, json) = ack_to_rows(hostile, &sample_ack(), OutputFormat::Json);
        let table_cell = table[0][0].as_str().unwrap();
        let json_cell = json[0][0].as_str().unwrap();
        assert!(
            !table_cell.contains('\u{1b}'),
            "table cell keeps the escape: {table_cell:?}"
        );
        assert_eq!(json_cell, hostile, "machine cell must stay exact");
    }
}

#[cfg(test)]
mod repin_tests {
    use super::*;

    fn sample_job() -> trawl_client::RepinJobResponse {
        trawl_client::RepinJobResponse {
            id: 3,
            field: "status".into(),
            from_type: "BIGINT".into(),
            to_type: "VARCHAR".into(),
            dry_run: true,
            force: false,
            status: "succeeded".into(),
            requested_by: Some("ops".into()),
            started_at: "2026-08-12T10:00:00Z".into(),
            finished_at: Some("2026-08-12T10:00:05Z".into()),
            error: None,
            files_total: 4,
            rows_carrying: 1000,
            projected_nulls: 0,
            resurrectable: 25,
            affected_bytes: 1 << 20,
            files_done: 4,
            rows_rewritten: 1200,
            rows_nulled: 0,
            rows_resurrected: 25,
            dialect: None,
            ambiguous_numerals: 0,
            unmapped_samples: Vec::new(),
            liveness: None,
            requires_force: Some(false),
            requires_force_reason: None,
            cancel_requested_at: None,
            cancelled_by: None,
            max_nulled_rows: None,
            max_ambiguous_rows: None,
            accepted_max_nulled_rows: None,
            accepted_max_ambiguous_rows: None,
        }
    }

    /// The job renders through the shared driver formatter in every
    /// format the schema family honours.
    #[test]
    fn repin_job_renders_in_table_json_and_csv() {
        let (columns, rows) = repin_job_to_rows(&sample_job(), OutputFormat::Table);
        for format in [OutputFormat::Table, OutputFormat::Json, OutputFormat::Csv] {
            let mut out = Vec::new();
            render_driver_results(&columns, &rows, format, &mut out).unwrap();
            let text = String::from_utf8(out).unwrap();
            assert!(text.contains("VARCHAR"), "{format:?}: {text}");
            assert!(text.contains("succeeded"), "{format:?}: {text}");
        }
        let mut out = Vec::new();
        render_driver_results(&columns, &rows, OutputFormat::Json, &mut out).unwrap();
        let parsed: Json =
            serde_json::from_str(String::from_utf8(out).unwrap().lines().next().unwrap()).unwrap();
        assert_eq!(parsed["resurrectable"], 25);
        assert_eq!(parsed["projected_nulls"], 0);
    }

    /// The operator-facing transcript of a `--dry-run`, rendered by the real
    /// CLI path.
    ///
    /// The numbers are the ones the server produces in
    /// `trawl-server/tests/repin.rs::repin_to_severity_dry_run_matches_the_executed_rewrite`
    /// (five rows carrying, one unreadable `gold`, one dialect-ambiguous
    /// `3`), so this pins what an operator reads when the plan says the
    /// executing request would refuse.
    #[test]
    fn the_dry_run_transcript_shows_the_plan_the_evidence_and_the_verdict() {
        let job = trawl_client::RepinJobResponse {
            id: 1,
            field: "level".into(),
            from_type: "VARCHAR".into(),
            to_type: "SEVERITY".into(),
            dry_run: true,
            force: false,
            status: "succeeded".into(),
            requested_by: Some("ops".into()),
            started_at: "2026-08-17T12:00:00Z".into(),
            finished_at: Some("2026-08-17T12:00:01Z".into()),
            error: None,
            files_total: 1,
            rows_carrying: 5,
            projected_nulls: 1,
            resurrectable: 0,
            affected_bytes: 4096,
            files_done: 0,
            rows_rewritten: 0,
            rows_nulled: 0,
            rows_resurrected: 0,
            dialect: Some("otel".into()),
            ambiguous_numerals: 1,
            unmapped_samples: vec!["gold".into()],
            liveness: Some(trawl_client::RepinLiveness {
                last_seen: "2026-08-17T11:59:58Z".into(),
                service: "api".into(),
            }),
            requires_force: Some(true),
            requires_force_reason: Some(
                "1 stored value(s) cannot be read as SEVERITY and would be nulled \
                 (the originals stay findable in _raw)"
                    .into(),
            ),
            cancel_requested_at: None,
            cancelled_by: None,
            max_nulled_rows: None,
            max_ambiguous_rows: None,
            accepted_max_nulled_rows: None,
            accepted_max_ambiguous_rows: None,
        };

        let mut out = Vec::new();
        writeln!(out, "repin {}: dry run", job.field).unwrap();
        let (columns, rows) = repin_job_to_rows(&job, OutputFormat::Table);
        render_driver_results(&columns, &rows, OutputFormat::Table, &mut out).unwrap();
        render_repin_case_file(&mut out, true, &job).unwrap();
        let text = String::from_utf8(out).unwrap();

        // Printed, not just asserted: the transcript as a whole is what an
        // operator reads, and `--no-capture` shows it.
        println!("{text}");

        for expected in [
            "repin level: dry run",
            "SEVERITY",
            "otel",
            "values the new pin cannot read:",
            "- gold",
            "STILL being written",
            "requires --force:",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
    }

    /// The case file is the evidence a plan's numbers cannot carry: the
    /// asserted dialect, the ambiguous rows, the values the pin cannot read,
    /// the force verdict, and, for a field something is still writing, the
    /// discontinuity warning. A repin translates history, while live events
    /// keep taking the ingest-time reading.
    #[test]
    fn the_repin_case_file_states_the_evidence_and_the_discontinuity() {
        let mut job = sample_job();
        job.field = "level".into();
        job.to_type = "SEVERITY".into();
        job.dialect = Some("syslog".into());
        job.ambiguous_numerals = 3;
        job.unmapped_samples = vec!["gold".into(), "platinum".into()];
        job.liveness = Some(trawl_client::RepinLiveness {
            last_seen: "2026-08-17T09:00:00Z".into(),
            service: "nginx".into(),
        });
        job.requires_force = Some(true);
        job.requires_force_reason = Some("2 stored value(s) cannot be read as SEVERITY".into());

        let mut out = Vec::new();
        render_repin_case_file(&mut out, true, &job).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("numeral dialect: syslog"), "{text}");
        assert!(text.contains("ambiguous numerals: 3 row(s)"), "{text}");
        assert!(
            text.contains("- gold") && text.contains("- platinum"),
            "{text}"
        );
        assert!(text.contains("_raw"), "{text}");
        // The discontinuity, in the operator's terms and with the remedy.
        assert!(text.contains("STILL being written"), "{text}");
        assert!(text.contains("nginx"), "{text}");
        assert!(text.contains("historical `3` becomes 17"), "{text}");
        assert!(text.contains("severity_from"), "{text}");
        assert!(text.contains("requires --force:"), "{text}");

        // A clean plan says none of it — no dialect line for a non-severity
        // target, no warning for a field nothing writes.
        let mut out = Vec::new();
        render_repin_case_file(&mut out, true, &sample_job()).unwrap();
        assert!(String::from_utf8(out).unwrap().is_empty());
    }

    /// The row carries every fact, in every machine format: a scripted
    /// caller must never have to parse the case file's prose. CSV joins the
    /// samples into one cell (", "-separated) so the first sample's first
    /// character is the cell's first character, where the formula-injection
    /// prefix fires; rendered as a JSON array, a hostile sample would hide
    /// behind the `[`. JSON keeps the real array.
    #[test]
    fn the_repin_row_carries_the_evidence_in_every_machine_format() {
        let mut job = sample_job();
        job.dialect = Some("otel".into());
        job.ambiguous_numerals = 7;
        job.unmapped_samples = vec!["gold".into(), "=cmd()".into()];
        job.liveness = Some(trawl_client::RepinLiveness {
            last_seen: "2026-08-17T09:00:00Z".into(),
            service: "nginx".into(),
        });
        job.requires_force = Some(true);
        job.requires_force_reason = Some("2 stored value(s) cannot be read".into());
        let render = |format| {
            let (columns, rows) = repin_job_to_rows(&job, format);
            let mut out = Vec::new();
            render_driver_results(&columns, &rows, format, &mut out).unwrap();
            String::from_utf8(out).unwrap()
        };

        let parsed: Json =
            serde_json::from_str(render(OutputFormat::Json).lines().next().unwrap()).unwrap();
        assert_eq!(parsed["dialect"], "otel");
        assert_eq!(parsed["ambiguous_numerals"], 7);
        assert_eq!(parsed["requires_force"], true);
        assert_eq!(
            parsed["requires_force_reason"],
            "2 stored value(s) cannot be read"
        );
        assert_eq!(parsed["unmapped_samples"][0], "gold");
        assert_eq!(parsed["field_last_seen"], "2026-08-17T09:00:00Z");
        assert_eq!(parsed["field_last_service"], "nginx");

        let csv = render(OutputFormat::Csv);
        assert!(csv.contains("unmapped_samples"), "{csv}");
        assert!(csv.contains("gold"), "{csv}");
        assert!(csv.contains("nginx"), "{csv}");
        assert!(csv.contains("2 stored value(s) cannot be read"), "{csv}");

        let table = render(OutputFormat::Table);
        assert!(table.contains("otel") && table.contains("nginx"), "{table}");

        // A sample whose first character is a formula trigger lands at the
        // start of the joined cell, where the CSV escaping neutralises it.
        let mut hostile = job.clone();
        hostile.unmapped_samples = vec!["=cmd()".into(), "gold".into()];
        let (columns, rows) = repin_job_to_rows(&hostile, OutputFormat::Csv);
        let mut out = Vec::new();
        render_driver_results(&columns, &rows, OutputFormat::Csv, &mut out).unwrap();
        let csv = String::from_utf8(out).unwrap();
        assert!(csv.contains("'=cmd()"), "formula prefix missing: {csv}");

        // A job whose scan has not recorded a plan reports no verdict —
        // never `false`, which would read as "safe to execute".
        let mut running = sample_job();
        running.status = "running".into();
        running.requires_force = None;
        running.requires_force_reason = None;
        let (columns, rows) = repin_job_to_rows(&running, OutputFormat::Json);
        let mut out = Vec::new();
        render_driver_results(&columns, &rows, OutputFormat::Json, &mut out).unwrap();
        let parsed: Json =
            serde_json::from_str(String::from_utf8(out).unwrap().lines().next().unwrap()).unwrap();
        assert_eq!(parsed["requires_force"], Json::Null);
        let mut out = Vec::new();
        render_repin_case_file(&mut out, true, &running).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("not known yet"), "{text}");
        assert!(!text.contains("requires --force"), "{text}");
    }

    /// The ceiling decision, at the level where it is a decision: which
    /// numbers a forced run is bound to, and whether it has to scan first
    /// to learn them.
    ///
    /// The flow around it needs a server (two round trips, the second
    /// carrying the first's answer), so what a unit test can pin is the
    /// decision and the values, not the wire. The paired evidence that the
    /// second request really carries them lives in the server's repin
    /// integration tests, which see the persisted request ceilings.
    #[test]
    fn a_forced_run_binds_stated_ceilings_and_previews_for_the_rest() {
        let base = RepinFlags {
            dialect: None,
            dry_run: false,
            force: false,
            yes: true,
            wait: false,
            max_nulled_rows: None,
            max_ambiguous_rows: None,
        };
        // Unforced: no loss is accepted at all, so there are no terms.
        assert_eq!(ceiling_plan(&base), CeilingPlan::NotApplicable);
        // A dry run mutates nothing; there is nothing to hold it to.
        assert_eq!(
            ceiling_plan(&RepinFlags {
                dry_run: true,
                force: true,
                max_nulled_rows: Some(5),
                ..base
            }),
            CeilingPlan::NotApplicable
        );
        // Both stated: execute with them, no preview scan.
        assert_eq!(
            ceiling_plan(&RepinFlags {
                force: true,
                max_nulled_rows: Some(250),
                max_ambiguous_rows: Some(0),
                ..base
            }),
            CeilingPlan::Stated(BoundCeilings {
                max_nulled: 250,
                max_ambiguous: 0,
            })
        );
        // One stated is not both: the other half only exists once a scan
        // has counted the corpus.
        for flags in [
            RepinFlags {
                force: true,
                max_nulled_rows: Some(250),
                ..base
            },
            RepinFlags {
                force: true,
                max_ambiguous_rows: Some(3),
                ..base
            },
            RepinFlags {
                force: true,
                ..base
            },
        ] {
            assert_eq!(ceiling_plan(&flags), CeilingPlan::Preview);
        }
    }

    /// What the preview hands the execution: both accepted numbers, or
    /// nothing. A half-resolved report is a job that has not scanned (or a
    /// server without ceilings), and restating half of a pair would bind
    /// one dimension while the other quietly re-derived.
    #[test]
    fn the_preview_restates_both_accepted_ceilings_or_neither() {
        let mut job = sample_job();
        job.accepted_max_nulled_rows = Some(22);
        job.accepted_max_ambiguous_rows = Some(10);
        assert_eq!(
            accepted_ceilings(&job),
            Some(BoundCeilings {
                max_nulled: 22,
                max_ambiguous: 10,
            })
        );
        // The printed sentence carries the numbers the request will state.
        let notice = ceiling_notice(accepted_ceilings(&job).unwrap(), true);
        assert!(notice.contains("22"), "{notice}");
        assert!(notice.contains("10"), "{notice}");
        assert!(notice.contains("preview scan"), "{notice}");
        assert!(!ceiling_notice(accepted_ceilings(&job).unwrap(), false).contains("preview scan"));

        job.accepted_max_ambiguous_rows = None;
        assert_eq!(accepted_ceilings(&job), None);
        assert_eq!(accepted_ceilings(&sample_job()), None);
    }

    /// The remedy a refusal ends on depends on what the invocation already
    /// carried: force is the answer to a refusal that accepted no loss, and
    /// nonsense to one that was refused by a ceiling.
    #[test]
    fn a_refusal_names_the_remedy_the_operator_has_not_tried() {
        let reason = "12 nulled row(s) over an accepted 10";

        let unforced = repin_refusal(Some(reason), false, 12);
        assert!(unforced.contains(reason), "{unforced}");
        assert!(unforced.contains("re-run with --force"), "{unforced}");
        assert!(!unforced.contains("--max-nulled-rows"), "{unforced}");

        let forced = repin_refusal(Some(reason), true, 12);
        assert!(forced.contains(reason), "{forced}");
        assert!(
            forced.contains("raise --max-nulled-rows/--max-ambiguous-rows"),
            "{forced}"
        );
        assert!(
            !forced.contains("re-run with --force"),
            "an operator who passed force is not told to pass it: {forced}"
        );

        // The fallback, for a refusal the server sent no reason with.
        let bare = repin_refusal(None, false, 7);
        assert!(bare.contains("would null 7 stored value(s)"), "{bare}");
        assert!(bare.contains("re-run with --force"), "{bare}");
        assert!(
            repin_refusal(None, true, 7).contains("raise --max-nulled-rows"),
            "the fallback branches too"
        );
    }

    /// Every place the CLI names a refusal branches on whether the job
    /// carried force, so an operator who already passed it is never told to
    /// pass it: the table verdict, and the case file's own line.
    #[test]
    fn a_forced_refusal_is_labelled_by_its_ceilings_not_by_the_flag() {
        assert_eq!(refusal_verdict(false), "refused: needs --force");
        assert_eq!(refusal_verdict(true), "refused: over its ceilings");

        let mut job = sample_job();
        job.status = "refused_needs_force".into();
        job.requires_force = Some(true);
        job.requires_force_reason = Some("12 nulled row(s) over an accepted 10".into());

        let case_file = |job: &trawl_client::RepinJobResponse| {
            let mut out = Vec::new();
            render_repin_case_file(&mut out, true, job).unwrap();
            String::from_utf8(out).unwrap()
        };

        let unforced = case_file(&job);
        assert!(unforced.contains("requires --force:"), "{unforced}");
        assert!(!unforced.contains("--max-nulled-rows"), "{unforced}");

        job.force = true;
        let forced = case_file(&job);
        assert!(forced.contains("over its accepted ceilings"), "{forced}");
        assert!(forced.contains("--max-nulled-rows"), "{forced}");
        assert!(
            !forced.contains("requires --force"),
            "the flag was already passed: {forced}"
        );
        assert!(
            forced.contains("12 nulled row(s) over an accepted 10"),
            "the server's own reason survives either label: {forced}"
        );
    }

    /// The accepted ceilings are part of the case file and part of the
    /// machine record: an operator reading a finished job sees the terms it
    /// ran under without asking postgres.
    #[test]
    fn the_case_file_and_the_row_report_the_accepted_ceilings() {
        let mut job = sample_job();
        job.force = true;
        job.max_nulled_rows = Some(25);
        job.accepted_max_nulled_rows = Some(22);
        job.accepted_max_ambiguous_rows = Some(10);

        let mut out = Vec::new();
        render_repin_case_file(&mut out, true, &job).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("accepted ceilings: 22"), "{text}");
        assert!(text.contains("10 dialect-ambiguous"), "{text}");

        let (columns, rows) = repin_job_to_rows(&job, OutputFormat::Json);
        let mut out = Vec::new();
        render_driver_results(&columns, &rows, OutputFormat::Json, &mut out).unwrap();
        let parsed: Json =
            serde_json::from_str(String::from_utf8(out).unwrap().lines().next().unwrap()).unwrap();
        assert_eq!(parsed["accepted_max_nulled_rows"], 22);
        assert_eq!(parsed["accepted_max_ambiguous_rows"], 10);
        // The request's echo rides beside the accepted pair: the operator
        // asked for 25 and the job is held to 22, and a machine format that
        // showed only one of the two could not tell that apart from a
        // default nobody stated.
        assert_eq!(parsed["max_nulled_rows"], 25);
        assert_eq!(
            parsed["max_ambiguous_rows"],
            Json::Null,
            "an unstated ceiling echoes as null, not as the resolved one"
        );

        // An unforced job accepts no loss, so it states no ceilings.
        let (_, rows) = repin_job_to_rows(&sample_job(), OutputFormat::Json);
        assert_eq!(
            rows[0][columns
                .iter()
                .position(|c| c == "accepted_max_nulled_rows")
                .unwrap()],
            Json::Null
        );
    }

    /// An executing repin off a TTY refuses without `--yes` before any
    /// network access — the connection params here are deliberately
    /// unusable, so reaching the client would fail differently.
    #[tokio::test]
    async fn non_tty_execute_without_yes_refuses() {
        let conn = ConnectionParams {
            url: "https://127.0.0.1:1".into(),
            token: "unused".into(),
            insecure: true,
        };
        let mut out = Vec::new();
        let err = run_repin(
            &mut out,
            conn,
            "status",
            "VARCHAR",
            RepinFlags {
                dialect: None,
                dry_run: false,
                force: false,
                yes: false,
                wait: false,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
            },
            Some(OutputFormat::Json),
        )
        .await
        .expect_err("must refuse without --yes off a TTY");
        assert!(
            matches!(err, CliError::Usage(ref msg) if msg.contains("--yes")),
            "got {err:?}"
        );
        assert!(out.is_empty(), "nothing rendered before the refusal");
    }

    /// `--dialect` is a severity-only assertion, refused here before any
    /// network access — the server refuses it too, but this one is a typo
    /// an operator can fix without spending a job claim. A severity target
    /// carries it through (the refusal below is the `--yes` gate, i.e. the
    /// dialect check passed).
    #[tokio::test]
    async fn dialect_applies_to_a_severity_target_only() {
        let conn = ConnectionParams {
            url: "https://127.0.0.1:1".into(),
            token: "unused".into(),
            insecure: true,
        };
        let flags = RepinFlags {
            dialect: Some(crate::cli::SeverityDialect::Syslog),
            dry_run: true,
            force: false,
            yes: false,
            wait: false,
            max_nulled_rows: None,
            max_ambiguous_rows: None,
        };
        for to in ["VARCHAR", "bigint"] {
            let mut out = Vec::new();
            let err = run_repin(&mut out, conn.clone(), "level", to, flags, None)
                .await
                .expect_err("a dialect on a non-severity target must refuse");
            assert!(
                matches!(err, CliError::Usage(ref msg) if msg.contains("--to severity only")),
                "{to}: got {err:?}"
            );
            assert!(out.is_empty());
        }
        // A severity target gets past the flag check and stops at the
        // execute-confirmation gate instead.
        let mut out = Vec::new();
        let err = run_repin(
            &mut out,
            conn,
            "level",
            "severity",
            RepinFlags {
                dry_run: false,
                ..flags
            },
            Some(OutputFormat::Json),
        )
        .await
        .expect_err("no TTY, no --yes");
        assert!(
            matches!(err, CliError::Usage(ref msg) if msg.contains("--yes")),
            "got {err:?}"
        );
    }

    /// The cancel receipt renders in every format, and a machine format
    /// carries the verdict even when the server attached no job row.
    ///
    /// With a job attached it is still ONE record: one header, one width.
    /// Two tables in one csv stream — a two-column receipt followed by the
    /// 23-column job — is not a csv document at all (#109 review F4).
    #[test]
    fn a_cancel_receipt_renders_its_verdict_in_every_format() {
        for (outcome, spelling) in [
            (trawl_client::RepinCancelOutcome::Cancelling, "cancelling"),
            (
                trawl_client::RepinCancelOutcome::PastPointOfNoReturn,
                "past_point_of_no_return",
            ),
            (
                trawl_client::RepinCancelOutcome::NoJobRunning,
                "no_job_running",
            ),
        ] {
            let receipt = trawl_client::RepinCancelResponse {
                outcome,
                detail: "the words the server chose".into(),
                job: None,
            };
            for format in [OutputFormat::Json, OutputFormat::Csv] {
                let (columns, rows) = cancel_receipt_to_rows(&receipt, format);
                let mut out = Vec::new();
                render_driver_results(&columns, &rows, format, &mut out).unwrap();
                let text = String::from_utf8(out).unwrap();
                assert!(text.contains(spelling), "{format:?}: {text}");
            }
        }

        // A job rides along: same header, same width, and the job's own
        // facts are in the same record as the verdict.
        let mut job = sample_job();
        job.status = "cancelled".into();
        job.cancelled_by = Some("ops".into());
        let receipt = trawl_client::RepinCancelResponse {
            outcome: trawl_client::RepinCancelOutcome::Cancelling,
            detail: "cancel accepted".into(),
            job: Some(job),
        };

        let (columns, rows) = cancel_receipt_to_rows(&receipt, OutputFormat::Csv);
        let mut out = Vec::new();
        render_driver_results(&columns, &rows, OutputFormat::Csv, &mut out).unwrap();
        let csv = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = csv.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "one header, one row: {csv}");
        let header = lines[0];
        assert!(header.starts_with("outcome,detail,"), "{header}");
        assert_eq!(
            header.matches("outcome").count(),
            1,
            "exactly one header line: {csv}"
        );
        let width = |line: &str| line.chars().filter(|c| *c == ',').count();
        assert_eq!(width(lines[0]), width(lines[1]), "rectangular: {csv}");
        assert!(lines[1].contains("cancelled"), "{csv}");
        assert!(lines[1].contains("ops"), "{csv}");

        // json is ndjson: one object carrying both halves.
        let (columns, rows) = cancel_receipt_to_rows(&receipt, OutputFormat::Json);
        let mut out = Vec::new();
        render_driver_results(&columns, &rows, OutputFormat::Json, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 1, "one object: {text}");
        let parsed: Json = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["outcome"], "cancelling");
        assert_eq!(parsed["status"], "cancelled");
        assert_eq!(parsed["cancelled_by"], "ops");

        // No job attached: the same columns, nulled.
        let bodiless = trawl_client::RepinCancelResponse {
            outcome: trawl_client::RepinCancelOutcome::NoJobRunning,
            detail: "nothing running".into(),
            job: None,
        };
        let (with_job, _) = cancel_receipt_to_rows(&receipt, OutputFormat::Json);
        let (without_job, rows) = cancel_receipt_to_rows(&bodiless, OutputFormat::Json);
        assert_eq!(with_job, without_job);
        let mut out = Vec::new();
        render_driver_results(&without_job, &rows, OutputFormat::Json, &mut out).unwrap();
        let parsed: Json =
            serde_json::from_str(String::from_utf8(out).unwrap().lines().next().unwrap()).unwrap();
        assert_eq!(parsed["outcome"], "no_job_running");
        assert_eq!(parsed["status"], Json::Null);
    }

    /// `--wait` follows one job, and a poll that comes back about another
    /// one is an unknown outcome, never an answer.
    ///
    /// The race: job A is started under `--wait`, another operator cancels
    /// it, A terminalizes, and job B claims the freed slot before A's next
    /// poll. The status surface then names B. Handing back the caller's
    /// stale copy of A would hand back a row still saying `running`, which
    /// reads as neither cancelled nor refused, and the command would exit 0
    /// over a rewrite that never ran.
    #[test]
    fn a_successor_job_on_the_status_surface_is_not_an_answer() {
        let mut ours = sample_job();
        ours.id = 7;
        ours.status = "running".into();

        assert_eq!(same_job(7, Some(ours.clone())).unwrap().id, 7);

        let mut successor = sample_job();
        successor.id = 8;
        for seen in [Some(successor), None] {
            let err = same_job(7, seen).expect_err("a poll about another job is not an answer");
            let CliError::Usage(msg) = err else {
                panic!("expected a usage error, got {err:?}")
            };
            assert!(msg.contains("repin job 7"), "{msg}");
            assert!(msg.contains("unknown"), "{msg}");
            assert!(msg.contains("repin-status"), "{msg}");
        }
    }

    /// Only a completed rewrite (or a dry run's report) exits 0.
    ///
    /// `failed` is the reachable one this test exists for: a cancel whose
    /// request row cannot be written downgrades the job to `failed`, the
    /// route still answers 200, and under `--wait` the old code fell past
    /// the cancelled and refused checks into `Ok(())` — exit 0 over a repin
    /// that never ran (#109 review R2-3).
    #[test]
    fn only_a_finished_repin_exits_zero() {
        for status in ["succeeded", "running"] {
            let mut job = sample_job();
            job.status = status.into();
            repin_completion(&job).unwrap_or_else(|e| panic!("{status} must pass: {e:?}"));
        }

        for status in ["failed", "blocked", "something_new"] {
            let mut job = sample_job();
            job.status = status.into();
            job.error = Some("the store went away".into());
            let err = repin_completion(&job).expect_err("{status} is not a success");
            let CliError::Usage(msg) = err else {
                panic!("expected a usage error, got {err:?}")
            };
            assert!(msg.contains(status), "the real status is named: {msg}");
            assert!(msg.contains("the store went away"), "{msg}");
            assert!(msg.contains("repin-status"), "{msg}");
            assert!(!msg.contains("dry run"), "{msg}");
        }
    }

    /// The table header names what the row ended on, not what the start
    /// request said. A `failed` row must never print as "dry run", and a
    /// refusal keeps the split its flags earned: a forced job was stopped
    /// by its ceilings, not by a missing flag it already passed.
    #[test]
    fn the_header_verdict_follows_the_terminal_row() {
        assert_eq!(repin_verdict_word("dry run", "succeeded", false), "dry run");
        assert_eq!(repin_verdict_word("started", "running", false), "started");
        assert_eq!(repin_verdict_word("dry run", "failed", false), "failed");
        assert_eq!(
            repin_verdict_word("started", "cancelled", false),
            "cancelled"
        );
        assert_eq!(repin_verdict_word("started", "blocked", false), "blocked");
        assert_eq!(
            repin_verdict_word("started", "refused_needs_force", false),
            "refused: needs --force"
        );
        assert_eq!(
            repin_verdict_word("started", "refused_needs_force", true),
            "refused: over its ceilings"
        );
    }

    /// A cancelled job row shows who asked and when, in the machine
    /// formats a script reads.
    #[test]
    fn a_cancelled_job_row_names_the_asker() {
        let mut job = sample_job();
        job.status = "cancelled".into();
        job.cancel_requested_at = Some("2026-09-03T21:00:00Z".into());
        job.cancelled_by = Some("ops".into());
        job.error = Some("cancelled by ops during build; the live corpus was never touched".into());
        let (columns, rows) = repin_job_to_rows(&job, OutputFormat::Json);
        let mut out = Vec::new();
        render_driver_results(&columns, &rows, OutputFormat::Json, &mut out).unwrap();
        let parsed: Json =
            serde_json::from_str(String::from_utf8(out).unwrap().lines().next().unwrap()).unwrap();
        assert_eq!(parsed["status"], "cancelled");
        assert_eq!(parsed["cancelled_by"], "ops");
        assert_eq!(parsed["cancel_requested_at"], "2026-09-03T21:00:00Z");
    }
}

/// `GcPinsResponse` candidates → generic (columns, rows).
///
/// One rectangle: the run's summary numbers are printed as labels around
/// this table, never as a second header inside it, so a piped `-f csv` is
/// one parseable record set.
pub fn gc_candidates_to_rows(resp: &trawl_client::GcPinsResponse) -> (Vec<String>, Vec<Vec<Json>>) {
    let columns = ["field", "type", "last_seen", "services"]
        .map(str::to_owned)
        .to_vec();
    let rows = resp
        .candidates
        .iter()
        .map(|c| {
            vec![
                Json::from(c.field.clone()),
                Json::from(c.data_type.clone()),
                c.last_seen.clone().map_or(Json::Null, Json::from),
                Json::from(c.services),
            ]
        })
        .collect();
    (columns, rows)
}

/// A window in seconds as a short human span, for the summary lines.
///
/// Whole units only, largest that divides evenly, so `2592000` reads
/// `30d` and `100000` stays `100000s`. The seconds are printed beside it,
/// because they are the number the wire and the server logs carry.
fn human_secs(secs: u64) -> String {
    for (unit, size) in [("w", 604_800u64), ("d", 86_400), ("h", 3600), ("m", 60)] {
        if secs >= size && secs.is_multiple_of(size) {
            return format!("{}{unit}", secs / size);
        }
    }
    format!("{secs}s")
}

/// The lines printed above the candidate table: what was asked for, what
/// the retention floor did to it, and what the scan cost.
///
/// The CLI computes no window. All three numbers are the server's own, and
/// the floor line says whether it applied, because "I asked for 7d and got
/// 90d" is the surprise an operator has to be able to see.
fn gc_summary_lines(resp: &trawl_client::GcPinsResponse) -> Vec<String> {
    let mut lines = vec![
        format!(
            "pin gc ({}) at {}",
            if resp.dry_run { "dry run" } else { "executed" },
            resp.decided_at
        ),
        format!(
            "requested window: {} ({}s)",
            human_secs(resp.requested_older_than_secs),
            resp.requested_older_than_secs
        ),
    ];
    lines.push(match resp.retention_floor_secs {
        Some(floor) if floor > resp.requested_older_than_secs => format!(
            "retention floor:  {} ({}s), raised the window: a pin cannot be \
             called dead over a span shorter than the corpus trawl still keeps",
            human_secs(floor),
            floor
        ),
        Some(floor) => format!(
            "retention floor:  {} ({}s), did not apply",
            human_secs(floor),
            floor
        ),
        None => "retention floor:  none (the global or a per-env max_age_days is 0)".to_owned(),
    });
    lines.push(format!(
        "effective window: {} ({}s)",
        human_secs(resp.effective_older_than_secs),
        resp.effective_older_than_secs
    ));
    lines.push(format!(
        "examined {} pin(s) past the window, read {} parquet footer(s)",
        resp.pins_examined, resp.files_scanned
    ));
    lines
}

/// The line printed below the candidate table.
fn gc_outcome_line(resp: &trawl_client::GcPinsResponse) -> String {
    if resp.dry_run {
        format!(
            "{} pin(s) would be reclaimed; nothing was deleted (re-run without \
             --dry-run)",
            resp.candidates.len()
        )
    } else {
        format!("{} pin(s) reclaimed", resp.deleted)
    }
}

/// `trawl schema gc-pins [--dry-run] [--older-than <dur>]`.
///
/// The candidate list is the one table; the summary lines go around it
/// through [`label`], so they reach stdout for a human and stderr under a
/// machine format. A csv or ndjson run is therefore a single rectangular
/// record set with no second header in the middle of it.
///
/// No confirmation prompt, unlike `repin`: this deletes catalog metadata
/// only, and a field wrongly reclaimed re-pins cleanly the next time a
/// sender writes it. `--dry-run` is the safety.
pub async fn run_gc_pins<W: Write>(
    out: &mut W,
    conn: ConnectionParams,
    dry_run: bool,
    older_than: Option<&str>,
    format: Option<OutputFormat>,
) -> Result<(), CliError> {
    let format = resolve_format(format)?;
    let older_than_secs = older_than
        .map(|s| parse_window(s, "--older-than"))
        .transpose()?;
    let client = make_client(&conn)?;
    // A refusal (409 for a repin owning the data root or an unreadable
    // corpus) arrives as a client error carrying the server's own message,
    // and `main` prints it and exits non-zero.
    let resp = client.schema_gc_pins(dry_run, older_than_secs).await?;

    let human = format == OutputFormat::Table;
    for line in gc_summary_lines(&resp) {
        label(out, human, &line)?;
    }
    let (columns, rows) = gc_candidates_to_rows(&resp);
    render(out, &columns, &rows, format)?;
    label(out, human, &gc_outcome_line(&resp))?;
    Ok(())
}

#[cfg(test)]
mod gc_tests {
    use super::*;

    fn sample_report(dry_run: bool) -> trawl_client::GcPinsResponse {
        trawl_client::GcPinsResponse {
            dry_run,
            decided_at: "2026-09-04T12:00:00Z".into(),
            requested_older_than_secs: 604_800,
            retention_floor_secs: Some(7_776_000),
            effective_older_than_secs: 7_776_000,
            pins_examined: 3,
            files_scanned: 12,
            candidates: vec![
                trawl_client::GcPinCandidate {
                    field: "retired_counter".into(),
                    data_type: "BIGINT".into(),
                    last_seen: Some("2026-01-02T03:04:05Z".into()),
                    services: 2,
                },
                trawl_client::GcPinCandidate {
                    field: "typo_feild".into(),
                    data_type: "VARCHAR".into(),
                    last_seen: None,
                    services: 0,
                },
            ],
            deleted: if dry_run { 0 } else { 2 },
        }
    }

    #[test]
    fn candidate_rows_carry_every_column() {
        let (columns, rows) = gc_candidates_to_rows(&sample_report(true));
        assert_eq!(columns, ["field", "type", "last_seen", "services"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Json::from("retired_counter"));
        assert_eq!(rows[0][3], Json::from(2));
        // A never-observed pin is a candidate with a null observation, not
        // a row the renderer drops.
        assert_eq!(rows[1][2], Json::Null);
    }

    #[test]
    fn human_secs_uses_whole_units_only() {
        assert_eq!(human_secs(2_592_000), "30d");
        assert_eq!(human_secs(604_800), "1w");
        assert_eq!(human_secs(7200), "2h");
        assert_eq!(human_secs(90), "90s");
        assert_eq!(human_secs(0), "0s");
    }

    #[test]
    fn older_than_takes_the_last_window_grammar() {
        assert_eq!(parse_last("30d").unwrap(), 2_592_000);
        assert_eq!(parse_last("12w").unwrap(), 7_257_600);
        assert_eq!(parse_last("0s").unwrap(), 0);
        for bad in ["30", "d", "7µ", "-1d", "30y", ""] {
            assert!(
                matches!(parse_last(bad), Err(CliError::Usage(_))),
                "{bad:?} must be a usage error"
            );
        }
    }

    /// A csv run is one rectangle: exactly one header line, and every data
    /// line carries the same field count. The summary rides stderr, which
    /// is why a second header cannot appear here.
    #[test]
    fn csv_output_is_one_rectangle() {
        let (columns, rows) = gc_candidates_to_rows(&sample_report(false));
        let mut out = Vec::new();
        render(&mut out, &columns, &rows, OutputFormat::Csv).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 3, "one header + two rows: {text}");
        assert_eq!(lines[0], "field,type,last_seen,services");
        for line in &lines {
            assert_eq!(line.matches(',').count(), 3, "ragged row: {line}");
        }
    }

    #[test]
    fn table_output_carries_the_window_provenance_and_the_count() {
        let report = sample_report(true);
        let summary = gc_summary_lines(&report).join("\n");
        assert!(
            summary.contains("requested window: 1w (604800s)"),
            "{summary}"
        );
        assert!(
            summary.contains("retention floor:  90d (7776000s), raised the window"),
            "{summary}"
        );
        assert!(
            summary.contains("effective window: 90d (7776000s)"),
            "{summary}"
        );
        assert!(summary.contains("read 12 parquet footer(s)"), "{summary}");
        assert_eq!(
            gc_outcome_line(&report),
            "2 pin(s) would be reclaimed; nothing was deleted (re-run without \
             --dry-run)"
        );

        let (columns, rows) = gc_candidates_to_rows(&report);
        let mut out = Vec::new();
        render(&mut out, &columns, &rows, OutputFormat::Table).unwrap();
        let table = String::from_utf8(out).unwrap();
        assert!(table.contains("retired_counter"), "{table}");
        assert!(table.contains("typo_feild"), "{table}");
    }

    /// The two floor cases the summary must tell apart, plus the one where
    /// there is no floor at all.
    #[test]
    fn the_floor_line_says_whether_it_applied() {
        let mut report = sample_report(false);
        assert!(gc_summary_lines(&report)[2].contains("raised the window"));

        report.requested_older_than_secs = 15_552_000;
        report.effective_older_than_secs = 15_552_000;
        assert!(gc_summary_lines(&report)[2].contains("did not apply"));

        report.retention_floor_secs = None;
        assert!(gc_summary_lines(&report)[2].contains("max_age_days is 0"));
    }

    #[test]
    fn an_executed_run_reports_the_servers_deleted_count() {
        let report = sample_report(false);
        assert_eq!(gc_outcome_line(&report), "2 pin(s) reclaimed");
        assert!(gc_summary_lines(&report)[0].contains("executed"));
    }

    #[test]
    fn json_output_is_one_record_per_candidate() {
        let (columns, rows) = gc_candidates_to_rows(&sample_report(false));
        let mut out = Vec::new();
        render(&mut out, &columns, &rows, OutputFormat::Json).unwrap();
        let text = String::from_utf8(out).unwrap();
        let records: Vec<Json> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("each line is a JSON object"))
            .collect();
        assert_eq!(records.len(), 2, "{text}");
        assert_eq!(records[0]["field"], Json::from("retired_counter"));
        assert_eq!(records[1]["last_seen"], Json::Null);
    }
}
