// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl schema` subcommands: catalog read commands (#51).
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
    let input = input.trim();
    // Split off the last CHAR, not the last byte: `split_at` panics off a
    // char boundary, and the unit position is exactly where a multi-byte
    // char lands (`--last 7µ` must be a usage error, not a crash).
    let (num, unit) = match input.char_indices().next_back() {
        Some((idx, _)) => input.split_at(idx),
        None => ("", ""),
    };
    let n: u64 = num
        .parse()
        .map_err(|_| CliError::Usage(format!("invalid --last window: {input:?}")))?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => {
            return Err(CliError::Usage(format!(
                "invalid --last window: {input:?} (units: s, m, h, d, w)"
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
    eprintln!(
        "{}/{} pins used{}",
        resp.pinned_total,
        resp.pin_capacity,
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
    /// server and no postgres (the retained-DESCRIBE acceptance evidence).
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

// -- repin (ADR-0011 slice B) -------------------------------------------------

/// The `trawl schema repin` flag bundle.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)] // four independent CLI switches
pub struct RepinFlags {
    /// Scan and report only.
    pub dry_run: bool,
    /// Accept a lossy projection / run a resurrection-only pass.
    pub force: bool,
    /// Skip the interactive confirmation.
    pub yes: bool,
    /// Poll the job to completion.
    pub wait: bool,
}

/// One repin job → generic key/value (columns, rows) for the driver
/// formatter — the job is a single record, so it renders as one row.
pub fn repin_job_to_rows(job: &trawl_client::RepinJobResponse) -> (Vec<String>, Vec<Vec<Json>>) {
    let columns = [
        "id",
        "field",
        "from",
        "to",
        "status",
        "files_total",
        "files_done",
        "rows_carrying",
        "projected_nulls",
        "resurrectable",
        "rows_rewritten",
        "rows_nulled",
        "rows_resurrected",
        "error",
    ]
    .map(str::to_owned)
    .to_vec();
    let rows = vec![vec![
        Json::from(job.id),
        Json::from(job.field.clone()),
        Json::from(job.from_type.clone()),
        Json::from(job.to_type.clone()),
        Json::from(job.status.clone()),
        Json::from(job.files_total),
        Json::from(job.files_done),
        Json::from(job.rows_carrying),
        Json::from(job.projected_nulls),
        Json::from(job.resurrectable),
        Json::from(job.rows_rewritten),
        Json::from(job.rows_nulled),
        Json::from(job.rows_resurrected),
        job.error.clone().map_or(Json::Null, Json::from),
    ]];
    (columns, rows)
}

/// `trawl schema repin <field> --to <type>`.
///
/// An executing repin rewrites the archive, so it confirms interactively —
/// and off a TTY it REFUSES without `--yes` rather than assuming (a piped
/// or scripted invocation must state its intent). Dry runs never prompt.
pub async fn run_repin(
    out: &mut impl Write,
    conn: ConnectionParams,
    field: &str,
    to: &str,
    flags: RepinFlags,
    format: Option<OutputFormat>,
) -> Result<(), CliError> {
    let format = resolve_format(format)?;
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
    let outcome = client
        .schema_repin(field, to, flags.dry_run, flags.force)
        .await?;
    let (verdict, job) = match outcome {
        trawl_client::RepinStart::Report(job) => ("dry run", job),
        trawl_client::RepinStart::Started(job) => ("started", job),
        trawl_client::RepinStart::Refused(job) => ("refused: needs --force", job),
    };

    let mut job = job;
    if flags.wait && job.status == "running" {
        job = wait_for_terminal(&client, job).await?;
    }
    // Computed AFTER the wait: a started job can still refuse at the
    // cutover gate when data ingested after the scan turns out to be
    // unreadable under the new type.
    let refused = job.status == "refused_needs_force";

    if format == OutputFormat::Table {
        writeln!(out, "repin {}: {verdict}", job.field)?;
    }
    let (columns, rows) = repin_job_to_rows(&job);
    render_driver_results(&columns, &rows, format, out)?;
    if refused {
        // A pre-scan refusal reports its projection; a cutover refusal
        // reports what the finished rewrite actually nulled.
        let lost = if job.rows_nulled > 0 {
            job.rows_nulled
        } else {
            job.projected_nulls
        };
        return Err(CliError::Usage(format!(
            "repin would null {lost} stored value(s); re-run with --force to \
             accept the loss (originals remain findable in _raw)"
        )));
    }
    Ok(())
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
        match status.job {
            Some(j) if j.id == id => latest = j,
            // A different (or no) job means ours got reconciled away by a
            // restart; report what we last saw.
            _ => break,
        }
    }
    Ok(latest)
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
            let (columns, rows) = repin_job_to_rows(&job);
            render_driver_results(&columns, &rows, format, out)?;
        }
        None => writeln!(out, "no repin job has ever run")?,
    }
    Ok(())
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
        }
    }

    /// The job renders through the shared driver formatter in every
    /// format the schema family honours.
    #[test]
    fn repin_job_renders_in_table_json_and_csv() {
        let (columns, rows) = repin_job_to_rows(&sample_job());
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

    /// An executing repin off a TTY refuses without `--yes` BEFORE any
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
                dry_run: false,
                force: false,
                yes: false,
                wait: false,
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
}
