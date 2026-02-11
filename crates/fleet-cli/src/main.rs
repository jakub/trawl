use std::io::{self, IsTerminal, Write};
use std::process;

use clap::{Parser, ValueEnum};
use fleet_engine::executor::Executor;
use fleet_engine::value::{QueryResult, Value};

/// fleet — search your logs with a pipeline DSL.
#[derive(Parser)]
#[command(name = "fleet", version, about)]
struct Cli {
    /// Parquet glob path (e.g. "/data/**/*.parquet").
    #[arg(long)]
    data: String,

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

fn main() {
    let cli = Cli::parse();

    let format = cli.format.unwrap_or_else(|| {
        if io::stdout().is_terminal() {
            OutputFormat::Table
        } else {
            OutputFormat::Json
        }
    });

    let executor = match Executor::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("fleet: failed to initialize engine: {e}");
            process::exit(1);
        }
    };

    let result = match executor.run_query(&cli.query, &cli.data) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fleet: {e}");
            process::exit(1);
        }
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();

    match format {
        OutputFormat::Table => render_table(&result, &mut out),
        OutputFormat::Json => render_json(&result, &mut out),
        OutputFormat::Csv => render_csv(&result, &mut out),
    }
}

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
    // ndjson: one JSON object per row
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
    // header
    let headers: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    let _ = writeln!(out, "{}", headers.join(","));

    // rows
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
