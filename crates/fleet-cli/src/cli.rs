//! CLI mode: query execution, validation, and output formatting.

use std::io::{self, IsTerminal, Write};
use std::path::Path;

use clap::ValueEnum;
use fleet_engine::value::{QueryResult, Value};

use crate::CliError;

/// Output format for CLI query results.
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
    Csv,
    Parquet,
}

/// Resolved connection parameters (after config + env + CLI override merge).
pub struct ConnectionParams {
    pub url: String,
    pub token: String,
    pub insecure: bool,
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

    let result = if let Some(data) = data {
        run_embedded_mode(data, query, timezone)?
    } else if let Some(conn) = conn {
        run_daemon_mode(&conn, query, timezone).await?
    } else {
        return Err(CliError::Usage(
            "provide --url (daemon mode) or --data (embedded mode)".into(),
        ));
    };

    // Write to file or stdout.
    if let Some(output_path) = output {
        let mut file = std::fs::File::create(output_path)?;
        match format {
            OutputFormat::Table => render_table(&result, &mut file)?,
            OutputFormat::Json => render_ndjson(&result, &mut file)?,
            OutputFormat::Csv => render_csv(&result, &mut file)?,
            OutputFormat::Parquet => unreachable!("handled above"),
        }
    } else {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        match format {
            OutputFormat::Table => render_table(&result, &mut out)?,
            OutputFormat::Json => render_ndjson(&result, &mut out)?,
            OutputFormat::Csv => render_csv(&result, &mut out)?,
            OutputFormat::Parquet => unreachable!("handled above"),
        }
    }

    Ok(())
}

/// Validate a DSL query.
///
/// If connection params are provided, validates via the server (checks syntax,
/// semantics, function arity, regex patterns). Otherwise, validates locally
/// (parse-only via `fleet_core`).
pub async fn run_validate(query: &str, conn: Option<ConnectionParams>) -> Result<(), CliError> {
    if let Some(conn) = conn {
        // Server-side validation (richer checks).
        let client = make_client(&conn)?;
        let response = client.validate(query).await?;
        if response.valid {
            println!("valid");
        } else {
            for err in &response.errors {
                eprintln!("{err}");
            }
            return Err(CliError::Usage("query validation failed".into()));
        }
    } else {
        // Local parse-only validation.
        match fleet_core::parser::parse(query) {
            Ok(_) => println!("valid"),
            Err(errors) => {
                for err in &errors {
                    eprintln!("{err}");
                }
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
        let executor = fleet_engine::executor::Executor::new()?;
        executor.export_parquet(query, data, output_path, usize::MAX)?;
    } else if let Some(conn) = conn {
        // Daemon mode: fetch parquet bytes via HTTP export endpoint.
        let client = make_client(conn)?;
        let bytes = client
            .export(query, fleet_client::ExportFormat::Parquet, None)
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
) -> Result<QueryResult, CliError> {
    let client = make_client(conn)?;
    let response = client
        .query_paginated_tz(query, None, None, Some(timezone.to_owned()))
        .await?;
    Ok(response.result)
}

/// Execute the query locally with an embedded `DuckDB` engine.
fn run_embedded_mode(data: &str, query: &str, timezone: &str) -> Result<QueryResult, CliError> {
    let utc_offset_secs =
        fleet_engine::timezone::resolve_utc_offset(timezone).map_err(CliError::Usage)?;
    let executor = fleet_engine::executor::Executor::new()?;
    // CLI has no server-side row limit — use usize::MAX.
    Ok(executor.run_query(query, data, usize::MAX, utc_offset_secs)?)
}

/// Build an `HttpClient` from resolved connection params.
fn make_client(conn: &ConnectionParams) -> Result<fleet_client::HttpClient, CliError> {
    let client = if conn.insecure {
        fleet_client::HttpClient::new_insecure(&conn.url, &conn.token)?
    } else {
        fleet_client::HttpClient::new(&conn.url, &conn.token)?
    };
    Ok(client)
}

// -- output formatters -------------------------------------------------------

fn render_table(result: &QueryResult, out: &mut impl Write) -> io::Result<()> {
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

    for row in &result.rows {
        let cells: Vec<String> = row.iter().map(ToString::to_string).collect();
        table.add_row(cells);
    }

    writeln!(out, "{table}")?;
    writeln!(out, "{} row(s)", result.row_count())?;
    Ok(())
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

// -- driver output formatters -------------------------------------------------

/// Render driver result data (columns + JSON rows) in the requested format.
/// Used by `fleet driver query` and `fleet driver get-results`.
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
                "parquet format is not supported for driver output, use -o with fleet query instead"
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
}
