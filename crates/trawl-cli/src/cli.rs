// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CLI mode: query execution, validation, and output formatting.

use std::io::{self, IsTerminal, Write};
use std::path::Path;

use clap::ValueEnum;
use trawl_core::sanitize::sanitize_display_text;
use trawl_core::text_layout::{caret_layout, locate_span};
use trawl_engine::value::{QueryResult, Value};

use crate::CliError;

/// Output format for CLI query results.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
    Csv,
    Parquet,
}

/// Which dialect a `SEVERITY` repin reads the corpus's numerals in: the
/// same closed vocabulary the DSL's `sev()` and the `[ingest]` derivation
/// config take.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum SeverityDialect {
    /// `OTel` `SeverityNumber`: 1-24, counting up.
    Otel,
    /// Syslog PRI severity: 0-7, counting down.
    Syslog,
}

impl SeverityDialect {
    /// The wire token — the one spelling the server parses.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Otel => "otel",
            Self::Syslog => "syslog",
        }
    }
}

/// Resolved connection parameters (after config + env + CLI override merge).
#[derive(Clone)]
pub struct ConnectionParams {
    pub url: String,
    pub token: String,
    pub trust: trawl_client::TlsTrust,
}

impl ConnectionParams {
    /// Build the `HttpClient` for these params. Every command and the TUI
    /// connect through here, so the trust rules have one home.
    pub fn client(&self) -> Result<trawl_client::HttpClient, CliError> {
        Ok(trawl_client::HttpClient::with_trust(
            &self.url,
            &self.token,
            &self.trust,
        )?)
    }
}

/// Hand-written so the API token never reaches a log line or a panic message.
impl std::fmt::Debug for ConnectionParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionParams")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .field("trust", &self.trust)
            .finish()
    }
}

/// Execute a query and print the results.
pub async fn run_query(
    query: &str,
    data: Option<&str>,
    format: Option<OutputFormat>,
    output: Option<&Path>,
    conn: Option<ConnectionParams>,
    timezone: &str,
) -> Result<(), CliError> {
    let format = format.unwrap_or_else(|| {
        if io::stdout().is_terminal() {
            OutputFormat::Table
        } else {
            OutputFormat::Json
        }
    });

    // Parquet output requires -o flag (binary format can't go to stdout).
    if format == OutputFormat::Parquet && output.is_none() {
        return Err(CliError::Usage(
            "parquet output requires -o/--output flag".into(),
        ));
    }

    // Parquet export: use DuckDB's native COPY TO directly.
    if format == OutputFormat::Parquet {
        let output_path = output.expect("validated above");
        return run_parquet_export(query, data, conn.as_ref(), output_path).await;
    }

    // Block `from saved` in embedded mode: resolving a saved query's name
    // to parquet paths needs the server's saved-query and report-run stores.
    if data.is_some()
        && let Ok(ast) = trawl_core::parser::parse(query)
        && ast.from_saved_stage().is_some()
    {
        return Err(CliError::Usage(
            "\"from saved\" requires a server connection and cannot be used with --data".into(),
        ));
    }

    let (result, degraded, severity_columns) = if let Some(data) = data {
        match run_embedded_mode(data, query, timezone) {
            // Embedded mode has no catalog, so no degraded notice to
            // carry; `sev()` declares its own pin, so its columns still
            // render as tokens without one.
            Ok(r) => (r, Vec::new(), embedded_severity_columns(query)),
            Err(CliError::Engine(ref engine_err)) => {
                render_engine_error(query, engine_err);
                return Err(CliError::Usage("query failed".into()));
            }
            Err(e) => return Err(e),
        }
    } else if let Some(conn) = conn {
        match run_daemon_mode(&conn, query, timezone).await {
            Ok(r) => r,
            Err(CliError::Client(ref client_err)) => {
                render_client_error(query, client_err);
                return Err(CliError::Usage("query failed".into()));
            }
            Err(e) => return Err(e),
        }
    } else {
        return Err(CliError::Usage(
            "provide --url (daemon mode) or --data (embedded mode)".into(),
        ));
    };

    let stdout = io::stdout();
    emit_results(
        &result,
        format,
        output,
        &degraded,
        &severity_columns,
        &mut stdout.lock(),
        &mut io::stderr(),
    )
}

/// The severity-rendering columns of an embedded query.
///
/// The same walk the server runs, from a pin-blind root: with no catalog
/// only a function-declared pin survives it, which is exactly `sev()`. An
/// unparseable query renders nothing special; the engine reports the parse
/// error.
fn embedded_severity_columns(query: &str) -> Vec<String> {
    trawl_core::parser::parse(query).map_or_else(
        |_| Vec::new(),
        |ast| {
            trawl_core::pin_scope::severity_output_columns(
                &ast.pipeline,
                &trawl_core::pin_scope::PinScope::unpinned(),
            )
        },
    )
}

