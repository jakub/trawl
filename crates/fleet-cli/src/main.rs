use std::io::{self, IsTerminal, Write};
use std::process;

use clap::{Parser, ValueEnum};
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

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("{0}")]
    Engine(#[from] fleet_engine::error::EngineError),
    #[error("{0}")]
    Client(#[from] fleet_client::ClientError),
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Usage(String),
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if let Err(e) = run(&cli).await {
        // Broken pipe is expected (e.g. `fleet query ... | head`), exit quietly.
        if let CliError::Io(ref io_err) = e {
            if io_err.kind() == io::ErrorKind::BrokenPipe {
                process::exit(1);
            }
        }
        eprintln!("fleet: {e}");
        process::exit(1);
    }
}

async fn run(cli: &Cli) -> Result<(), CliError> {
    let format = cli.format.unwrap_or_else(|| {
        if io::stdout().is_terminal() {
            OutputFormat::Table
        } else {
            OutputFormat::Json
        }
    });

    let result = if let Some(url) = &cli.url {
        run_daemon_mode(url, cli).await?
    } else if let Some(data) = &cli.data {
        run_embedded_mode(data, &cli.query)?
    } else {
        return Err(CliError::Usage(
            "provide --url (daemon mode) or --data (embedded mode)".into(),
        ));
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();

    match format {
        OutputFormat::Table => render_table(&result, &mut out)?,
        OutputFormat::Json => render_ndjson(&result, &mut out)?,
        OutputFormat::Csv => render_csv(&result, &mut out)?,
    }

    Ok(())
}

/// Connect to the daemon and execute the query over HTTPS.
async fn run_daemon_mode(url: &str, cli: &Cli) -> Result<QueryResult, CliError> {
    let token = cli
        .token
        .as_deref()
        .ok_or_else(|| CliError::Usage("--token is required when using --url".into()))?;

    let client = if cli.insecure {
        fleet_client::HttpClient::new_insecure(url, token)?
    } else {
        fleet_client::HttpClient::new(url, token)?
    };

    Ok(client.query(&cli.query).await?)
}

/// Execute the query locally with an embedded `DuckDB` engine.
fn run_embedded_mode(data: &str, query: &str) -> Result<QueryResult, CliError> {
    let executor = fleet_engine::executor::Executor::new()?;
    // CLI has no server-side row limit — use usize::MAX.
    Ok(executor.run_query(query, data, usize::MAX)?)
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
