//! Print what the embedded `--data` lane sends to `DuckDB` for one query.
//!
//! `reader <glob>` prints the cold reader the probe describes, so the
//! caller can time `DESCRIBE SELECT * FROM <reader>` on the same text.
//!
//! `emit <glob> <dsl> <columns.tsv>` emits the query the way
//! `ResolvedSource::emit_cold` does for a plain glob, with no pins, over
//! the `name<TAB>type` rows the probe returned. It prints one JSON object:
//! the parameterized SQL, its parameters, and the same SQL with each `?`
//! replaced by its parameter as a literal, ready for `EXPLAIN`.

use std::process::ExitCode;

use trawl_core::context::EvalContext;
use trawl_core::emitter::{self, ColdColumns, InputColumns, SqlValue};
use trawl_core::schema::FieldTypes;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [mode, glob] if mode == "reader" => match emitter::source_reader(glob) {
            Ok(reader) => {
                println!("{reader}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&format!("{e:?}")),
        },
        [mode, glob, dsl, tsv] if mode == "emit" => emit(glob, dsl, tsv),
        _ => fail("usage: issue-333-emit-sql reader <glob> | emit <glob> <dsl> <columns.tsv>"),
    }
}

fn emit(glob: &str, dsl: &str, tsv: &str) -> ExitCode {
    let text = match std::fs::read_to_string(tsv) {
        Ok(text) => text,
        Err(e) => return fail(&format!("read {tsv}: {e}")),
    };
    let described: Vec<(String, String)> = text
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| line.split_once('\t'))
        .map(|(name, ty)| (name.to_owned(), ty.to_owned()))
        .collect();
    let columns = InputColumns {
        cold: ColdColumns::described(described),
        hot: None,
    };
    let query = match trawl_core::parser::parse(dsl) {
        Ok(query) => query,
        Err(e) => return fail(&format!("parse: {e:?}")),
    };
    let emitted = match emitter::emit_with_pins(
        &query,
        glob,
        &FieldTypes::new(),
        &columns,
        EvalContext::capture(),
    ) {
        Ok(emitted) => emitted,
        Err(e) => return fail(&format!("emit: {e:?}")),
    };
    let params: Vec<String> = emitted.params.iter().map(literal).collect();
    let out = serde_json::json!({
        "sql": emitted.sql,
        "params": params,
        "inlined": inline(&emitted.sql, &params),
        "rust_stages": emitted.rust_stages.len(),
    });
    println!("{out}");
    ExitCode::SUCCESS
}

/// A parameter as a SQL literal, quotes doubled.
fn literal(value: &SqlValue) -> String {
    match value {
        SqlValue::String(s) => format!("'{}'", s.replace('\'', "''")),
        other => other.to_string(),
    }
}

/// Replace each `?` outside a quoted literal or identifier, in order.
fn inline(sql: &str, params: &[String]) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut next = params.iter();
    let mut quote: Option<char> = None;
    for c in sql.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '?' => {
                out.push_str(next.next().map_or("?", String::as_str));
                continue;
            }
            None => {}
        }
        out.push(c);
    }
    out
}

fn fail(message: &str) -> ExitCode {
    eprintln!("{message}");
    ExitCode::FAILURE
}