/// Write the results and their footer to the two streams.
///
/// The stream split is the contract: with `-o`, the file is the
/// deliverable and the footer goes to `err` so it can never
/// contaminate it; without it, results and footer share `out`.
fn emit_results(
    result: &QueryResult,
    format: OutputFormat,
    output: Option<&Path>,
    degraded: &[String],
    severity_columns: &[String],
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<(), CliError> {
    if let Some(output_path) = output {
        let mut file = std::fs::File::create(output_path)?;
        render_results(result, format, severity_columns, &mut file)?;
        // The file is the deliverable; the notice belongs on the terminal.
        write_degraded_footer(err, format, degraded)?;
    } else {
        render_results(result, format, severity_columns, out)?;
        write_degraded_footer(out, format, degraded)?;
    }

    Ok(())
}

/// Render the result rows in the requested format.
fn render_results(
    result: &QueryResult,
    format: OutputFormat,
    severity_columns: &[String],
    out: &mut impl Write,
) -> io::Result<()> {
    match format {
        OutputFormat::Table => render_table(result, severity_columns, out),
        OutputFormat::Json => render_ndjson(result, out),
        OutputFormat::Csv => render_csv(result, out),
        OutputFormat::Parquet => unreachable!("handled above"),
    }
}

/// The incomplete-results footer (ADR-0011): one line after the row count
/// when the query bound a field whose pin is shelving values.
///
/// Table output only. json/csv/parquet carry `degraded_fields` on the wire,
/// which is the notice for a machine, and a prose line inside a machine
/// format is a parse error waiting to happen. One sentence, no advice beyond
/// where to look: the case file holds the evidence and the remedy.
fn write_degraded_footer(
    out: &mut impl Write,
    format: OutputFormat,
    fields: &[String],
) -> io::Result<()> {
    if format != OutputFormat::Table || fields.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "note: results may be incomplete — degraded field(s): {} \
         (see: trawl schema field {})",
        fields.join(", "),
        fields[0]
    )
}

/// Validate a DSL query.
///
/// If connection params are provided, validates via the server (checks syntax,
/// semantics, function arity, regex patterns). Otherwise, validates locally
/// (parse-only via `trawl_core`).
pub async fn run_validate(query: &str, conn: Option<ConnectionParams>) -> Result<(), CliError> {
    if let Some(conn) = conn {
        // Server-side validation (richer checks).
        let client = conn.client()?;
        let response = client.validate(query).await?;
        if response.valid {
            write_valid(&mut std::io::stdout().lock(), &response)?;
        } else {
            render_error_details(query, &response.errors);
            return Err(CliError::Usage("query validation failed".into()));
        }
    } else {
        // Local parse-only validation.
        match trawl_core::parser::parse(query) {
            Ok(_) => println!("valid"),
            Err(errors) => {
                let details: Vec<_> = errors.iter().map(parse_error_to_detail).collect();
                render_error_details(query, &details);
                return Err(CliError::Usage("query validation failed".into()));
            }
        }
    }

    Ok(())
}

/// Print a valid server verdict: `valid`, then why the server returned no
/// formatted text when it refused to format. A refusal leaves the query
/// valid, so it is a note, not an error (ADR-0061).
fn write_valid(
    out: &mut impl std::io::Write,
    response: &trawl_client::ValidationResponse,
) -> std::io::Result<()> {
    writeln!(out, "valid")?;
    if let Some(refusal) = &response.format_error {
        writeln!(out, "not formatted: {}", refusal.message)?;
    }
    Ok(())
}

/// Export query results as parquet (embedded or daemon mode).
async fn run_parquet_export(
    query: &str,
    data: Option<&str>,
    conn: Option<&ConnectionParams>,
    output_path: &Path,
) -> Result<(), CliError> {
    if let Some(data) = data {
        // Embedded mode: export directly via DuckDB.
        let executor = trawl_engine::executor::Executor::new()?;
        // Embedded mode is pin-blind by design (no catalog, ADR-0011):
        // the explicit empty set keeps that decision visible.
        executor.export_parquet(
            query,
            data,
            &trawl_core::schema::FieldTypes::new(),
            output_path,
            usize::MAX,
        )?;
    } else if let Some(conn) = conn {
        // Daemon mode: fetch parquet bytes via HTTP export endpoint.
        let client = conn.client()?;
        let bytes = client
            .export(query, trawl_client::ExportFormat::Parquet, None)
            .await?;
        std::fs::write(output_path, &bytes)?;
    } else {
        return Err(CliError::Usage(
            "provide --url (daemon mode) or --data (embedded mode)".into(),
        ));
    }
    Ok(())
}

/// Connect to the daemon and execute the query over HTTPS.
async fn run_daemon_mode(
    conn: &ConnectionParams,
    query: &str,
    timezone: &str,
) -> Result<(QueryResult, Vec<String>, Vec<String>), CliError> {
    let client = conn.client()?;
    let response = client
        .query_paginated_tz(query, None, None, Some(timezone.to_owned()))
        .await?;
    Ok(daemon_outcome(response))
}

/// Split a daemon response into what the printer needs: the rows, the
/// degraded-field note (ADR-0011) and the severity columns. Both advisory
/// channels are carried, never dropped.
fn daemon_outcome(
    response: trawl_client::QueryResponse,
) -> (QueryResult, Vec<String>, Vec<String>) {
    (
        response.result,
        response.degraded_fields,
        response.severity_columns,
    )
}

/// Execute the query locally with an embedded `DuckDB` engine.
fn run_embedded_mode(data: &str, query: &str, timezone: &str) -> Result<QueryResult, CliError> {
    let utc_offset_secs =
        trawl_engine::timezone::resolve_utc_offset(timezone).map_err(CliError::Usage)?;
    let executor = trawl_engine::executor::Executor::new()?;
    // CLI has no server-side row limit, hence usize::MAX. Embedded mode is
    // pin-blind by design (no catalog, ADR-0011): the explicit empty set
    // keeps that decision visible.
    Ok(executor.run_query(
        query,
        data,
        &trawl_core::schema::FieldTypes::new(),
        usize::MAX,
        utc_offset_secs,
    )?)
}

