// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Error types for the query engine.

use trawl_core::parser::ParseError;

/// Errors that can occur during query execution.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// One or more parse errors in the DSL query.
    #[error("parse error: {}", format_parse_errors(.0))]
    Parse(Vec<ParseError>),

    /// The emitter could not produce valid SQL from the AST.
    #[error("emit error: {0}")]
    Emit(#[from] trawl_core::emitter::EmitError),

    /// `DuckDB` returned an error during execution, other than running out
    /// of the query budgets ([`Self::ResourceExhausted`]).
    ///
    /// `From<duckdb::Error>` classifies by the structured diagnostic's
    /// `exception_type` and lands every type but `Out of Memory` here. The
    /// message renders as `DuckDB`'s own `<type> Error: <message>` text.
    #[error("database error: {}", render_duckdb_error(.0))]
    Database(#[source] duckdb::Error),

    /// The query ran out of the query database's memory or spill budget:
    /// `DuckDB` reported an `Out of Memory` exception (ADR-0056). Both
    /// `max_temp_directory_size` and `memory_limit` raise that type.
    ///
    /// Carries nothing from `DuckDB`: its message names sizes, and the
    /// spill path can appear in it, so neither crosses into this error's
    /// text. The read lanes never retry it hot-only: the budgets are shared
    /// by every query on the database, so a retry would only spend them
    /// again.
    #[error(
        "query could not complete with the available query resources; \
         narrow the time range or simplify the query"
    )]
    ResourceExhausted,

    /// Query produced more rows than the configured limit.
    #[error("result too large: query returned more than {0} rows")]
    ResultTooLarge(usize),

    /// A read matched no files even though its source still reaches files on
    /// disk — the empty answer would silently drop the cold data, which
    /// ADR-0008 forbids. Raised by every lane (query and export, with a hot
    /// buffer and without), because the source is resolved before the read
    /// on all of them and the gate is the same. Transient by nature: the read
    /// raced retention/compaction/a repin moving a file, or the hot snapshot
    /// vanished.
    #[error("query matched no files while cold data exists on disk; retry the query")]
    ColdDataUnread,

    /// The query is wrong in a way the engine proved by asking `DuckDB`
    /// a question of its own, and no fallback can make it right.
    ///
    /// Distinct from [`Self::Emit`], which the hot lanes treat as a
    /// missing column: a column absent from cold parquet but present in
    /// a hot snapshot is a real query the cold read could not answer,
    /// so those lanes retry hot-only and answer an unanswerable one with
    /// an empty result (ADR-0009). A refusal is not that. Retrying it
    /// against a narrower source produces the same refusal, and turning
    /// it into an empty result would answer 200 with zero rows to a
    /// query that was never going to mean anything — which is how a
    /// `timechart` over a non-timestamp column escaped its own check.
    ///
    /// The message is trawl-authored and names the reader's own tokens,
    /// so it crosses the trust boundary intact, like a parse or emit
    /// refusal.
    #[error("{message}")]
    Refused { message: String },

    /// The query database refused a budget, or a clone read one back other
    /// than configured (ADR-0056). The structured-error setting
    /// (`errors_as_json`) is read back the same way, since without it no
    /// failure can classify as [`Self::ResourceExhausted`]. Raised only
    /// while the database opens, so trawld refuses to start rather than run
    /// without its bounds.
    #[error(
        "the query engine did not apply {setting}: configured {configured}, reads back {applied}"
    )]
    BudgetNotApplied {
        setting: &'static str,
        configured: String,
        applied: String,
    },

    /// Filesystem I/O error (e.g. writing temp files for parquet export).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The caller's cancellation latch was set at the bind-to-execute
    /// boundary, so this lane stopped before running the statement
    /// (ADR-0024). Content-free: which query, and who cancelled it, is
    /// the caller's own record.
    #[error("query cancelled")]
    Cancelled,
}

