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

    match format {
        OutputFormat::Table => render_table(&result, &mut out),
        OutputFormat::Json => render_json(&result, &mut out),
        OutputFormat::Csv => render_csv(&result, &mut out),
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
    };
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

fn render_table(result: &QueryResult, out: &mut impl Write) {
    if result.is_empty() {
        let _ = writeln!(out, "no results");
        return;
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

    let _ = writeln!(out, "{table}");
    let _ = writeln!(out, "{} row(s)", result.row_count());
}

fn render_json(result: &QueryResult, out: &mut impl Write) {
    for row in &result.rows {
        let mut map = serde_json::Map::new();
        for (col, val) in result.columns.iter().zip(row.iter()) {
            map.insert(col.name.clone(), value_to_json(val));
        }
        let _ = serde_json::to_writer(&mut *out, &map);
        let _ = writeln!(out);
    }
}

fn render_csv(result: &QueryResult, out: &mut impl Write) {
    let headers: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    let _ = writeln!(out, "{}", headers.join(","));

    for row in &result.rows {
        let cells: Vec<String> = row.iter().map(|v| csv_escape(&v.to_string())).collect();
        let _ = writeln!(out, "{}", cells.join(","));
    }
}

fn value_to_json(val: &Value) -> serde_json::Value {
    match val {
        Value::Null => serde_json::Value::Null,
        Value::Boolean(b) => serde_json::Value::Bool(*b),
        Value::Integer(i) => serde_json::json!(i),
        Value::Float(f) => serde_json::json!(f),
        Value::String(s) => serde_json::Value::String(s.clone()),
    }
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}