// -- output formatters -------------------------------------------------------

fn render_table(
    result: &QueryResult,
    severity_columns: &[String],
    out: &mut impl Write,
) -> io::Result<()> {
    if result.is_empty() {
        writeln!(out, "no results")?;
        return Ok(());
    }

    let mut table = comfy_table::Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_FULL)
        .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS)
        .set_content_arrangement(comfy_table::ContentArrangement::Dynamic);

    let headers: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    table.set_header(headers);

    // A severity column displays its OTel token (ADR-0013 §6): `17` reads
    // `error`, the same vocabulary that would filter it. `_severity` by
    // name, plus whatever the response declared (a `sev()` output). Only
    // the table renders it; json/csv keep the number for arithmetic
    // consumers.
    let severity_cells: Vec<bool> = result
        .columns
        .iter()
        .map(|c| trawl_core::severity::renders_as_severity(&c.name, severity_columns))
        .collect();

    for row in &result.rows {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, v)| {
                if severity_cells.get(i).copied().unwrap_or(false) {
                    severity_cell_text(v)
                } else {
                    // A timestamp shows the wall clock the server rendered
                    // in the reader's offset, without the wire's `T` and
                    // offset.
                    result
                        .columns
                        .get(i)
                        .and_then(|c| trawl_api::display::timestamp_cell_text(&c.ty, v))
                        .unwrap_or_else(|| v.to_string())
                }
            })
            .collect();
        table.add_row(cells);
    }

    writeln!(out, "{table}")?;
    writeln!(out, "{} row(s)", result.row_count())?;
    Ok(())
}

/// The `OTel` token a `_severity` cell displays, or `None` where the
/// ladder has no reading for it and the surface renders the value the
/// way it renders any other.
///
/// The one in-crate door onto `trawl_core::severity::token_text` — the
/// table renderer here and the TUI results grid share the rule but not
/// their fallback formatting.
///
/// An unsigned past `i64::MAX` reads like every other shape the ladder
/// has no rung for: no token, and the cell shows its digits. Every
/// dialect's ladder ends well inside `i64`.
pub(crate) fn severity_token(v: &Value) -> Option<&'static str> {
    match v {
        Value::Integer(n) => trawl_core::severity::token_text(*n),
        _ => None,
    }
}

/// A `_severity` cell as the table shows it.
fn severity_cell_text(v: &Value) -> String {
    severity_token(v).map_or_else(|| v.to_string(), str::to_owned)
}

fn render_ndjson(result: &QueryResult, out: &mut impl Write) -> io::Result<()> {
    for row in &result.rows {
        let mut map = serde_json::Map::new();
        for (col, val) in result.columns.iter().zip(row.iter()) {
            // An export file, not the wire: integers stay JSON numbers
            // (ADR-0035, amended 2026-10-22).
            map.insert(col.name.clone(), val.to_plain_json());
        }
        serde_json::to_writer(&mut *out, &map).map_err(io::Error::other)?;
        writeln!(out)?;
    }
    Ok(())
}

fn render_csv(result: &QueryResult, out: &mut impl Write) -> io::Result<()> {
    let headers: Vec<String> = result
        .columns
        .iter()
        .map(|c| csv_escape_string(&c.name))
        .collect();
    writeln!(out, "{}", headers.join(","))?;

    for row in &result.rows {
        let cells: Vec<String> = row.iter().map(csv_escape_value).collect();
        writeln!(out, "{}", cells.join(","))?;
    }
    Ok(())
}