/// `DuckDB`'s exception type for running out of memory or spill space.
const OUT_OF_MEMORY: &str = "Out of Memory";

impl From<duckdb::Error> for EngineError {
    /// Classify a `DuckDB` error by its diagnostic's `exception_type`.
    ///
    /// `Out of Memory` is [`Self::ResourceExhausted`]. Everything else is
    /// [`Self::Database`]: another type, a message that is not a
    /// diagnostic (a connection without `errors_as_json`, or a malformed
    /// one), and every driver-side variant. The message text is never
    /// searched, so a message that says "Out of Memory" under another type
    /// stays a database error.
    fn from(error: duckdb::Error) -> Self {
        match DuckDbDiagnostic::of(&error) {
            Some(diagnostic) if diagnostic.exception_type == OUT_OF_MEMORY => {
                Self::ResourceExhausted
            }
            _ => Self::Database(error),
        }
    }
}

/// The structured diagnostic `DuckDB` reports in place of its message
/// text when `errors_as_json` is on: one flat JSON object of strings.
///
/// Every connection an [`crate::executor::Executor`] opens turns the
/// setting on before anything clones the database, so every error those
/// connections raise carries one. `exception_type` is `DuckDB`'s
/// `ExceptionTypeToString` name (`Out of Memory`, `Binder`, `IO`,
/// `Conversion`, ...). The other keys are the exception's extra info and
/// are ignored.
///
/// `DuckDB` 1.5.5 passes a raw message that already starts with `{`
/// through unchanged (`ErrorData::ConvertErrorToJSON`), so a statement
/// that controls its whole message, as `error('{...}')` does, can name
/// any type. Trawl's DSL cannot emit such a statement, and the worst it
/// could do is make the caller's own query answer as exhausted.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct DuckDbDiagnostic {
    pub(crate) exception_type: String,
    pub(crate) exception_message: String,
}

impl DuckDbDiagnostic {
    /// The diagnostic `error` carries, if it is a `DuckDB` failure whose
    /// message parses as one. A message that is not JSON, not an object,
    /// lacks either key, holds a non-string value or repeats a key gives
    /// `None`.
    pub(crate) fn of(error: &duckdb::Error) -> Option<Self> {
        let duckdb::Error::DuckDBFailure(_, Some(message)) = error else {
            return None;
        };
        serde_json::from_str(message).ok()
    }

    /// `DuckDB`'s text form of the same error, `<type> Error: <message>`,
    /// without the query-location context the text form appends.
    pub(crate) fn text(&self) -> String {
        format!("{} Error: {}", self.exception_type, self.exception_message)
    }
}

/// A `DuckDB` error as `DuckDB` would word it without `errors_as_json`.
fn render_duckdb_error(error: &duckdb::Error) -> String {
    DuckDbDiagnostic::of(error).map_or_else(|| error.to_string(), |d| d.text())
}

