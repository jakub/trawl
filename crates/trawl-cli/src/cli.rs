// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CLI mode: query execution, validation, and output formatting.

use std::io::{self, IsTerminal, Write};
use std::path::Path;

use clap::ValueEnum;
use trawl_api::zone::DisplayZone;
use trawl_core::sanitize::sanitize_display_text;
use trawl_core::text_layout::{caret_layout, locate_span};
use trawl_engine::value::{QueryResult, Value};

use crate::CliError;
use crate::terminal::table_cell;

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

/// The display zone a command that prints instants reads from
/// `ui.timezone`, parsed once before it sends anything. An invalid value is
/// a usage error naming it; no path falls back to UTC.
pub fn display_zone(value: &str) -> Result<DisplayZone, CliError> {
    value
        .parse()
        .map_err(|refusal: trawl_api::zone::ZoneRefusal| CliError::Usage(refusal.to_string()))
}

/// Export a query's results as Parquet to `output`.
///
/// Parquet carries UTC instants and prints none, so this path never reads
/// the display zone.
pub async fn run_parquet_query(
    query: &str,
    data: Option<&str>,
    output: Option<&Path>,
    conn: Option<ConnectionParams>,
) -> Result<(), CliError> {
    // Parquet output requires -o flag (binary format can't go to stdout).
    let Some(output_path) = output else {
        return Err(CliError::Usage(
            "parquet output requires -o/--output flag".into(),
        ));
    };
    // Use DuckDB's native COPY TO directly.
    run_parquet_export(query, data, conn.as_ref(), output_path).await
}