/// Escape a value for CSV output, applying formula injection protection
/// only to string values (numeric types are inherently safe).
fn csv_escape_value(val: &Value) -> String {
    match val {
        Value::Null => String::new(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        // full precision for data fidelity (TUI display uses 2dp for readability)
        Value::Float(f) => f.to_string(),
        Value::String(s) => csv_escape_string(s),
        Value::Array(_) => csv_escape_string(&val.to_plain_json().to_string()),
    }
}

/// Escape a string for CSV, preventing formula injection and quoting
/// as needed for commas, quotes, and newlines.
fn csv_escape_string(s: &str) -> String {
    // Prevent CSV injection: prefix formula-triggering characters with a
    // single quote so spreadsheet apps don't interpret cells as formulas.
    let s = if s.starts_with(['=', '+', '-', '@', '\t', '|']) {
        format!("'{s}")
    } else {
        s.to_owned()
    };

    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

// -- error rendering ----------------------------------------------------------

/// Convert a `ParseError` to an `ErrorDetail` for display.
fn parse_error_to_detail(e: &trawl_core::parser::ParseError) -> trawl_client::ErrorDetail {
    trawl_client::ErrorDetail {
        message: e.message.clone(),
        span: Some(trawl_client::ErrorSpan {
            start: e.span.start,
            end: e.span.end,
        }),
        label: e.label.clone(),
        hint: e.hint.clone(),
    }
}

/// Render structured error details with rustc-style caret underlines to stderr.
///
/// For each detail with a span, shows the query text with the error region
/// underlined:
/// ```text
///   _severity=error | staats count() by host
///                     ~~~~~~
///   unknown command 'staats'
///   hint: did you mean 'stats'?
/// ```
pub fn render_error_details(query: &str, details: &[trawl_client::ErrorDetail]) {
    // stderr is the last place to report anything; a failed write has
    // nowhere to go.
    let _ = write_error_details(&mut io::stderr().lock(), query, details);
}

/// Write `details` against `query`, the text that was sent.
///
/// A span is located with [`locate_span`]. One that cannot index `query`
/// (reversed, past the end, or inside a character) gets the message and
/// hint without an excerpt; it is never clamped. A located span shows the
/// line holding its start, sanitized, and carets under the span's display
/// cells. A span running onto a later line marks only its start line.
/// The message and hint are sanitized too: they reach the terminal.
fn write_error_details(
    out: &mut impl Write,
    query: &str,
    details: &[trawl_client::ErrorDetail],
) -> io::Result<()> {
    for detail in details {
        let located = detail
            .span
            .as_ref()
            .and_then(|span| locate_span(query, span.start, span.end));
        if let Some(at) = located {
            let layout = caret_layout(query, &at);
            writeln!(out)?;
            writeln!(out, "  {}{}{}", layout.before, layout.covered, layout.after)?;
            writeln!(
                out,
                "  {}{}",
                " ".repeat(layout.pad),
                "~".repeat(layout.carets)
            )?;
        }
        writeln!(out, "  {}", sanitize_display_text(&detail.message))?;
        if let Some(hint) = &detail.hint {
            writeln!(out, "  hint: {}", sanitize_display_text(hint))?;
        }
        writeln!(out)?;
    }
    Ok(())
}

/// Render a `ClientError` with span details (if available) for CLI output.
pub fn render_client_error(query: &str, err: &trawl_client::ClientError) {
    // stderr is the last place to report anything; a failed write has
    // nowhere to go.
    let _ = write_client_error(&mut io::stderr().lock(), query, err);
}

/// Write `err` for the query `query`, the text that was sent.
///
/// The summary line is sanitized like the details: the server's message
/// and the error around it reach the terminal.
fn write_client_error(
    out: &mut impl Write,
    query: &str,
    err: &trawl_client::ClientError,
) -> io::Result<()> {
    let details = err.error_details();
    // A validation error's one detail has no span and repeats the summary
    // the error already prints, so only a span earns the detailed form.
    if details.iter().any(|d| d.span.is_some()) {
        if let Some(envelope) = err.error_envelope() {
            writeln!(
                out,
                "trawl: {:?}: {}",
                envelope.code,
                sanitize_display_text(&envelope.message)
            )?;
        }
        write_error_details(out, query, details)
    } else {
        writeln!(out, "trawl: {}", sanitize_display_text(&err.to_string()))
    }
}

/// Render an `EngineError` with span details for embedded mode.
pub fn render_engine_error(query: &str, err: &trawl_engine::error::EngineError) {
    match err {
        trawl_engine::error::EngineError::Parse(errors) => {
            eprintln!("trawl: parse error");
            let details: Vec<_> = errors.iter().map(parse_error_to_detail).collect();
            render_error_details(query, &details);
        }
        other => eprintln!("trawl: {other}"),
    }
}

// -- driver output formatters -------------------------------------------------

/// Render driver result data (columns + JSON rows) in the requested format.
/// Used by `trawl driver query`, `trawl driver get-results` and the
/// `trawl schema` tables.
pub fn render_driver_results(
    columns: &[String],
    rows: &[Vec<serde_json::Value>],
    format: OutputFormat,
    out: &mut impl Write,
) -> io::Result<()> {
    match format {
        OutputFormat::Table => render_driver_table(columns, rows, out),
        OutputFormat::Json => render_driver_ndjson(columns, rows, out),
        OutputFormat::Csv => render_driver_csv(columns, rows, out),
        OutputFormat::Parquet => {
            writeln!(
                out,
                "parquet format is not supported for driver output, use -o with trawl query instead"
            )
        }
    }
}

fn render_driver_table(
    columns: &[String],
    rows: &[Vec<serde_json::Value>],
    out: &mut impl Write,
) -> io::Result<()> {
    if rows.is_empty() {
        writeln!(out, "no results")?;
        return Ok(());
    }

    let mut table = comfy_table::Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_FULL)
        .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS)
        .set_content_arrangement(comfy_table::ContentArrangement::Dynamic);

    let headers: Vec<&str> = columns.iter().map(String::as_str).collect();
    table.set_header(headers);

    for row in rows {
        let cells: Vec<String> = row.iter().map(json_display).collect();
        table.add_row(cells);
    }

    writeln!(out, "{table}")?;
    writeln!(out, "{} row(s)", rows.len())?;
    Ok(())
}

fn render_driver_ndjson(
    columns: &[String],
    rows: &[Vec<serde_json::Value>],
    out: &mut impl Write,
) -> io::Result<()> {
    for row in rows {
        let mut map = serde_json::Map::new();
        for (col, val) in columns.iter().zip(row.iter()) {
            map.insert(col.clone(), val.clone());
        }
        serde_json::to_writer(&mut *out, &map).map_err(io::Error::other)?;
        writeln!(out)?;
    }
    Ok(())
}

fn render_driver_csv(
    columns: &[String],
    rows: &[Vec<serde_json::Value>],
    out: &mut impl Write,
) -> io::Result<()> {
    let headers: Vec<String> = columns.iter().map(|c| csv_escape_string(c)).collect();
    writeln!(out, "{}", headers.join(","))?;

    for row in rows {
        let cells: Vec<String> = row.iter().map(csv_escape_json).collect();
        writeln!(out, "{}", cells.join(","))?;
    }
    Ok(())
}

/// Display a JSON value as a human-readable string (for table cells).
fn json_display(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "null".to_owned(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        // Arrays/objects: compact JSON representation.
        other => other.to_string(),
    }
}

