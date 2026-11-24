// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CLI mode: query execution, validation, and output formatting.

use std::io::{self, Write};
use std::path::Path;

use clap::ValueEnum;
use trawl_api::zone::DisplayZone;
use trawl_core::sanitize::sanitize_display_text;
use trawl_core::text_layout::{caret_layout, locate_span};
use trawl_engine::value::{QueryResult, Value};

use crate::CliError;
use crate::terminal::{Destination, table_cell, write_json_line};

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
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Otel => "otel",
            Self::Syslog => "syslog",
        }
    }
}

#[derive(Clone)]
pub struct ConnectionParams {
    pub url: String,
    pub token: String,
    pub trust: trawl_client::TlsTrust,
}

impl ConnectionParams {
    pub fn client(&self) -> Result<trawl_client::HttpClient, CliError> {
        Ok(trawl_client::HttpClient::with_trust(
            &self.url,
            &self.token,
            &self.trust,
        )?)
    }
}

impl std::fmt::Debug for ConnectionParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionParams")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .field("trust", &self.trust)
            .finish()
    }
}

pub fn display_zone(value: &str) -> Result<DisplayZone, CliError> {
    value
        .parse()
        .map_err(|refusal: trawl_api::zone::ZoneRefusal| CliError::Usage(refusal.to_string()))
}

pub async fn run_parquet_query(
    query: &str,
    data: Option<&str>,
    output: Option<&Path>,
    conn: Option<ConnectionParams>,
) -> Result<(), CliError> {
    let Some(output_path) = output else {
        return Err(CliError::Usage(
            "parquet output requires -o/--output flag".into(),
        ));
    };
    // Callers reach this without `run_query`, so it advises for itself.
    render_query_advice(query, trawl_core::parser::parse(query).ok().as_ref());
    run_parquet_export(query, data, conn.as_ref(), output_path).await
}