fn format_parse_errors(errors: &[ParseError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod structured_error_classification {
    use super::EngineError;

    /// A `DuckDB` failure whose message is `message`, as the driver builds
    /// one from a failed statement.
    fn failure(message: &str) -> duckdb::Error {
        duckdb::Error::DuckDBFailure(duckdb::ffi::Error::new(1), Some(message.to_owned()))
    }

    /// The error `sql` raises on a connection with `errors_as_json` set as
    /// given.
    fn live(sql: &str, json: bool) -> duckdb::Error {
        let conn = duckdb::Connection::open_in_memory().expect("duckdb");
        conn.execute_batch(&format!("SET errors_as_json = {json}"))
            .expect("errors_as_json");
        conn.execute_batch(sql).expect_err(sql)
    }

    #[test]
    fn an_out_of_memory_diagnostic_is_resource_exhausted() {
        let error = EngineError::from(failure(
            r#"{"exception_type":"Out of Memory","exception_message":"failed to offload data block of size 256.0 KiB (48.2 MiB/48.4 MiB used)"}"#,
        ));
        assert!(matches!(error, EngineError::ResourceExhausted), "{error:?}");
        let text = error.to_string();
        assert!(
            !text.contains("MiB") && !text.contains("offload"),
            "no DuckDB text crosses into the error: {text}"
        );
    }

    /// The words "Out of Memory" anywhere but the exception type do not
    /// classify: in another type's message, live and planted, or in a
    /// message that is not a diagnostic.
    #[test]
    fn out_of_memory_text_under_another_type_stays_a_database_error() {
        let hostile = [
            live(
                r#"SELECT * FROM "Out of Memory: failed to offload data block""#,
                true,
            ),
            live(
                "SELECT error('Out of Memory Error: failed to offload data block')",
                true,
            ),
            failure(
                r#"{"exception_type":"Binder","exception_message":"Out of Memory: failed to offload"}"#,
            ),
            failure(r#"{"exception_type":"Out of Memory ","exception_message":"trailing space"}"#),
            failure(r#"{"exception_type":"out of memory","exception_message":"case"}"#),
        ];
        for error in hostile {
            let shown = error.to_string();
            assert!(shown.contains("Out of Memory") || shown.contains("ut of memory"));
            let classified = EngineError::from(error);
            assert!(
                matches!(classified, EngineError::Database(_)),
                "{shown} classified as {classified:?}"
            );
        }
    }

    /// A message that is not a well-formed diagnostic stays a database
    /// error, whatever it says: text from a connection without
    /// `errors_as_json`, broken or partial JSON, a non-string type, a
    /// repeated key, and the driver's own variants.
    #[test]
    fn a_malformed_diagnostic_stays_a_database_error() {
        let malformed = [
            live(
                "SET memory_limit = '8MB'; SELECT length(string_agg(md5(i::VARCHAR), ',')) \
                 FROM range(2000000) t(i)",
                false,
            ),
            failure("Out of Memory Error: failed to offload data block"),
            failure(r#"{"exception_type":"Out of Memory""#),
            failure(r#"{"exception_type":"Out of Memory"}"#),
            failure(r#"{"exception_type":["Out of Memory"],"exception_message":"x"}"#),
            failure(r#"{"exception_type":"Out of Memory","exception_message":7}"#),
            failure(
                r#"{"exception_type":"Binder","exception_message":"x","exception_type":"Out of Memory"}"#,
            ),
            failure(r#"[{"exception_type":"Out of Memory","exception_message":"x"}]"#),
            failure(r#"{"exception_type":"Out of Memory","exception_message":"x"} trailing"#),
            duckdb::Error::DuckDBFailure(duckdb::ffi::Error::new(1), None),
            duckdb::Error::InvalidColumnName("Out of Memory".into()),
        ];
        for error in malformed {
            let shown = format!("{error:?}");
            let classified = EngineError::from(error);
            assert!(
                matches!(classified, EngineError::Database(_)),
                "{shown} classified as {classified:?}"
            );
        }
    }

    /// The text-form connection above really did run out of memory, so its
    /// generic classification is the missing diagnostic's doing.
    #[test]
    fn the_text_form_out_of_memory_is_duckdb_s_own() {
        let error = live(
            "SET memory_limit = '8MB'; SELECT length(string_agg(md5(i::VARCHAR), ',')) \
             FROM range(2000000) t(i)",
            false,
        );
        assert!(
            error.to_string().starts_with("Out of Memory Error: "),
            "{error}"
        );
    }

    /// A database error reads as `DuckDB`'s text form, not as the JSON the
    /// connection reported.
    #[test]
    fn a_database_error_renders_the_diagnostic_as_text() {
        let error = EngineError::from(live("SELECT nonexistent_col FROM (SELECT 1 AS x)", true));
        let text = error.to_string();
        assert!(
            text.starts_with("database error: Binder Error: Referenced column \"nonexistent_col\""),
            "{text}"
        );
    }
}
