// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CLI mode: query execution, validation, and output formatting.

use std::io::{self, IsTerminal, Write};
use std::path::Path;

use clap::ValueEnum;
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

/// Resolved connection parameters (after config + env + CLI override merge).
#[derive(Clone)]
pub struct ConnectionParams {
    pub url: String,
    pub token: String,
    pub insecure: bool,
}

/// Hand-written so the API token never reaches a log line or a panic message.
impl std::fmt::Debug for ConnectionParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionParams")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .field("insecure", &self.insecure)
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

    // Block `from saved` in embedded mode — it requires server-side
    // auth database access to resolve saved query names to parquet paths.
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
            // Embedded mode has no catalog, so it has no notice to carry
            // — but `sev()` declares its own pin, so its columns still
            // render as tokens here (that is the whole point of a
            // FUNCTION-declared pin: it needs no catalog).
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

/// The severity-rendering columns of an EMBEDDED query.
///
/// The same walk the server runs, from a pin-blind root: embedded mode
/// has no catalog, so only a FUNCTION-declared pin can survive it — which
/// is exactly `sev()`, and exactly why it is declared rather than
/// derived. An unparseable query renders nothing special; the engine
/// reports the parse error.
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
/// The stream split is the contract: with `-o`, the FILE is the
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

/// The incomplete-results footer (ADR-0011 slice C1): one line after the
/// row count when the query bound a field whose pin is shelving values.
///
/// Table output only. json/csv/parquet carry `degraded_fields` on the wire
/// — that IS the notice for a machine — and a prose line in a machine format
/// is a parse error waiting to happen. One sentence, no advice beyond where
/// to look: the case file holds the evidence and the remedy.
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
        let client = make_client(&conn)?;
        let response = client.validate(query).await?;
        if response.valid {
            println!("valid");
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
        // Embedded mode is pin-blind by design (no catalog, ADR-0011
        // slice A): the explicit empty set keeps that decision visible.
        executor.export_parquet(
            query,
            data,
            &trawl_core::schema::FieldTypes::new(),
            output_path,
            usize::MAX,
        )?;
    } else if let Some(conn) = conn {
        // Daemon mode: fetch parquet bytes via HTTP export endpoint.
        let client = make_client(conn)?;
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
    let client = make_client(conn)?;
    let response = client
        .query_paginated_tz(query, None, None, Some(timezone.to_owned()))
        .await?;
    Ok(daemon_outcome(response))
}

/// Split a daemon response into what the printer needs: the rows and the
/// degraded-field note (ADR-0011 slice C1), which is carried, never
/// dropped.
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
    // CLI has no server-side row limit — use usize::MAX. Embedded mode is
    // pin-blind by design (no catalog, ADR-0011 slice A): the explicit
    // empty set keeps that decision visible.
    Ok(executor.run_query(
        query,
        data,
        &trawl_core::schema::FieldTypes::new(),
        usize::MAX,
        utc_offset_secs,
    )?)
}

/// Build an `HttpClient` from resolved connection params.
fn make_client(conn: &ConnectionParams) -> Result<trawl_client::HttpClient, CliError> {
    let client = if conn.insecure {
        trawl_client::HttpClient::new_insecure(&conn.url, &conn.token)?
    } else {
        trawl_client::HttpClient::new(&conn.url, &conn.token)?
    };
    Ok(client)
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

    // A severity column DISPLAYS its OTel token (ADR-0013 §6): `17` reads
    // `error`, the same vocabulary that would filter it. `_severity` by
    // name, plus whatever the response declared (a `sev()` output). Only
    // the table renders it — json/csv keep the number, for arithmetic
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
                    v.to_string()
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
            // Value's custom Serialize impl maps directly to JSON primitives,
            // so this conversion is infallible.
            map.insert(
                col.name.clone(),
                serde_json::to_value(val).expect("Value serialization is infallible"),
            );
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
        // full precision for data fidelity (TUI display uses 2dp for readability)
        Value::Float(f) => f.to_string(),
        Value::String(s) => csv_escape_string(s),
        Value::Array(arr) => {
            let json = serde_json::to_string(arr).unwrap_or_default();
            csv_escape_string(&json)
        }
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
///                 ~~~~~~
///   error: unknown command 'staats'
///   hint: did you mean 'stats'?
/// ```
pub fn render_error_details(query: &str, details: &[trawl_client::ErrorDetail]) {
    for detail in details {
        if let Some(ref span) = detail.span {
            render_span_error(
                query,
                span.start,
                span.end,
                &detail.message,
                detail.hint.as_deref(),
            );
        } else {
            eprintln!("  {}", detail.message);
            if let Some(ref hint) = detail.hint {
                eprintln!("  hint: {hint}");
            }
        }
        eprintln!();
    }
}

/// Render a single span error with caret underline and optional hint.
fn render_span_error(query: &str, start: usize, end: usize, message: &str, hint: Option<&str>) {
    // Clamp to valid byte boundaries.
    let start = start.min(query.len());
    let end = end.min(query.len()).max(start);

    // Find the line containing the span start.
    let mut line_start = 0;
    let mut line_end = query.len();
    for (i, ch) in query.char_indices() {
        if ch == '\n' {
            if i < start {
                line_start = i + 1;
            }
            if i >= end && line_end == query.len() {
                line_end = i;
            }
        }
    }

    let line = &query[line_start..line_end];
    let col_start = start - line_start;
    let col_end = (end - line_start).min(line.len());
    let underline_len = (col_end - col_start).max(1);

    eprintln!();
    eprintln!("  {line}");
    eprintln!("  {}{}", " ".repeat(col_start), "~".repeat(underline_len));
    eprintln!("  {message}");
    if let Some(hint) = hint {
        eprintln!("  hint: {hint}");
    }
}

/// Render a `ClientError` with span details (if available) for CLI output.
pub fn render_client_error(query: &str, err: &trawl_client::ClientError) {
    let details = err.error_details();
    if details.is_empty() {
        eprintln!("trawl: {err}");
    } else {
        if let Some(envelope) = err.error_envelope() {
            eprintln!("trawl: {:?}: {}", envelope.code, envelope.message);
        }
        render_error_details(query, details);
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
/// Used by `trawl driver query` and `trawl driver get-results`.
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

    /// The notice is a TABLE-only footer: a machine format carries the
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

    /// A mocked daemon response carrying the note channel beside one
    /// result row.
    fn mocked_response() -> trawl_client::QueryResponse {
        trawl_client::QueryResponse {
            result: QueryResult {
                columns: vec![trawl_engine::value::Column {
                    name: "host".to_owned(),
                }],
                rows: vec![vec![Value::String("db1".to_owned())]],
            },
            truncated: false,
            pagination: trawl_client::PaginationMeta {
                limit: 100,
                offset: 0,
                returned: 1,
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
            columns: vec![trawl_engine::value::Column {
                name: trawl_core::schema::SEVERITY.to_owned(),
            }],
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

    /// A DECLARED severity column — `sev()`'s output under its own alias
    /// — renders its token in the table and its NUMBER everywhere a
    /// machine reads (ADR-0013 slice 2, ruling 9).
    #[test]
    fn a_declared_severity_column_displays_its_token_in_the_table_only() {
        let result = QueryResult {
            columns: vec![
                trawl_engine::value::Column {
                    name: "s".to_owned(),
                },
                trawl_engine::value::Column {
                    name: "n".to_owned(),
                },
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

        // Machine formats carry the number in BOTH columns.
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

    /// The embedded lane names the same columns the server would: the
    /// pin is the FUNCTION's, so it survives a catalog-less root.
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
}