pub async fn run_query(
    query: &str,
    data: Option<&str>,
    format: Option<OutputFormat>,
    output: Option<&Path>,
    zone: DisplayZone,
    conn: Option<ConnectionParams>,
) -> Result<(), CliError> {
    let destination = Destination::stdout();
    let format = format.unwrap_or(match destination {
        Destination::Terminal => OutputFormat::Table,
        Destination::Data => OutputFormat::Json,
    });
    if format == OutputFormat::Parquet {
        return run_parquet_query(query, data, output, conn).await;
    }

    let parsed = trawl_core::parser::parse(query).ok();

    if data.is_some()
        && let Some(ast) = &parsed
        && ast.from_saved_stage().is_some()
    {
        return Err(CliError::Usage(
            "\"from saved\" requires a server connection and cannot be used with --data".into(),
        ));
    }

    render_query_advice(query, parsed.as_ref());

    let (mut result, degraded, severity_columns) = if let Some(data) = data {
        match run_embedded_mode(data, query) {
            Ok(r) => (r, Vec::new(), embedded_severity_columns(parsed.as_ref())),
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
        destination,
        &mut stdout.lock(),
        &mut io::stderr(),
    )
}

/// The same walk the server runs, from a pin-blind root: with no catalog
/// only a function-declared pin survives it, which is exactly `sev()`.
fn embedded_severity_columns(parsed: Option<&trawl_core::ast::Query>) -> Vec<String> {
    parsed.map_or_else(Vec::new, |ast| {
        trawl_core::pin_scope::severity_output_columns(
            &ast.pipeline,
            &trawl_core::pin_scope::PinScope::unpinned(),
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn emit_results(
    result: &mut QueryResult,
    format: OutputFormat,
    output: Option<&Path>,
    zone: DisplayZone,
    degraded: &[String],
    severity_columns: &[String],
    destination: Destination,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<(), CliError> {
    if let Some(output_path) = output {
        let mut file = std::fs::File::create(output_path)?;
        render_results(
            result,
            format,
            zone,
            severity_columns,
            Destination::Data,
            &mut file,
        )?;
        write_degraded_footer(err, format, degraded)?;
    } else {
        render_results(result, format, zone, severity_columns, destination, out)?;
        write_degraded_footer(out, format, degraded)?;
    }

    Ok(())
}

fn render_results(
    result: &mut QueryResult,
    format: OutputFormat,
    zone: DisplayZone,
    severity_columns: &[String],
    destination: Destination,
    out: &mut impl Write,
) -> io::Result<()> {
    match format {
        OutputFormat::Table => render_table(result, zone, severity_columns, out),
        OutputFormat::Json => {
            trawl_api::zone::apply_zone(result, zone);
            render_ndjson(result, destination, out)
        }
        OutputFormat::Csv => {
            trawl_api::zone::apply_zone(result, zone);
            render_csv(result, destination, out)
        }
        OutputFormat::Parquet => unreachable!("handled above"),
    }
}

/// Table output only. json/csv/parquet carry `degraded_fields` on the wire,
/// and a prose line inside a machine format is a parse error waiting to
/// happen.
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

pub async fn run_validate(query: &str, conn: Option<ConnectionParams>) -> Result<(), CliError> {
    if let Some(conn) = conn {
        let client = conn.client()?;
        let response = client.validate(query).await?;
        if response.valid {
            write_valid(&mut std::io::stdout().lock(), &response)?;
            if let Ok(parsed) = trawl_core::parser::parse_unbuilt(query) {
                let _ = write_advice(
                    &mut io::stderr().lock(),
                    query,
                    &trawl_core::advice::advise(&parsed),
                );
            }
        } else {
            render_error_details(query, &response.errors);
            return Err(CliError::Usage("query validation failed".into()));
        }
    } else {
        let (errors, advice) = match trawl_core::parser::parse(query) {
            Ok(parsed) => match trawl_core::emitter::validate_pipeline(&parsed.pipeline) {
                Ok(()) => (Vec::new(), Some(trawl_core::advice::advise(&parsed))),
                Err(e) => (e.to_parse_errors(query.len()), None),
            },
            Err(errors) => (errors, None),
        };
        if errors.is_empty() {
            println!("valid");
            if let Some(advice) = advice {
                let _ = write_advice(&mut io::stderr().lock(), query, &advice);
            }
        } else {
            let details: Vec<_> = errors.iter().map(parse_error_to_detail).collect();
            render_error_details(query, &details);
            return Err(CliError::Usage("query validation failed".into()));
        }
    }

    Ok(())
}

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

async fn run_parquet_export(
    query: &str,
    data: Option<&str>,
    conn: Option<&ConnectionParams>,
    output_path: &Path,
) -> Result<(), CliError> {
    if let Some(data) = data {
        let executor = trawl_engine::executor::Executor::new()?;
        executor.export_parquet(
            query,
            data,
            &trawl_core::schema::FieldTypes::new(),
            output_path,
            usize::MAX,
        )?;
    } else if let Some(conn) = conn {
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

async fn run_daemon_mode(
    conn: &ConnectionParams,
    query: &str,
) -> Result<(QueryResult, Vec<String>, Vec<String>), CliError> {
    let client = conn.client()?;
    let response = client.query_paginated(query, None, None).await?;
    Ok(daemon_outcome(response))
}

fn daemon_outcome(
    response: trawl_client::QueryResponse,
) -> (QueryResult, Vec<String>, Vec<String>) {
    (
        response.result,
        response.degraded_fields,
        response.severity_columns,
    )
}

fn run_embedded_mode(data: &str, query: &str) -> Result<QueryResult, CliError> {
    let executor = trawl_engine::executor::Executor::new()?;
    Ok(executor.run_query(
        query,
        data,
        &trawl_core::schema::FieldTypes::new(),
        usize::MAX,
    )?)
}

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

pub(crate) fn severity_token(v: &Value) -> Option<&'static str> {
    match v {
        Value::Integer(n) => trawl_core::severity::token_text(*n),
        _ => None,
    }
}

fn severity_cell_text(v: &Value) -> String {
    severity_token(v).map_or_else(|| v.to_string(), str::to_owned)
}

fn render_ndjson(
    result: &QueryResult,
    destination: Destination,
    out: &mut impl Write,
) -> io::Result<()> {
    for row in &result.rows {
        let mut map = serde_json::Map::new();
        for (col, val) in result.columns.iter().zip(row.iter()) {
            map.insert(col.name.clone(), val.to_plain_json());
        }
        write_json_line(out, &map, destination)?;
    }
    Ok(())
}

fn render_csv(
    result: &QueryResult,
    destination: Destination,
    out: &mut impl Write,
) -> io::Result<()> {
    let headers: Vec<String> = result
        .columns
        .iter()
        .map(|c| csv_text(&c.name, destination))
        .collect();
    writeln!(out, "{}", headers.join(","))?;

    for row in &result.rows {
        let cells: Vec<String> = row
            .iter()
            .map(|v| csv_escape_value(v, destination))
            .collect();
        writeln!(out, "{}", cells.join(","))?;
    }
    Ok(())
}

fn csv_escape_value(val: &Value, destination: Destination) -> String {
    match val {
        Value::Null => String::new(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        // full precision for data fidelity (TUI display uses 2dp for readability)
        Value::Float(f) => f.to_string(),
        Value::String(s) => csv_text(s, destination),
        Value::Array(_) => csv_text(&val.to_plain_json().to_string(), destination),
    }
}

/// On a terminal each unsafe character, a line break included, becomes
/// U+FFFD first, so formula protection and quoting see the text the
/// operator will.
fn csv_text(text: &str, destination: Destination) -> String {
    match destination {
        Destination::Terminal => csv_escape_string(&sanitize_display_text(text)),
        Destination::Data => csv_escape_string(text),
    }
}

fn csv_escape_string(s: &str) -> String {
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

pub fn render_error_details(query: &str, details: &[trawl_client::ErrorDetail]) {
    let _ = write_error_details(&mut io::stderr().lock(), query, details);
}

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
            write_caret_excerpt(out, query, &at)?;
        }
        writeln!(out, "  {}", sanitize_display_text(&detail.message))?;
        if let Some(hint) = &detail.hint {
            writeln!(out, "  hint: {}", sanitize_display_text(hint))?;
        }
        writeln!(out)?;
    }
    Ok(())
}

fn write_caret_excerpt(
    out: &mut impl Write,
    query: &str,
    at: &trawl_core::text_layout::SpanLine,
) -> io::Result<()> {
    let layout = caret_layout(query, at);
    writeln!(out)?;
    writeln!(out, "  {}{}{}", layout.before, layout.covered, layout.after)?;
    writeln!(
        out,
        "  {}{}",
        " ".repeat(layout.pad),
        "~".repeat(layout.carets)
    )
}

fn render_query_advice(query: &str, parsed: Option<&trawl_core::ast::Query>) {
    let Some(parsed) = parsed else { return };
    if trawl_core::emitter::validate_pipeline(&parsed.pipeline).is_err() {
        return;
    }
    let _ = write_advice(
        &mut io::stderr().lock(),
        query,
        &trawl_core::advice::advise(parsed),
    );
}

fn write_advice(
    out: &mut impl Write,
    query: &str,
    report: &trawl_core::advice::AdviceReport,
) -> io::Result<()> {
    for finding in &report.findings {
        if let Some(at) = locate_span(query, finding.span.start, finding.span.end) {
            write_caret_excerpt(out, query, &at)?;
        }
        writeln!(out, "  advice: {}", sanitize_display_text(&finding.message))?;
        writeln!(out, "  hint: {}", sanitize_display_text(&finding.hint))?;
        writeln!(out)?;
    }
    if report.omitted > 0 {
        let noun = if report.omitted == 1 { "note" } else { "notes" };
        writeln!(out, "  and {} more advice {noun} not shown", report.omitted)?;
        writeln!(out)?;
    }
    Ok(())
}

pub fn render_client_error(query: &str, err: &trawl_client::ClientError) {
    let _ = write_client_error(&mut io::stderr().lock(), query, err);
}

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

pub fn render_engine_error(query: &str, err: &trawl_engine::error::EngineError) {
    let _ = write_engine_error(&mut io::stderr().lock(), query, err);
}

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

pub fn render_driver_results(
    columns: &[String],
    rows: &[Vec<serde_json::Value>],
    format: OutputFormat,
    destination: Destination,
    out: &mut impl Write,
) -> io::Result<()> {
    match format {
        OutputFormat::Table => render_driver_table(columns, rows, out),
        OutputFormat::Json => render_driver_ndjson(columns, rows, destination, out),
        OutputFormat::Csv => render_driver_csv(columns, rows, destination, out),
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
    destination: Destination,
    out: &mut impl Write,
) -> io::Result<()> {
    for row in rows {
        let mut map = serde_json::Map::new();
        for (col, val) in columns.iter().zip(row.iter()) {
            map.insert(col.clone(), val.clone());
        }
        write_json_line(out, &map, destination)?;
    }
    Ok(())
}

fn render_driver_csv(
    columns: &[String],
    rows: &[Vec<serde_json::Value>],
    destination: Destination,
    out: &mut impl Write,
) -> io::Result<()> {
    let headers: Vec<String> = columns.iter().map(|c| csv_text(c, destination)).collect();
    writeln!(out, "{}", headers.join(","))?;

    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .map(|v| csv_escape_json(v, destination))
            .collect();
        writeln!(out, "{}", cells.join(","))?;
    }
    Ok(())
}

fn json_display(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "null".to_owned(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn csv_escape_json(v: &serde_json::Value, destination: Destination) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => csv_text(s, destination),
        other => csv_text(&other.to_string(), destination),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            histogram: None,
        }
    }

    #[test]
    fn the_daemon_note_reaches_the_terminal_and_never_the_deliverable() {
        let (result, degraded, severity) = daemon_outcome(mocked_response());
        assert_eq!(degraded, ["duration"], "degraded_fields must survive");
        assert!(severity.is_empty());

        let emit_with = |result: &QueryResult, format, output: Option<&Path>, destination| {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            emit_results(
                &mut result.clone(),
                format,
                output,
                DisplayZone::Utc,
                &degraded,
                &severity,
                destination,
                &mut out,
                &mut err,
            )
            .unwrap();
            (
                String::from_utf8(out).unwrap(),
                String::from_utf8(err).unwrap(),
            )
        };
        let emit =
            |format, output: Option<&Path>| emit_with(&result, format, output, Destination::Data);

        let (out, err) = emit(OutputFormat::Table, None);
        assert!(out.contains("db1"), "{out}");
        assert!(out.contains("note: results may be incomplete"), "{out}");
        assert!(err.is_empty(), "{err}");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let (out, err) = emit(OutputFormat::Table, Some(&path));
        assert!(out.is_empty(), "{out}");
        assert!(err.contains("note: results may be incomplete"), "{err}");
        let file = std::fs::read_to_string(&path).unwrap();
        assert!(file.contains("db1"), "{file}");
        assert!(!file.contains("note:"), "{file}");

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

        let mut hostile = result.clone();
        hostile
            .rows
            .push(vec![Value::String("x\u{1b}[2J\u{202e}\ny".to_owned())]);
        for format in [OutputFormat::Json, OutputFormat::Csv] {
            let (piped, _) = emit_with(&hostile, format, None, Destination::Data);
            assert!(piped.contains('\u{202e}'), "{format:?}: {piped:?}");
            let path = dir.path().join("hostile.out");
            let (out, err) = emit_with(&hostile, format, Some(&path), Destination::Terminal);
            assert!(out.is_empty(), "{out}");
            assert!(err.is_empty(), "{err}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), piped, "{format:?}");
        }
        let path = dir.path().join("hostile.txt");
        emit_with(
            &hostile,
            OutputFormat::Table,
            Some(&path),
            Destination::Terminal,
        );
        let file = std::fs::read_to_string(&path).unwrap();
        assert!(!file.contains(['\u{1b}', '\u{202e}']), "{file:?}");
        assert!(file.contains("x\u{FFFD}[2J\u{FFFD}"), "{file}");
    }

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
        let render = |f: fn(&QueryResult, Destination, &mut Vec<u8>) -> io::Result<()>| {
            let mut buf = Vec::new();
            f(&result, Destination::Data, &mut buf).unwrap();
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

    #[test]
    fn a_timestamp_column_named_like_severity_still_shows_in_the_display_zone() {
        use trawl_engine::value::{Column, ColumnType};
        let mut result = QueryResult {
            columns: vec![Column::new("_severity", ColumnType::Timestamp)],
            rows: vec![vec![Value::String("2026-10-25T00:30:00.000000Z".into())]],
        };
        let zone = "+02:00".parse::<DisplayZone>().unwrap();
        let mut buf = Vec::new();
        render_results(
            &mut result,
            OutputFormat::Table,
            zone,
            &[],
            Destination::Data,
            &mut buf,
        )
        .unwrap();
        let table = String::from_utf8(buf).unwrap();
        assert!(table.contains("2026-10-25 02:30:00 +02:00"), "{table}");
    }

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
            render_results(
                &mut result.clone(),
                format,
                zone,
                &[],
                Destination::Data,
                &mut buf,
            )
            .unwrap();
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

    #[test]
    fn an_invalid_display_zone_is_a_usage_error_naming_it() {
        assert_eq!(display_zone(" local ").unwrap(), DisplayZone::Local);
        let err = display_zone("Mars/Olympus").unwrap_err();
        assert!(matches!(err, CliError::Usage(_)), "{err:?}");
        assert!(err.to_string().contains("\"Mars/Olympus\""), "{err}");
    }

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

        for f in [
            render_ndjson as fn(&QueryResult, Destination, &mut Vec<u8>) -> io::Result<()>,
            render_csv,
        ] {
            let mut buf = Vec::new();
            f(&result, Destination::Data, &mut buf).unwrap();
            let text = String::from_utf8(buf).unwrap();
            assert!(!text.contains("error2"), "{text}");
        }
    }

    #[test]
    fn the_embedded_lane_declares_sev_columns_without_a_catalog() {
        let embedded_severity_columns = |query: &str| {
            super::embedded_severity_columns(trawl_core::parser::parse(query).ok().as_ref())
        };
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
        assert_eq!(
            csv_escape_value(&Value::Integer(-42), Destination::Data),
            "-42"
        );
    }

    #[test]
    fn csv_negative_float_not_prefixed() {
        assert_eq!(
            csv_escape_value(&Value::Float(-1.5), Destination::Data),
            "-1.5"
        );
    }

    #[test]
    fn csv_formula_string_prefixed() {
        assert_eq!(
            csv_escape_value(&Value::String("=cmd".into()), Destination::Data),
            "'=cmd"
        );
    }

    #[test]
    fn csv_tab_prefixed() {
        assert_eq!(
            csv_escape_value(&Value::String("\tfoo".into()), Destination::Data),
            "'\tfoo"
        );
    }

    #[test]
    fn csv_at_sign_prefixed() {
        assert_eq!(
            csv_escape_value(&Value::String("@sum".into()), Destination::Data),
            "'@sum"
        );
    }

    #[test]
    fn csv_pipe_prefixed() {
        assert_eq!(
            csv_escape_value(&Value::String("|cmd".into()), Destination::Data),
            "'|cmd"
        );
    }

    #[test]
    fn csv_null_empty() {
        assert_eq!(csv_escape_value(&Value::Null, Destination::Data), "");
    }

    #[test]
    fn csv_string_with_comma() {
        assert_eq!(
            csv_escape_value(&Value::String("a,b".into()), Destination::Data),
            "\"a,b\""
        );
    }

    #[test]
    fn csv_string_with_quotes() {
        assert_eq!(
            csv_escape_value(&Value::String(r#"say "hi""#.into()), Destination::Data),
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

    const NO_EXCERPT: &str = "  the message\n  hint: the hint\n\n";

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

    fn unsafe_chars(text: &str) -> usize {
        text.chars()
            .filter(|&c| c != '\n' && trawl_core::sanitize::is_unsafe_display_char(c))
            .count()
    }

    fn cell_lines(table: &str) -> Vec<&str> {
        table
            .lines()
            .filter_map(|line| line.strip_prefix('│')?.strip_suffix('│'))
            .flat_map(|line| line.split('┆'))
            .map(|cell| cell.trim_matches(' '))
            .collect()
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "both table renderers share one fixture"
    )]
    fn terminal_table_sanitizes_sender_text() {
        use trawl_engine::value::{Column, ColumnType};

        let header = "n\u{1b}[2J\u{202e}\u{9b}";
        let header_shown = "n\u{FFFD}[2J\u{FFFD}\u{FFFD}";

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

        // json_display spells the object as compact JSON, which escapes C0
        // controls but leaves C1, bidi and format characters raw.
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
        render_driver_results(
            &columns,
            &rows,
            OutputFormat::Table,
            Destination::Data,
            &mut buf,
        )
        .unwrap();
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
            Destination::Data,
            &mut buf,
        )
        .unwrap();
        let table = String::from_utf8(buf).unwrap();
        assert!(!table.contains('\r'), "{table:?}");
        assert_eq!(cell_lines(&table), ["text", "a", "b", "a", "b"], "{table}");
    }

    #[test]
    fn terminal_cli_lines_sanitize_sender_text() {
        let hostile = "v\u{1b}[2J\u{202e}\nx";
        let shown = "v\u{FFFD}[2J\u{FFFD}\u{FFFD}x";

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

        let refused = trawl_engine::error::EngineError::Refused {
            message: hostile.to_owned(),
        };
        let mut buf = Vec::new();
        write_engine_error(&mut buf, "q", &refused).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), format!("trawl: {shown}\n"));

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

    fn every_unsafe_group() -> Vec<String> {
        use trawl_core::sanitize::is_unsafe_display_char;
        let groups: Vec<String> = [
            '\u{0}'..='\u{1f}',
            '\u{7f}'..='\u{9f}',
            '\u{ad}'..='\u{ad}',
            '\u{61c}'..='\u{61c}',
            '\u{200b}'..='\u{200f}',
            '\u{2028}'..='\u{202e}',
            '\u{2060}'..='\u{2060}',
            '\u{2066}'..='\u{2069}',
            '\u{feff}'..='\u{feff}',
        ]
        .into_iter()
        .map(Iterator::collect)
        .collect();
        let all = groups.concat();
        assert!(all.chars().all(is_unsafe_display_char), "{all:?}");
        for c in (0..=0x0010_ffff)
            .filter_map(char::from_u32)
            .filter(|&c| is_unsafe_display_char(c))
        {
            assert!(all.contains(c), "{c:?} is in no group");
        }
        groups.iter().map(|group| format!("<{group}>")).collect()
    }

    fn json_texts() -> Vec<String> {
        let mut texts = every_unsafe_group();
        texts.extend(
            [
                "q\"t",
                "b\\s",
                "\\u202e",
                "\\\u{202e}",
                "🦀",
                "e\u{1b}f",
                "c\u{9b}d",
                "r\u{202e}l",
                "n\nl",
            ]
            .map(str::to_owned),
        );
        texts
    }

    fn assert_terminal_json(
        data: &[u8],
        terminal: &[u8],
        maps: &[serde_json::Map<String, serde_json::Value>],
    ) {
        use trawl_core::sanitize::is_unsafe_display_char;
        let mut expected = Vec::new();
        for map in maps {
            serde_json::to_writer(&mut expected, map).unwrap();
            expected.push(b'\n');
        }
        assert!(data == expected, "data bytes moved: {data:?}");

        let terminal = std::str::from_utf8(terminal).unwrap();
        assert!(terminal.ends_with('\n'), "{terminal:?}");
        let lines: Vec<&str> = terminal.split_terminator('\n').collect();
        assert_eq!(lines.len(), maps.len(), "{terminal:?}");
        for (line, map) in lines.iter().zip(maps) {
            assert!(!line.chars().any(is_unsafe_display_char), "{line:?}");
            let decoded: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(decoded, serde_json::Value::Object(map.clone()), "{line}");
        }
        for escaped in [
            r#""e\u001bf""#,
            r#""c\u009bd""#,
            r#""r\u202el""#,
            r#""n\u000al""#,
            r#""\\\u202e""#,
            r#""\\u202e""#,
            r#""q\"t""#,
            r#""b\\s""#,
            "\"🦀\"",
        ] {
            assert!(terminal.contains(escaped), "{escaped} missing: {terminal}");
        }
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the query, driver and status renderers share one fixture"
    )]
    fn terminal_json_escapes_without_changing_values() {
        use serde_json::{Map, Value as Json, json};
        use trawl_core::sanitize::is_unsafe_display_char;
        use trawl_engine::value::{Column, ColumnType};

        let texts = json_texts();
        let scalars = [
            ("null", Value::Null, Json::Null),
            ("yes", Value::Boolean(true), json!(true)),
            ("no", Value::Boolean(false), json!(false)),
            ("neg", Value::Integer(-1), json!(-1)),
            ("max", Value::UInt(u64::MAX), json!(u64::MAX)),
        ];
        let names: Vec<String> = texts
            .iter()
            .map(|t| format!("k{t}"))
            .chain(scalars.iter().map(|(name, ..)| (*name).to_owned()))
            .collect();

        let result = QueryResult {
            columns: names
                .iter()
                .map(|n| Column::new(n.as_str(), ColumnType::String))
                .collect(),
            rows: vec![
                texts
                    .iter()
                    .map(|t| Value::String(t.clone()))
                    .chain(scalars.iter().map(|(_, v, _)| v.clone()))
                    .collect(),
                texts
                    .iter()
                    .map(|t| {
                        let inner = vec![Value::String(t.clone()), Value::Null];
                        Value::Array(vec![Value::String(t.clone()), Value::Array(inner)])
                    })
                    .chain(scalars.iter().map(|(_, v, _)| v.clone()))
                    .collect(),
            ],
        };
        let maps: Vec<Map<String, Json>> = result
            .rows
            .iter()
            .map(|row| {
                names
                    .iter()
                    .cloned()
                    .zip(row.iter().map(Value::to_plain_json))
                    .collect()
            })
            .collect();
        let render = |destination| {
            let mut buf = Vec::new();
            render_ndjson(&result, destination, &mut buf).unwrap();
            buf
        };
        assert_terminal_json(
            &render(Destination::Data),
            &render(Destination::Terminal),
            &maps,
        );

        let nested = |t: &str| {
            let mut inner = Map::new();
            inner.insert(format!("d{t}"), json!(t));
            let mut outer = Map::new();
            outer.insert(format!("n{t}"), json!([t, inner]));
            Json::Object(outer)
        };
        let rows: Vec<Vec<Json>> = vec![
            texts
                .iter()
                .map(|t| json!(t))
                .chain(scalars.iter().map(|(.., j)| j.clone()))
                .collect(),
            texts
                .iter()
                .map(|t| nested(t))
                .chain(scalars.iter().map(|(.., j)| j.clone()))
                .collect(),
        ];
        let maps: Vec<Map<String, Json>> = rows
            .iter()
            .map(|row| names.iter().cloned().zip(row.iter().cloned()).collect())
            .collect();
        let render = |destination| {
            let mut buf = Vec::new();
            render_driver_results(&names, &rows, OutputFormat::Json, destination, &mut buf)
                .unwrap();
            buf
        };
        assert_terminal_json(
            &render(Destination::Data),
            &render(Destination::Terminal),
            &maps,
        );

        let status = crate::tui::driver::DriverData {
            query: Some(texts.concat()),
            result_columns: Some(names.clone()),
            live_mode: Some(true),
            result_rows: Some(rows.len()),
            columns: Some(names.clone()),
            rows: Some(rows.clone()),
            ..Default::default()
        };
        let data = crate::terminal::json_pretty(&status, Destination::Data).unwrap();
        assert_eq!(data, serde_json::to_string_pretty(&status).unwrap());
        let shown = crate::terminal::json_pretty(&status, Destination::Terminal).unwrap();
        assert_eq!(
            serde_json::from_str::<Json>(&shown).unwrap(),
            serde_json::to_value(&status).unwrap()
        );
        let indents = |text: &str| -> Vec<usize> {
            text.split('\n')
                .map(|line| line.len() - line.trim_start_matches(' ').len())
                .collect()
        };
        assert!(data.lines().count() > 1, "{data}");
        assert_eq!(indents(&shown), indents(&data));
        for line in shown.split('\n') {
            assert!(!line.chars().any(is_unsafe_display_char), "{line:?}");
        }
    }

    #[test]
    fn terminal_csv_replaces_unsafe_characters() {
        use serde_json::json;
        use trawl_engine::value::{Column, ColumnType};

        let text = |t: &str| (Value::String(t.to_owned()), json!(t));
        // Each value as a query cell and as driver JSON, then what a pipe
        // gets, then what a terminal gets.
        let cases = [
            (text("e\u{1b}[2Jx"), "e\u{1b}[2Jx", "e\u{FFFD}[2Jx"),
            (text("r\u{202e}l"), "r\u{202e}l", "r\u{FFFD}l"),
            (text("c\u{9b}1m"), "c\u{9b}1m", "c\u{FFFD}1m"),
            (text("a\nb"), "\"a\nb\"", "a\u{FFFD}b"),
            (text("a\r\nb"), "\"a\r\nb\"", "a\u{FFFD}\u{FFFD}b"),
            (text("a\rb"), "\"a\rb\"", "a\u{FFFD}b"),
            (text("a\tb"), "a\tb", "a\u{FFFD}b"),
            (text("\tx"), "'\tx", "\u{FFFD}x"),
            (text("=cmd"), "'=cmd", "'=cmd"),
            (text("+1"), "'+1", "'+1"),
            (text("-1"), "'-1", "'-1"),
            ((Value::Integer(-1), json!(-1)), "-1", "-1"),
            ((Value::Float(-1.5), json!(-1.5)), "-1.5", "-1.5"),
            (text("@x"), "'@x", "'@x"),
            (text("|x"), "'|x", "'|x"),
            ((Value::Null, json!(null)), "", ""),
            (text("null"), "null", "null"),
            ((Value::Boolean(true), json!(true)), "true", "true"),
            (text("a,\u{202e}"), "\"a,\u{202e}\"", "\"a,\u{FFFD}\""),
            (
                (
                    Value::Array(vec![Value::String("q\u{202e}\"".to_owned())]),
                    json!(["q\u{202e}\""]),
                ),
                "\"[\"\"q\u{202e}\\\"\"\"\"]\"",
                "\"[\"\"q\u{FFFD}\\\"\"\"\"]\"",
            ),
        ];
        let header = "h\u{1b}[2J\u{202e}";
        let lines = |head: &str, cells: &mut dyn Iterator<Item = &str>| {
            let mut text = format!("{head}\n");
            for cell in cells {
                text.push_str(cell);
                text.push('\n');
            }
            text
        };
        let data = lines(header, &mut cases.iter().map(|c| c.1));
        let terminal = lines("h\u{FFFD}[2J\u{FFFD}", &mut cases.iter().map(|c| c.2));
        assert_eq!(unsafe_chars(&terminal), 0, "{terminal:?}");
        assert_eq!(terminal.matches('\n').count(), cases.len() + 1);

        let result = QueryResult {
            columns: vec![Column::new(header, ColumnType::String)],
            rows: cases.iter().map(|c| vec![c.0.0.clone()]).collect(),
        };
        let render = |destination| {
            let mut buf = Vec::new();
            render_csv(&result, destination, &mut buf).unwrap();
            String::from_utf8(buf).unwrap()
        };
        assert_eq!(render(Destination::Data), data);
        assert_eq!(render(Destination::Terminal), terminal);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.csv");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        emit_results(
            &mut result.clone(),
            OutputFormat::Csv,
            Some(&path),
            DisplayZone::Utc,
            &[],
            &[],
            Destination::Terminal,
            &mut out,
            &mut err,
        )
        .unwrap();
        assert!(out.is_empty() && err.is_empty(), "{out:?} {err:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), data);

        let mut rows: Vec<Vec<serde_json::Value>> =
            cases.iter().map(|c| vec![c.0.1.clone()]).collect();
        rows.push(vec![json!({ "k\u{202e}": "v" })]);
        let render = |destination| {
            let mut buf = Vec::new();
            render_driver_results(
                &[header.to_owned()],
                &rows,
                OutputFormat::Csv,
                destination,
                &mut buf,
            )
            .unwrap();
            String::from_utf8(buf).unwrap()
        };
        assert_eq!(
            render(Destination::Data),
            format!("{data}\"{{\"\"k\u{202e}\"\":\"\"v\"\"}}\"\n")
        );
        assert_eq!(
            render(Destination::Terminal),
            format!("{terminal}\"{{\"\"k\u{FFFD}\"\":\"\"v\"\"}}\"\n")
        );
    }
}