/// CSV-escape a JSON value, applying formula injection protection to strings.
fn csv_escape_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => csv_escape_string(s),
        other => csv_escape_string(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A format refusal prints under `valid` and is not an error.
    #[test]
    fn a_valid_query_the_server_would_not_format_says_why() {
        let render = |format_error| {
            let response = trawl_client::ValidationResponse {
                valid: true,
                errors: vec![],
                formatted: None,
                format_error,
            };
            let mut buf = Vec::new();
            write_valid(&mut buf, &response).unwrap();
            String::from_utf8(buf).unwrap()
        };

        assert_eq!(render(None), "valid\n");
        let refused = render(Some(trawl_client::types::FormatErrorDetail {
            kind: trawl_client::types::FormatErrorKind::TooLong,
            message: "the formatted query would be 80002 bytes".into(),
        }));
        assert_eq!(
            refused,
            "valid\nnot formatted: the formatted query would be 80002 bytes\n"
        );
    }

    /// The notice is a table-only footer: a machine format carries the
    /// wire field instead, and a prose line inside ndjson or CSV would
    /// corrupt it.
    #[test]
    fn the_degraded_footer_is_table_only() {
        let fields = vec!["duration".to_owned(), "status".to_owned()];
        let render = |format, fields: &[String]| {
            let mut buf = Vec::new();
            write_degraded_footer(&mut buf, format, fields).unwrap();
            String::from_utf8(buf).unwrap()
        };

        let footer = render(OutputFormat::Table, &fields);
        assert_eq!(
            footer.trim(),
            "note: results may be incomplete — degraded field(s): duration, status \
             (see: trawl schema field duration)"
        );
        assert!(render(OutputFormat::Json, &fields).is_empty());
        assert!(render(OutputFormat::Csv, &fields).is_empty());
        assert!(
            render(OutputFormat::Table, &[]).is_empty(),
            "a healthy query prints nothing at all"
        );
    }

    /// AC 2: integers past 2^53 reach the table with every digit. The
    /// response is decoded from its wire text, decimal strings under an
    /// `integer` column, so this is the path a daemon answer takes; a
    /// reader that went through `f64` would print 9007199254740992.
    #[test]
    fn the_table_prints_big_integers_from_the_wire_exactly() {
        let response: trawl_client::QueryResponse = serde_json::from_str(
            r#"{
                "columns": [{"name": "host", "type": "string"},
                            {"name": "id", "type": "integer"}],
                "rows": [["a", "9007199254740993"],
                         ["b", "9223372036854775808"],
                         ["c", "18446744073709551615"],
                         ["d", "-9223372036854775808"]],
                "pagination": {"limit": 100, "offset": 0, "returned": 4, "total": 4}
            }"#,
        )
        .expect("the typed wire decodes");
        let (result, degraded, severity) = daemon_outcome(response);
        assert_eq!(
            result
                .rows
                .iter()
                .map(|row| row[1].clone())
                .collect::<Vec<_>>(),
            vec![
                Value::Integer(9_007_199_254_740_993),
                Value::UInt(9_223_372_036_854_775_808),
                Value::UInt(u64::MAX),
                Value::Integer(i64::MIN),
            ]
        );

        let mut out = Vec::new();
        render_table(&result, &severity, &mut out).unwrap();
        let table = String::from_utf8(out).unwrap();
        for digits in [
            "9007199254740993",
            "9223372036854775808",
            "18446744073709551615",
            "-9223372036854775808",
        ] {
            assert!(table.contains(digits), "{digits} missing from:\n{table}");
        }
        assert!(!table.contains("9007199254740992"), "{table}");
        assert!(degraded.is_empty());
    }

    /// A mocked daemon response carrying the note channel beside one
    /// result row.
    fn mocked_response() -> trawl_client::QueryResponse {
        trawl_client::QueryResponse {
            execution: None,
            result: QueryResult {
                columns: vec![trawl_engine::value::Column::new(
                    "host",
                    trawl_engine::value::ColumnType::String,
                )],
                rows: vec![vec![Value::String("db1".to_owned())]],
            },
            pagination: trawl_client::PaginationMeta {
                limit: 100,
                offset: 0,
                returned: 1,
                total: 1,
            },
            degraded_fields: vec!["duration".to_owned()],
            severity_columns: Vec::new(),
        }
    }

    /// The daemon path carries the note channel off the wire and puts it
    /// on the right stream: with the results when stdout is the
    /// deliverable, on stderr when a file is.
    #[test]
    fn the_daemon_note_reaches_the_terminal_and_never_the_deliverable() {
        let (result, degraded, severity) = daemon_outcome(mocked_response());
        assert_eq!(degraded, ["duration"], "degraded_fields must survive");
        assert!(severity.is_empty());

        let emit = |format, output: Option<&Path>| {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            emit_results(
                &result, format, output, &degraded, &severity, &mut out, &mut err,
            )
            .unwrap();
            (
                String::from_utf8(out).unwrap(),
                String::from_utf8(err).unwrap(),
            )
        };

        // Table to stdout: the footer rides the result stream, stderr silent.
        let (out, err) = emit(OutputFormat::Table, None);
        assert!(out.contains("db1"), "{out}");
        assert!(out.contains("note: results may be incomplete"), "{out}");
        assert!(err.is_empty(), "{err}");

        // Table to a file: the file is the deliverable, the footer is
        // stderr's, and stdout stays silent.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let (out, err) = emit(OutputFormat::Table, Some(&path));
        assert!(out.is_empty(), "{out}");
        assert!(err.contains("note: results may be incomplete"), "{err}");
        let file = std::fs::read_to_string(&path).unwrap();
        assert!(file.contains("db1"), "{file}");
        assert!(!file.contains("note:"), "{file}");

        // Machine formats stay byte-clean: the wire field is the notice.
        for format in [OutputFormat::Json, OutputFormat::Csv] {
            let (out, err) = emit(format, None);
            assert!(!out.contains("note:"), "{out}");
            assert!(err.is_empty(), "{err}");
        }
        let (json, _) = emit(OutputFormat::Json, None);
        for line in json.lines() {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
        let (csv, _) = emit(OutputFormat::Csv, None);
        assert_eq!(csv, "host\ndb1\n");
    }

    /// The table renders the token; json and csv keep the number.
    #[test]
    fn the_severity_column_displays_its_token_in_the_table_only() {
        assert_eq!(severity_cell_text(&Value::Integer(17)), "error");
        assert_eq!(severity_cell_text(&Value::Integer(18)), "error2");
        assert_eq!(severity_cell_text(&Value::Integer(99)), "99");

        let result = QueryResult {
            columns: vec![trawl_engine::value::Column::new(
                trawl_core::schema::SEVERITY,
                trawl_engine::value::ColumnType::Integer,
            )],
            rows: vec![vec![Value::Integer(17)]],
        };
        let render = |f: fn(&QueryResult, &mut Vec<u8>) -> io::Result<()>| {
            let mut buf = Vec::new();
            f(&result, &mut buf).unwrap();
            String::from_utf8(buf).unwrap()
        };
        let table = {
            let mut buf = Vec::new();
            render_table(&result, &[], &mut buf).unwrap();
            String::from_utf8(buf).unwrap()
        };
        assert!(table.contains("error"));
        assert!(!table.contains(" 17 "));
        assert!(render(render_ndjson).contains("17"));
        assert!(render(render_csv).contains("17"));
    }

    /// A `timestamp` column shows in the table exactly as the engine's old
    /// zoneless text did (the wall clock the server rendered in the
    /// reader's offset), while json and csv carry the wire's RFC 3339.
    #[test]
    fn the_table_shows_wire_timestamps_as_the_wall_clock() {
        use trawl_engine::value::{Column, ColumnType};
        let table = |result: &QueryResult| {
            let mut buf = Vec::new();
            render_table(result, &[], &mut buf).unwrap();
            String::from_utf8(buf).unwrap()
        };
        let wire = [
            "2026-01-15T10:00:00.000000+05:30",
            "2026-01-15T10:00:00.120000+05:30",
            "infinity",
        ];
        let shown = ["2026-01-15 10:00:00", "2026-01-15 10:00:00.12", "infinity"];
        let typed = QueryResult {
            columns: vec![
                Column::new("_time", ColumnType::Timestamp),
                Column::new("seen", ColumnType::list(ColumnType::Timestamp)),
            ],
            rows: wire
                .iter()
                .map(|t| {
                    vec![
                        Value::String((*t).into()),
                        Value::Array(vec![Value::String((*t).into())]),
                    ]
                })
                .collect(),
        };
        let before = QueryResult {
            columns: vec![
                Column::new("_time", ColumnType::String),
                Column::new("seen", ColumnType::list(ColumnType::String)),
            ],
            rows: shown
                .iter()
                .map(|t| {
                    vec![
                        Value::String((*t).into()),
                        Value::Array(vec![Value::String((*t).into())]),
                    ]
                })
                .collect(),
        };
        assert_eq!(table(&typed), table(&before));

        for f in [
            render_ndjson as fn(&QueryResult, &mut Vec<u8>) -> io::Result<()>,
            render_csv,
        ] {
            let mut buf = Vec::new();
            f(&typed, &mut buf).unwrap();
            let text = String::from_utf8(buf).unwrap();
            assert!(text.contains(wire[1]), "{text}");
        }
    }

    /// A declared severity column (`sev()`'s output under its own alias)
    /// renders its token in the table and its number everywhere a machine
    /// reads (ADR-0013).
    #[test]
    fn a_declared_severity_column_displays_its_token_in_the_table_only() {
        let result = QueryResult {
            columns: vec![
                trawl_engine::value::Column::new("s", trawl_engine::value::ColumnType::Integer),
                trawl_engine::value::Column::new("n", trawl_engine::value::ColumnType::Integer),
            ],
            rows: vec![vec![Value::Integer(18), Value::Integer(18)]],
        };
        let declared = vec!["s".to_owned()];

        let mut buf = Vec::new();
        render_table(&result, &declared, &mut buf).unwrap();
        let table = String::from_utf8(buf).unwrap();
        assert!(table.contains("error2"), "{table}");
        assert!(
            table.contains("18"),
            "the undeclared column keeps its number: {table}"
        );

        // Undeclared: no list, no tokens.
        let mut buf = Vec::new();
        render_table(&result, &[], &mut buf).unwrap();
        let plain = String::from_utf8(buf).unwrap();
        assert!(!plain.contains("error2"), "{plain}");

        // A mixed-case alias: the pin scope folds the name and the
        // result column keeps the spelling the user typed, so the match
        // folds too — `DuckDB` identifiers are case-insensitive.
        let mixed = QueryResult {
            columns: vec![trawl_engine::value::Column::new(
                "S",
                trawl_engine::value::ColumnType::Integer,
            )],
            rows: vec![vec![Value::Integer(18)]],
        };
        let mut buf = Vec::new();
        render_table(&mixed, &declared, &mut buf).unwrap();
        let table = String::from_utf8(buf).unwrap();
        assert!(table.contains("error2"), "mixed-case alias: {table}");

        // Machine formats carry the number in both columns.
        for f in [
            render_ndjson as fn(&QueryResult, &mut Vec<u8>) -> io::Result<()>,
            render_csv,
        ] {
            let mut buf = Vec::new();
            f(&result, &mut buf).unwrap();
            let text = String::from_utf8(buf).unwrap();
            assert!(!text.contains("error2"), "{text}");
        }
    }

    /// The embedded lane names the same columns the server would: the pin
    /// is the function's, so it survives a catalog-less root.
    #[test]
    fn the_embedded_lane_declares_sev_columns_without_a_catalog() {
        assert_eq!(embedded_severity_columns("* | let s = sev(level)"), ["s"]);
        assert_eq!(
            embedded_severity_columns("* | let s = sev(level) | stats count() by s"),
            ["s"]
        );
        assert!(embedded_severity_columns("* | let s = lower(level)").is_empty());
        assert!(embedded_severity_columns("this is ) not a query").is_empty());
    }

    #[test]
    fn csv_negative_number_not_prefixed() {
        assert_eq!(csv_escape_value(&Value::Integer(-42)), "-42");
    }

    #[test]
    fn csv_negative_float_not_prefixed() {
        assert_eq!(csv_escape_value(&Value::Float(-1.5)), "-1.5");
    }

    #[test]
    fn csv_formula_string_prefixed() {
        assert_eq!(csv_escape_value(&Value::String("=cmd".into())), "'=cmd");
    }

    #[test]
    fn csv_tab_prefixed() {
        assert_eq!(csv_escape_value(&Value::String("\tfoo".into())), "'\tfoo");
    }

    #[test]
    fn csv_at_sign_prefixed() {
        assert_eq!(csv_escape_value(&Value::String("@sum".into())), "'@sum");
    }

    #[test]
    fn csv_pipe_prefixed() {
        assert_eq!(csv_escape_value(&Value::String("|cmd".into())), "'|cmd");
    }

    #[test]
    fn csv_null_empty() {
        assert_eq!(csv_escape_value(&Value::Null), "");
    }

    #[test]
    fn csv_string_with_comma() {
        assert_eq!(csv_escape_value(&Value::String("a,b".into())), "\"a,b\"");
    }

    #[test]
    fn csv_string_with_quotes() {
        assert_eq!(
            csv_escape_value(&Value::String(r#"say "hi""#.into())),
            r#""say ""hi""""#
        );
    }

    #[tokio::test]
    async fn parquet_format_without_output_flag_errors() {
        let result = run_query(
            "*",
            Some("dummy.parquet"),
            Some(OutputFormat::Parquet),
            None,
            None,
            "UTC",
        )
        .await;
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("parquet output requires -o"),
            "expected -o flag error, got: {err}"
        );
    }

    #[test]
    fn embedded_data_glob_still_selects_local_parquet_files() {
        let dir = tempfile::tempdir().unwrap();
        let executor = trawl_engine::executor::Executor::new().unwrap();
        let source = QueryResult {
            columns: vec![trawl_engine::value::Column::new(
                "message",
                trawl_engine::value::ColumnType::String,
            )],
            rows: vec![vec![Value::String("local row".into())]],
        };
        for name in ["one.parquet", "two.parquet", "excluded.other"] {
            executor
                .write_query_result_to_parquet(&source, &dir.path().join(name))
                .unwrap();
        }
        let glob = dir.path().join("*.parquet");
        let result = run_embedded_mode(glob.to_str().unwrap(), "*", "UTC").unwrap();
        assert_eq!(result.columns, source.columns);
        assert_eq!(
            result.rows,
            vec![source.rows[0].clone(), source.rows[0].clone()]
        );
    }

    /// Embedded parquet export is unbounded when the CLI supplies no row
    /// limit. Exercises the public CLI lane and reads the artifact back
    /// through `DuckDB`, since a literal `LIMIT usize::MAX` fails inside
    /// `DuckDB` before any file is written.
    #[tokio::test]
    async fn embedded_parquet_export_without_limit_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.parquet");
        let output = dir.path().join("output.parquet");
        let executor = trawl_engine::executor::Executor::new().unwrap();
        let source = QueryResult {
            columns: vec![trawl_engine::value::Column::new(
                "message",
                trawl_engine::value::ColumnType::String,
            )],
            rows: vec![
                vec![Value::String("first".to_owned())],
                vec![Value::String("second".to_owned())],
            ],
        };
        executor
            .write_query_result_to_parquet(&source, &input)
            .unwrap();

        run_query(
            "*",
            input.to_str(),
            Some(OutputFormat::Parquet),
            Some(&output),
            None,
            "UTC",
        )
        .await
        .unwrap();

        let roundtrip = executor
            .run_query(
                "*",
                output.to_str().unwrap(),
                &trawl_core::schema::FieldTypes::new(),
                usize::MAX,
                0,
            )
            .unwrap();
        assert_eq!(roundtrip, source);
    }

    /// What `write_error_details` prints for one detail with `span`.
    fn render_detail(query: &str, span: Option<(usize, usize)>) -> String {
        let detail = trawl_client::ErrorDetail {
            message: "the message".into(),
            span: span.map(|(start, end)| trawl_client::ErrorSpan { start, end }),
            label: None,
            hint: Some("the hint".into()),
        };
        let mut buf = Vec::new();
        write_error_details(&mut buf, query, &[detail]).unwrap();
        String::from_utf8(buf).unwrap()
    }

    /// The message and hint, alone: what a span that cannot index the
    /// query gets.
    const NO_EXCERPT: &str = "  the message\n  hint: the hint\n\n";

    /// Every row of the shared span table renders its excerpt line and a
    /// caret line at the row's display cells, or the message and hint with
    /// no excerpt when the span cannot index the text.
    #[test]
    fn terminal_caret_layout_conformance() {
        for case in trawl_core::text_layout::SPAN_CASES {
            let out = render_detail(case.text, Some((case.start, case.end)));
            assert!(!out.contains('\r'), "{}: {out:?}", case.name);
            match case.expect {
                Some(expect) => {
                    let lines: Vec<&str> = out.split('\n').collect();
                    assert_eq!(
                        lines,
                        [
                            "",
                            &format!("  {}", expect.excerpt),
                            &format!("  {}{}", " ".repeat(expect.pad), "~".repeat(expect.carets)),
                            "  the message",
                            "  hint: the hint",
                            "",
                            "",
                        ],
                        "{}",
                        case.name
                    );
                }
                None => assert_eq!(out, NO_EXCERPT, "{}", case.name),
            }
        }
    }

    /// The issue's query: the caret sits under the `(`, the span's first
    /// display cell, after two double-width characters.
    #[test]
    fn terminal_caret_layout_conformance_issue_query() {
        let query = "service=\"日本\" | where x ~ /(/";
        let start = query.find('(').unwrap();
        let out = render_detail(query, Some((start, start + 1)));
        // 26 characters but 28 display cells come before the `(`.
        let caret_line = format!("  {}~", " ".repeat(28));
        assert_eq!(
            out,
            format!(
                "\n  service=\"日本\" | where x ~ /(/\n{caret_line}\n  the message\n  hint: the hint\n\n"
            )
        );
    }

    /// A CRLF query prints no `\r`, and a span running onto later lines
    /// marks only its start line.
    #[test]
    fn terminal_caret_layout_conformance_crlf() {
        let query = "service=web\r\n| stats count(\r\nby host";
        let start = query.find("count").unwrap();
        let out = render_detail(query, Some((start, query.len())));
        assert!(!out.contains('\r'), "{out:?}");
        assert_eq!(
            out,
            "\n  | stats count(\n          ~~~~~~\n  the message\n  hint: the hint\n\n"
        );
    }

    /// Reversed, past-end and mid-character spans print the message and
    /// hint with no excerpt, never a clamped caret.
    #[test]
    fn terminal_caret_layout_conformance_unusable_spans() {
        let query = "a=é b";
        for (name, span) in [
            ("reversed", (3, 1)),
            ("past the end", (2, 99)),
            ("starts past the end", (99, 99)),
            ("start inside a character", (3, 5)),
            ("end inside a character", (0, 3)),
        ] {
            assert_eq!(render_detail(query, Some(span)), NO_EXCERPT, "{name}");
        }
        assert_eq!(render_detail(query, None), NO_EXCERPT, "no span");
    }

    /// What `write_client_error` prints for a server error whose message
    /// is `message` and whose one detail has `span`.
    fn render_server_error(message: &str, span: Option<(usize, usize)>) -> String {
        let err = trawl_client::ClientError::Server {
            status: 400,
            error: trawl_api::ErrorEnvelope {
                code: trawl_api::ErrorCode::ParseError,
                message: message.into(),
                details: vec![trawl_client::ErrorDetail {
                    message: "the message".into(),
                    span: span.map(|(start, end)| trawl_client::ErrorSpan { start, end }),
                    label: None,
                    hint: None,
                }],
                conflict: None,
            },
            request_id: None,
            retry_after_secs: None,
        };
        let mut buf = Vec::new();
        write_client_error(&mut buf, "q", &err).unwrap();
        String::from_utf8(buf).unwrap()
    }

    /// The summary line, with a span (the envelope's message) or without
    /// one (the whole error), cannot rewrite the terminal either.
    #[test]
    fn client_error_summary_is_sanitized() {
        let message = "bad\u{1b}[2J\u{202E}x";
        for span in [Some((0, 1)), None] {
            let out = render_server_error(message, span);
            let summary = out.lines().next().unwrap();
            assert!(!out.contains(['\u{1b}', '\u{202E}']), "{span:?}: {out:?}");
            assert!(
                summary.contains("bad\u{FFFD}[2J\u{FFFD}x"),
                "{span:?}: {out:?}"
            );
        }
        assert_eq!(
            render_server_error(message, Some((0, 1))),
            "trawl: ParseError: bad\u{FFFD}[2J\u{FFFD}x\n\n  q\n  ~\n  the message\n\n"
        );
        assert_eq!(
            render_server_error(message, None),
            "trawl: server error (HTTP 400): bad\u{FFFD}[2J\u{FFFD}x\n"
        );
    }

    /// A message or hint holding control or bidi characters cannot rewrite
    /// the terminal line.
    #[test]
    fn terminal_caret_layout_conformance_sanitizes_message_and_hint() {
        let detail = trawl_client::ErrorDetail {
            message: "bad\u{1b}[2J\u{202E}x".into(),
            span: None,
            label: None,
            hint: Some("try\r\nthis".into()),
        };
        let mut buf = Vec::new();
        write_error_details(&mut buf, "q", &[detail]).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "  bad\u{FFFD}[2J\u{FFFD}x\n  hint: try\u{FFFD}\u{FFFD}this\n\n"
        );
    }
}
