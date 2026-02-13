use std::io::{self, IsTerminal, Write};
use std::process;

use clap::{Parser, ValueEnum};
use fleet_engine::QueryEngine;
use fleet_engine::value::{QueryResult, Value};

/// fleet — search your logs with a pipeline DSL.
#[derive(Parser)]
#[command(name = "fleet", version, about)]
struct Cli {
    /// Daemon URL (e.g. `https://localhost:8080`). Enables daemon mode.
    #[arg(long, env = "FLEET_URL")]
    url: Option<String>,

    /// API key for daemon authentication (required with --url).
    #[arg(long, env = "FLEET_TOKEN")]
    token: Option<String>,

    /// Accept self-signed TLS certificates (like `curl -k`).
    #[arg(long, env = "FLEET_INSECURE")]
    insecure: bool,

    /// Parquet glob path for embedded mode (e.g. "/data/**/*.parquet").
    /// Used when --url is not set.
    #[arg(long)]
    data: Option<String>,

    /// The fleet DSL query.
    query: String,

    /// Output format (auto-detected if omitted: table for TTY, json for pipes).
    #[arg(long, short, value_enum)]
    format: Option<OutputFormat>,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Table,
    Json,
    Csv,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let format = cli.format.unwrap_or_else(|| {
        if io::stdout().is_terminal() {
            OutputFormat::Table
        } else {
            OutputFormat::Json
        }
    });

    let result = if let Some(url) = &cli.url {
        run_daemon_mode(url, &cli).await
    } else if let Some(data) = &cli.data {
        run_embedded_mode(data, &cli.query)
    } else {
        eprintln!("fleet: provide --url (daemon mode) or --data (embedded mode)");
        process::exit(1);
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();

    let write_result = match format {
        OutputFormat::Table => render_table(&result, &mut out),
        OutputFormat::Json => render_json(&result, &mut out),
        OutputFormat::Csv => render_csv(&result, &mut out),
    };

    if let Err(e) = write_result {
        // Broken pipe is expected (e.g. `fleet query ... | head`), exit quietly.
        if e.kind() != io::ErrorKind::BrokenPipe {
            eprintln!("fleet: write error: {e}");
        }
        process::exit(1);
    }
}

/// Connect to the daemon and execute the query over HTTPS.
async fn run_daemon_mode(url: &str, cli: &Cli) -> QueryResult {
    let token = cli.token.as_deref().unwrap_or_else(|| {
        eprintln!("fleet: --token is required when using --url");
        process::exit(1);
    });

    let client = if cli.insecure {
        fleet_client::HttpClient::new_insecure(url, token)
    } else {
        fleet_client::HttpClient::new(url, token)
    }
    .unwrap_or_else(|e| {
        eprintln!("fleet: {e}");
        process::exit(1);
    });
    match client.query(&cli.query).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fleet: {e}");
            process::exit(1);
        }
    }
}

/// Execute the query locally with an embedded `DuckDB` engine.
fn run_embedded_mode(data: &str, query: &str) -> QueryResult {
    let executor = match fleet_engine::executor::Executor::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("fleet: failed to initialize engine: {e}");
            process::exit(1);
        }
    };

    // CLI has no server-side row limit — use usize::MAX.
    match executor.run_query(query, data, usize::MAX) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fleet: {e}");
            process::exit(1);
        }
    }
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

fn render_json(result: &QueryResult, out: &mut impl Write) -> io::Result<()> {
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
        Value::Float(f) => f.to_string(),
        Value::String(s) => csv_escape_string(s),
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