/// Execute a query and print the results, each instant in `zone`.
///
/// A Parquet `format` goes to [`run_parquet_query`], which never reads
/// `zone`.
pub async fn run_query(
    query: &str,
    data: Option<&str>,
    format: Option<OutputFormat>,
    output: Option<&Path>,
    zone: DisplayZone,
    conn: Option<ConnectionParams>,
) -> Result<(), CliError> {
    let format = format.unwrap_or_else(|| {
        if io::stdout().is_terminal() {
            OutputFormat::Table
        } else {
            OutputFormat::Json
        }
    });
    if format == OutputFormat::Parquet {
        return run_parquet_query(query, data, output, conn).await;
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

    let (mut result, degraded, severity_columns) = if let Some(data) = data {
        match run_embedded_mode(data, query) {
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
        match run_daemon_mode(&conn, query).await {
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
        &mut result,
        format,
        output,
        zone,
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
#[allow(clippy::too_many_arguments)]
fn emit_results(
    result: &mut QueryResult,
    format: OutputFormat,
    output: Option<&Path>,
    zone: DisplayZone,
    degraded: &[String],
    severity_columns: &[String],
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<(), CliError> {
    if let Some(output_path) = output {
        let mut file = std::fs::File::create(output_path)?;
        render_results(result, format, zone, severity_columns, &mut file)?;
        // The file is the deliverable; the notice belongs on the terminal.
        write_degraded_footer(err, format, degraded)?;
    } else {
        render_results(result, format, zone, severity_columns, out)?;
        write_degraded_footer(out, format, degraded)?;
    }

    Ok(())
}

/// Render the result rows in the requested format, each instant in `zone`.
///
/// The table presents each instant with its own offset; json and csv carry
/// the wire spelling re-written in `zone`, so `result` leaves converted.
fn render_results(
    result: &mut QueryResult,
    format: OutputFormat,
    zone: DisplayZone,
    severity_columns: &[String],
    out: &mut impl Write,
) -> io::Result<()> {
    match format {
        OutputFormat::Table => render_table(result, zone, severity_columns, out),
        OutputFormat::Json => {
            trawl_api::zone::apply_zone(result, zone);
            render_ndjson(result, out)
        }
        OutputFormat::Csv => {
            trawl_api::zone::apply_zone(result, zone);
            render_csv(result, out)
        }
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
///
/// A field name is sender text, so each is sanitized whole.
fn write_degraded_footer(
    out: &mut impl Write,
    format: OutputFormat,
    fields: &[String],
) -> io::Result<()> {
    if format != OutputFormat::Table || fields.is_empty() {
        return Ok(());
    }
    let shown: Vec<String> = fields.iter().map(|f| sanitize_display_text(f)).collect();
    writeln!(
        out,
        "note: results may be incomplete — degraded field(s): {} \
         (see: trawl schema field {})",
        shown.join(", "),
        shown[0]
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
        writeln!(
            out,
            "not formatted: {}",
            sanitize_display_text(&refusal.message)
        )?;
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
) -> Result<(QueryResult, Vec<String>, Vec<String>), CliError> {
    let client = conn.client()?;
    let response = client.query_paginated(query, None, None).await?;
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
fn run_embedded_mode(data: &str, query: &str) -> Result<QueryResult, CliError> {
    let executor = trawl_engine::executor::Executor::new()?;
    // CLI has no server-side row limit, hence usize::MAX. Embedded mode is
    // pin-blind by design (no catalog, ADR-0011): the explicit empty set
    // keeps that decision visible.
    Ok(executor.run_query(
        query,
        data,
        &trawl_core::schema::FieldTypes::new(),
        usize::MAX,
    )?)
}

// -- output formatters -------------------------------------------------------

fn render_table(
    result: &QueryResult,
    zone: DisplayZone,
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

    let headers: Vec<String> = result.columns.iter().map(|c| table_cell(&c.name)).collect();
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
                // A timestamp shows its wall clock in the display zone with
                // that instant's own offset, whatever its column is called.
                let presented = result
                    .columns
                    .get(i)
                    .and_then(|c| trawl_api::zone::present_cell(&c.ty, v, zone));
                let text = match presented {
                    Some(text) => text,
                    None if severity_cells.get(i).copied().unwrap_or(false) => {
                        severity_cell_text(v)
                    }
                    None => v.to_string(),
                };
                table_cell(&text)
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
    // stderr is the last place to report anything; a failed write has
    // nowhere to go.
    let _ = write_engine_error(&mut io::stderr().lock(), query, err);
}

/// Write `err` for the query `query`, the text that was run.
///
/// An error other than a parse error can quote the data the engine read,
/// so it is sanitized whole, like the details.
fn write_engine_error(
    out: &mut impl Write,
    query: &str,
    err: &trawl_engine::error::EngineError,
) -> io::Result<()> {
    match err {
        trawl_engine::error::EngineError::Parse(errors) => {
            writeln!(out, "trawl: parse error")?;
            let details: Vec<_> = errors.iter().map(parse_error_to_detail).collect();
            write_error_details(out, query, &details)
        }
        other => writeln!(out, "trawl: {}", sanitize_display_text(&other.to_string())),
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

    let headers: Vec<String> = columns.iter().map(|c| table_cell(c)).collect();
    table.set_header(headers);

    for row in rows {
        let cells: Vec<String> = row.iter().map(|v| table_cell(&json_display(v))).collect();
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
        render_table(&result, DisplayZone::Utc, &severity, &mut out).unwrap();
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
                &mut result.clone(),
                format,
                output,
                DisplayZone::Utc,
                &degraded,
                &severity,
                &mut out,
                &mut err,
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
            render_table(&result, DisplayZone::Utc, &[], &mut buf).unwrap();
            String::from_utf8(buf).unwrap()
        };
        assert!(table.contains("error"));
        assert!(!table.contains(" 17 "));
        assert!(render(render_ndjson).contains("17"));
        assert!(render(render_csv).contains("17"));
    }

    /// A pivot names its columns from data, so a timestamp column can be
    /// called `_severity`. Its type decides how it reads, not its name.
    #[test]
    fn a_timestamp_column_named_like_severity_still_shows_in_the_display_zone() {
        use trawl_engine::value::{Column, ColumnType};
        let mut result = QueryResult {
            columns: vec![Column::new("_severity", ColumnType::Timestamp)],
            rows: vec![vec![Value::String("2026-10-25T00:30:00.000000Z".into())]],
        };
        let zone = "+02:00".parse::<DisplayZone>().unwrap();
        let mut buf = Vec::new();
        render_results(&mut result, OutputFormat::Table, zone, &[], &mut buf).unwrap();
        let table = String::from_utf8(buf).unwrap();
        assert!(table.contains("2026-10-25 02:30:00 +02:00"), "{table}");
    }

    /// The table shows each instant in the display zone with its own
    /// offset, so the two 02:30s of a fall-back night differ; json and csv
    /// carry the wire spelling re-written in the zone. A string column
    /// holding time-shaped text stays as sent everywhere.
    #[test]
    fn query_output_presents_each_instant_in_the_display_zone() {
        use trawl_engine::value::{Column, ColumnType};
        let s = |t: &str| Value::String(t.into());
        let berlin_summer = "+02:00".parse::<DisplayZone>().unwrap();
        let result = QueryResult {
            columns: vec![
                Column::new("_time", ColumnType::Timestamp),
                Column::new("seen", ColumnType::list(ColumnType::Timestamp)),
                Column::new("text", ColumnType::String),
            ],
            rows: vec![
                vec![
                    s("2026-10-25T00:30:00.000000Z"),
                    Value::Array(vec![s("2026-10-25T00:30:00.120000Z"), s("infinity")]),
                    s("2026-10-25T00:30:00.000000Z"),
                ],
                vec![s("-infinity"), Value::Array(vec![]), s("x")],
            ],
        };
        let render = |format, zone| {
            let mut buf = Vec::new();
            render_results(&mut result.clone(), format, zone, &[], &mut buf).unwrap();
            String::from_utf8(buf).unwrap()
        };

        let table = render(OutputFormat::Table, berlin_summer);
        for shown in [
            "2026-10-25 02:30:00 +02:00",
            "[2026-10-25 02:30:00.12 +02:00, infinity]",
            "2026-10-25T00:30:00.000000Z",
            "-infinity",
        ] {
            assert!(table.contains(shown), "{shown:?} missing from\n{table}");
        }
        let utc = render(OutputFormat::Table, DisplayZone::Utc);
        assert!(utc.contains("2026-10-25 00:30:00 +00:00"), "{utc}");

        let json = render(OutputFormat::Json, berlin_summer);
        let first: serde_json::Value = serde_json::from_str(json.lines().next().unwrap()).unwrap();
        assert_eq!(first["_time"], "2026-10-25T02:30:00.000000+02:00");
        assert_eq!(
            first["seen"],
            serde_json::json!(["2026-10-25T02:30:00.120000+02:00", "infinity"])
        );
        assert_eq!(first["text"], "2026-10-25T00:30:00.000000Z");

        let csv = render(OutputFormat::Csv, berlin_summer);
        let row = csv.lines().nth(1).unwrap();
        assert!(
            row.starts_with("2026-10-25T02:30:00.000000+02:00,"),
            "{row}"
        );
        assert!(row.ends_with(",2026-10-25T00:30:00.000000Z"), "{row}");
    }

    /// The zone a query reads comes from the one trawl-api grammar, and a
    /// refusal is a usage error naming the value.
    #[test]
    fn an_invalid_display_zone_is_a_usage_error_naming_it() {
        assert_eq!(display_zone(" local ").unwrap(), DisplayZone::Local);
        let err = display_zone("Mars/Olympus").unwrap_err();
        assert!(matches!(err, CliError::Usage(_)), "{err:?}");
        assert!(err.to_string().contains("\"Mars/Olympus\""), "{err}");
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
        render_table(&result, DisplayZone::Utc, &declared, &mut buf).unwrap();
        let table = String::from_utf8(buf).unwrap();
        assert!(table.contains("error2"), "{table}");
        assert!(
            table.contains("18"),
            "the undeclared column keeps its number: {table}"
        );

        // Undeclared: no list, no tokens.
        let mut buf = Vec::new();
        render_table(&result, DisplayZone::Utc, &[], &mut buf).unwrap();
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
        render_table(&mixed, DisplayZone::Utc, &declared, &mut buf).unwrap();
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
        let result = run_parquet_query("*", Some("dummy.parquet"), None, None).await;
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
        let result = run_embedded_mode(glob.to_str().unwrap(), "*").unwrap();
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

        run_parquet_query("*", input.to_str(), Some(&output), None)
            .await
            .unwrap();

        let roundtrip = executor
            .run_query(
                "*",
                output.to_str().unwrap(),
                &trawl_core::schema::FieldTypes::new(),
                usize::MAX,
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

    /// Sender text a table draws, each with what the cell rule makes of it:
    /// an OSC 52 clipboard write (ESC and BEL), a screen clear, a C1 CSI,
    /// an RLO, a zero-width space, a soft hyphen, a tab and a lone CR.
    const HOSTILE_CELLS: [(&str, &str); 8] = [
        ("o\u{1b}]52;c;aGk=\u{7}k", "o\u{FFFD}]52;c;aGk=\u{FFFD}k"),
        ("c\u{1b}[2Jl", "c\u{FFFD}[2Jl"),
        ("c\u{9b}1m", "c\u{FFFD}1m"),
        ("r\u{202e}lo", "r\u{FFFD}lo"),
        ("z\u{200b}w", "z\u{FFFD}w"),
        ("s\u{ad}hy", "s\u{FFFD}hy"),
        ("t\tab", "t\u{FFFD}ab"),
        ("c\rr", "c\u{FFFD}r"),
    ];

    /// The unsafe characters in `text` that are not a line break.
    fn unsafe_chars(text: &str) -> usize {
        text.chars()
            .filter(|&c| c != '\n' && trawl_core::sanitize::is_unsafe_display_char(c))
            .count()
    }

    /// The text of every cell line a table draws, left to right and top
    /// to bottom: what sits between its borders (`│` outside, `┆` between
    /// columns), with the padding spaces (only those) trimmed.
    fn cell_lines(table: &str) -> Vec<&str> {
        table
            .lines()
            .filter_map(|line| line.strip_prefix('│')?.strip_suffix('│'))
            .flat_map(|line| line.split('┆'))
            .map(|cell| cell.trim_matches(' '))
            .collect()
    }

    /// Both comfy-table renderers draw every header and cell through the
    /// cell rule, after value presentation: no unsafe character reaches the
    /// output, each becomes one U+FFFD, and a line break stays a line inside
    /// its cell.
    #[test]
    fn terminal_table_sanitizes_sender_text() {
        use trawl_engine::value::{Column, ColumnType};

        let header = "n\u{1b}[2J\u{202e}\u{9b}";
        let header_shown = "n\u{FFFD}[2J\u{FFFD}\u{FFFD}";

        // `trawl query`: a string column under a hostile name, a list, a
        // `_severity` column whose text has no token (the fallback), and a
        // timestamp column whose text does not parse (present_cell's
        // fallback).
        let result = QueryResult {
            columns: vec![
                Column::new(header, ColumnType::String),
                Column::new("list", ColumnType::list(ColumnType::String)),
                Column::new("_severity", ColumnType::String),
                Column::new("_time", ColumnType::Timestamp),
            ],
            rows: HOSTILE_CELLS
                .iter()
                .map(|(text, _)| {
                    vec![
                        Value::String((*text).to_owned()),
                        Value::Array(vec![
                            Value::String((*text).to_owned()),
                            Value::String("日本語".to_owned()),
                        ]),
                        Value::String((*text).to_owned()),
                        Value::String((*text).to_owned()),
                    ]
                })
                .collect(),
        };
        let mut buf = Vec::new();
        render_table(&result, DisplayZone::Utc, &[], &mut buf).unwrap();
        let table = String::from_utf8(buf).unwrap();
        assert_eq!(unsafe_chars(&table), 0, "{table:?}");
        assert!(table.contains(header_shown), "{table}");
        let cells = cell_lines(&table);
        for (_, shown) in HOSTILE_CELLS {
            // The string, the severity fallback and the timestamp fallback.
            let whole = cells.iter().filter(|cell| **cell == shown).count();
            assert_eq!(whole, 3, "{shown:?}: {table}");
            assert!(
                cells.contains(&format!("[{shown}, 日本語]").as_str()),
                "{table}"
            );
        }
        let sent: usize = unsafe_chars(header)
            + HOSTILE_CELLS
                .iter()
                .map(|(text, _)| 4 * unsafe_chars(text))
                .sum::<usize>();
        assert_eq!(table.matches('\u{FFFD}').count(), sent, "{table}");

        // The driver table (`schema`, `driver query`, `get-results`): a
        // hostile header, a string, and an object whose keys and values are
        // hostile. json_display spells the object as compact JSON, which
        // escapes C0 controls but leaves C1, bidi and format characters raw.
        let columns = vec![header.to_owned(), "text".to_owned(), "obj".to_owned()];
        let rows: Vec<Vec<serde_json::Value>> = HOSTILE_CELLS
            .iter()
            .map(|(text, _)| {
                vec![
                    serde_json::json!("résumé"),
                    serde_json::json!(text),
                    serde_json::json!({ "k\u{202e}\u{9b}": text, "n": [text] }),
                ]
            })
            .collect();
        let mut buf = Vec::new();
        render_driver_results(&columns, &rows, OutputFormat::Table, &mut buf).unwrap();
        let table = String::from_utf8(buf).unwrap();
        assert_eq!(unsafe_chars(&table), 0, "{table:?}");
        assert!(table.contains(header_shown), "{table}");
        assert!(table.contains("résumé"), "{table}");
        assert!(table.contains("\"k\u{FFFD}\u{FFFD}\":"), "{table}");
        let cells = cell_lines(&table);
        for (_, shown) in HOSTILE_CELLS {
            assert!(cells.contains(&shown), "{shown:?}: {table}");
        }
        let sent: usize = unsafe_chars(header)
            + rows
                .iter()
                .flatten()
                .map(|v| unsafe_chars(&json_display(v)))
                .sum::<usize>();
        assert_eq!(table.matches('\u{FFFD}').count(), sent, "{table}");

        // A line break, LF or CRLF, is a second line inside the same cell.
        let broken = QueryResult {
            columns: vec![Column::new("text", ColumnType::String)],
            rows: vec![
                vec![Value::String("a\nb".to_owned())],
                vec![Value::String("a\r\nb".to_owned())],
            ],
        };
        let mut buf = Vec::new();
        render_table(&broken, DisplayZone::Utc, &[], &mut buf).unwrap();
        let table = String::from_utf8(buf).unwrap();
        assert!(!table.contains('\r'), "{table:?}");
        assert_eq!(cell_lines(&table), ["text", "a", "b", "a", "b"], "{table}");

        let mut buf = Vec::new();
        render_driver_results(
            &["text".to_owned()],
            &[
                vec![serde_json::json!("a\nb")],
                vec![serde_json::json!("a\r\nb")],
            ],
            OutputFormat::Table,
            &mut buf,
        )
        .unwrap();
        let table = String::from_utf8(buf).unwrap();
        assert!(!table.contains('\r'), "{table:?}");
        assert_eq!(cell_lines(&table), ["text", "a", "b", "a", "b"], "{table}");
    }

    /// Every value a prose line interpolates goes through the sanitize
    /// rule whole: its ESC, RLO and line break each print U+FFFD, and only
    /// the line's own layout breaks it.
    #[test]
    fn terminal_cli_lines_sanitize_sender_text() {
        let hostile = "v\u{1b}[2J\u{202e}\nx";
        let shown = "v\u{FFFD}[2J\u{FFFD}\u{FFFD}x";

        // The degraded footer names the field twice.
        let mut buf = Vec::new();
        write_degraded_footer(
            &mut buf,
            OutputFormat::Table,
            &[hostile.to_owned(), "status".to_owned()],
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            format!(
                "note: results may be incomplete — degraded field(s): {shown}, status \
                 (see: trawl schema field {shown})\n"
            )
        );

        // The server's reason for not formatting a valid query.
        let response = trawl_client::ValidationResponse {
            valid: true,
            errors: vec![],
            formatted: None,
            format_error: Some(trawl_client::types::FormatErrorDetail {
                kind: trawl_client::types::FormatErrorKind::TooLong,
                message: hostile.to_owned(),
            }),
        };
        let mut buf = Vec::new();
        write_valid(&mut buf, &response).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            format!("valid\nnot formatted: {shown}\n")
        );

        // An embedded engine error, which can quote the data it read.
        let refused = trawl_engine::error::EngineError::Refused {
            message: hostile.to_owned(),
        };
        let mut buf = Vec::new();
        write_engine_error(&mut buf, "q", &refused).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), format!("trawl: {shown}\n"));

        // A parse error keeps its heading and its caret details.
        let parse = trawl_engine::error::EngineError::Parse(vec![trawl_core::parser::ParseError {
            message: hostile.to_owned(),
            span: 0..1,
            label: None,
            hint: None,
        }]);
        let mut buf = Vec::new();
        write_engine_error(&mut buf, "q", &parse).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            format!("trawl: parse error\n\n  q\n  ~\n  {shown}\n\n")
        );
    }
}
