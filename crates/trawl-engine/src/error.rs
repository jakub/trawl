// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Error types for the query engine.

use trawl_core::parser::ParseError;
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthChar as _;

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

    /// The Rust tail's aggregation would have dropped input at one of its
    /// caps (ADR-0001), so its answer would leave groups or values out.
    /// The batch answer has no channel to say it is partial, so the query
    /// is refused instead. The server answers it as `result_too_large`.
    ///
    /// The message is trawl-authored and names only the cap and its limit.
    #[error("result too large: {}; narrow the query", describe_cap(*.0))]
    AggregationCapped(trawl_core::stream::AggregationCap),

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

/// What passed an aggregation cap, with the cap's limit.
fn describe_cap(cap: trawl_core::stream::AggregationCap) -> String {
    use trawl_core::stream::AggregationCap;
    let limit = cap.limit();
    match cap {
        AggregationCap::Groups => format!("the aggregation has more than {limit} groups"),
        AggregationCap::DistinctValues => {
            format!("a dc() or values() has more than {limit} distinct values in one group")
        }
        AggregationCap::ExactValues => {
            format!("a median or percentile has more than {limit} values in one group")
        }
    }
}

/// `DuckDB`'s exception type for running out of memory or spill space.
const OUT_OF_MEMORY: &str = "Out of Memory";

/// `DuckDB`'s exception type for a statement stopped by an interrupt.
const INTERRUPT: &str = "INTERRUPT";

impl EngineError {
    /// Whether `DuckDB` stopped the statement because its connection was
    /// interrupted, read off the structured diagnostic's `exception_type`
    /// like every other classification here.
    ///
    /// The engine sample (ADR-0056) reads it to tell the interrupt it
    /// raised on its own overrunning probe from a failure of the engine.
    #[must_use]
    pub fn is_interrupt(&self) -> bool {
        matches!(self, Self::Database(error)
            if DuckDbDiagnostic::of(error).is_some_and(|d| d.exception_type == INTERRUPT))
    }
}

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
        if is_out_of_memory(&error) {
            Self::ResourceExhausted
        } else {
            Self::Database(error)
        }
    }
}

/// Whether `error` is `DuckDB` running out of memory or spill space: its
/// structured diagnostic names the `Out of Memory` exception type. The one
/// test [`EngineError::from`] classifies [`EngineError::ResourceExhausted`]
/// by, for callers that keep the `duckdb::Error`. The maintenance databases
/// (compaction, rollup, conform and repin) read it too: they turn on
/// `errors_as_json` for the same reason the query database does.
#[must_use]
pub fn is_out_of_memory(error: &duckdb::Error) -> bool {
    DuckDbDiagnostic::of(error).is_some_and(|d| d.exception_type == OUT_OF_MEMORY)
}

/// The structured diagnostic `DuckDB` reports in place of its message
/// text when `errors_as_json` is on: one flat JSON object of strings.
///
/// Every connection an [`crate::executor::Executor`] opens turns the
/// setting on before anything clones the database, so every error those
/// connections raise carries one. `exception_type` is `DuckDB`'s
/// `ExceptionTypeToString` name (`Out of Memory`, `Binder`, `IO`,
/// `Conversion`, ...). The other keys are the exception's extra info and
/// are ignored, except that [`locate`] reads `position`.
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
        parse_object(message)
    }

    /// `DuckDB`'s text form of the same error, `<type> Error: <message>`.
    /// It carries the query-location context the text form appends only
    /// once [`locate`] has put it back.
    pub(crate) fn text(&self) -> String {
        format!("{} Error: {}", self.exception_type, self.exception_message)
    }
}

/// `message` parsed as one JSON object of type `T`, or `None`.
///
/// The derive also takes a positional sequence for a struct, so
/// `["Out of Memory","x"]` would parse. Only an object reaches it, where
/// the derive refuses a repeated key.
fn parse_object<T: serde::de::DeserializeOwned>(message: &str) -> Option<T> {
    let json = message.trim_start_matches([' ', '\t', '\n', '\r']);
    if !json.starts_with('{') {
        return None;
    }
    serde_json::from_str(json).ok()
}

/// `error` with `DuckDB`'s query-location context put back into its
/// diagnostic, for the `statement` that raised it.
///
/// Without `errors_as_json`, `DuckDB` ends an error's message with the
/// statement line the error sits on and a caret under it
/// (`ErrorData::AddErrorLocation`). With it, the diagnostic carries only
/// the byte offset of that place, as its `position` key. The message
/// matchers in `executor` (no files, binder column) were written against
/// the text form: a binder error over a field whose name says "column"
/// matched on that line alone. So each lane puts the context back for the
/// statement it ran. The diagnostic keeps its `exception_type`, gains the
/// context at the end of its `exception_message`, as `DuckDB` adds it, and
/// drops its other keys, which nothing reads.
///
/// Every other error comes back unchanged: a driver variant, a message that
/// is not a diagnostic, one with no `position` or one that is not a decimal
/// string, and a position at or past the statement's end, where `DuckDB`
/// adds nothing either.
pub(crate) fn locate(error: duckdb::Error, statement: &str) -> duckdb::Error {
    match error {
        duckdb::Error::DuckDBFailure(code, Some(message)) => {
            let located = locate_message(&message, statement).unwrap_or(message);
            duckdb::Error::DuckDBFailure(code, Some(located))
        }
        other => other,
    }
}

/// The diagnostic `message` with its location context added, or `None`
/// when [`locate`] leaves it alone.
fn locate_message(message: &str, statement: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Position {
        position: String,
    }
    let diagnostic: DuckDbDiagnostic = parse_object(message)?;
    let Position { position } = parse_object(message)?;
    let context = query_location_context(statement, position.parse().ok()?)?;
    Some(
        serde_json::json!({
            "exception_type": diagnostic.exception_type,
            "exception_message": format!("{}\n\n{context}", diagnostic.exception_message),
        })
        .to_string(),
    )
}

/// The query-location context `DuckDB` 1.5.5 appends to an error's text
/// form: `LINE <n>: `, the statement line the error sits on, cut to about
/// 60 columns on each side of the error with `...` where it is cut, and a
/// line with a caret under the error. A port of
/// `QueryErrorContext::Format`. `None` where `DuckDB` appends nothing: an
/// error at or past the statement's end.
///
/// `DuckDB` walks the line in extended grapheme clusters, and gives each
/// cluster the render width of its first code point (utf8proc's
/// `charwidth`): two columns for an East Asian wide character or an emoji,
/// none for a control character, one for most else. A combining mark or a
/// joined emoji adds nothing to the cluster it sits in. Where the cut falls
/// decides whether a word after the error is on the line, so the binder
/// fallback's answer depends on measuring it the same way.
///
/// The tables here are `unicode-segmentation`'s and `unicode-width`'s,
/// not utf8proc's, which are Unicode 15.1. They agree on assigned
/// characters other than those added or reclassified since 15.1 and a few
/// that `unicode-width` measures by its own policy (the soft hyphen, the
/// line and paragraph separators, Arabic prepended marks, Hangul medial
/// vowels). They disagree on unassigned code points in wide blocks, which
/// `unicode-width` counts as two columns and utf8proc as one.
fn query_location_context(query: &str, error_location: usize) -> Option<String> {
    const MAX_LINE_RENDER_WIDTH: usize = 120;
    let bytes = query.as_bytes();
    if error_location >= bytes.len() {
        return None;
    }
    // The line the error sits on, and its number. A `\r` ends a line
    // unless a `\n` follows it before the error.
    let mut line_start = 0;
    let mut line_number = 1;
    for i in 0..error_location {
        let newline = match bytes[i] {
            b'\r' => i + 1 >= error_location || bytes[i + 1] != b'\n',
            b'\n' => true,
            _ => false,
        };
        if newline {
            line_number += 1;
            line_start = i + 1;
        }
    }
    let line_end = bytes[error_location..]
        .iter()
        .position(|&b| b == b'\n' || b == b'\r')
        .map_or(bytes.len(), |n| error_location + n);
    let line = query.get(line_start..line_end)?;

    // Each cluster's offset in the line, its render width, and whether the
    // line may be cut before it.
    let mut positions = Vec::new();
    let mut widths = Vec::new();
    let mut breaks = Vec::new();
    for (at, cluster) in line.grapheme_indices(true) {
        positions.push(at);
        widths.push(cluster.chars().next().and_then(char::width).unwrap_or(0));
        let lead = line.as_bytes()[at];
        breaks.push(is_operator(lead) || lead.is_ascii_whitespace() || lead == 0x0b);
    }
    let epos = positions
        .iter()
        .position(|&p| p >= error_location - line_start)
        .unwrap_or(0);

    // At most half the width before the error, cut where a break lies
    // within four clusters of the limit.
    let (mut from, mut to) = (line_start, line_end);
    let mut spos = 0;
    let mut truncate_beginning = false;
    let mut width = 0;
    for i in (1..=epos).rev() {
        width += widths[i];
        if width >= MAX_LINE_RENDER_WIDTH / 2 {
            let cut = (i.saturating_sub(4)..(i + 4).min(epos))
                .find(|&k| breaks[k])
                .unwrap_or(i);
            truncate_beginning = true;
            from += positions[cut];
            spos = cut;
            break;
        }
    }
    // And the same after it.
    let mut truncate_end = false;
    width = 0;
    for (i, w) in widths.iter().enumerate().skip(epos) {
        width += w;
        if width >= MAX_LINE_RENDER_WIDTH / 2 {
            let scan_from = if i > epos + 4 { i - 4 } else { epos };
            let cut = (scan_from..(i + 4).min(positions.len()))
                .find(|&k| breaks[k])
                .unwrap_or(i);
            truncate_end = true;
            to = line_start + positions[cut];
            break;
        }
    }

    let indicator = format!("LINE {line_number}: ");
    let begin = if truncate_beginning { "..." } else { "" };
    let end = if truncate_end { "..." } else { "" };
    let caret = widths[spos..epos].iter().sum::<usize>() + indicator.len() + begin.len();
    Some(format!(
        "{indicator}{begin}{}{end}\n{}^",
        query.get(from..to)?,
        " ".repeat(caret)
    ))
}

/// `DuckDB`'s `StringUtil::CharacterIsOperator`: ASCII punctuation other
/// than `_`.
fn is_operator(c: u8) -> bool {
    c != b'_' && c.is_ascii_punctuation()
}

/// A `DuckDB` error as `DuckDB` would word it without `errors_as_json`.
#[must_use]
pub fn render_duckdb_error(error: &duckdb::Error) -> String {
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
    use super::{DuckDbDiagnostic, EngineError};

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
    /// repeated key, an array (serde's derive takes a positional sequence
    /// for a struct), and the driver's own variants.
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
            failure(r#"["Out of Memory","x"]"#),
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

    /// A statement stopped through the connection's interrupt handle is an
    /// interrupt, and no other failure is: not another type, not the word
    /// in a message, not a message that is not a diagnostic.
    #[test]
    fn only_an_interrupt_diagnostic_is_an_interrupt() {
        let conn = duckdb::Connection::open_in_memory().expect("duckdb");
        conn.execute_batch("SET errors_as_json = true")
            .expect("errors_as_json");
        let handle = conn.interrupt_handle();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // `DuckDB` clears the flag when a statement begins, so one interrupt
        // could land before the statement and be lost: keep raising it
        // until the statement has stopped.
        let interrupter = std::thread::spawn({
            let done = std::sync::Arc::clone(&done);
            move || {
                while !done.load(std::sync::atomic::Ordering::SeqCst) {
                    handle.interrupt();
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        });
        let error = conn
            .query_row(
                "SELECT max(md5(range::VARCHAR)) FROM range(1000000000000)",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect_err("an interrupted statement fails");
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        interrupter.join().expect("interrupter");
        assert!(EngineError::from(error).is_interrupt());

        for other in [
            live("SELECT error('INTERRUPT')", true),
            failure(r#"{"exception_type":"Binder","exception_message":"INTERRUPT"}"#),
            failure("INTERRUPT Error: Interrupted!"),
        ] {
            let shown = other.to_string();
            assert!(!EngineError::from(other).is_interrupt(), "{shown}");
        }
        assert!(!EngineError::Cancelled.is_interrupt());
    }

    /// The error `sql` raises with `params` bound, rows drained, on a fresh
    /// connection with `errors_as_json` set as given.
    fn raised(sql: &str, params: &[&dyn duckdb::ToSql], json: bool) -> duckdb::Error {
        let conn = duckdb::Connection::open_in_memory().expect("duckdb");
        conn.execute_batch(&format!("SET errors_as_json = {json}"))
            .expect("errors_as_json");
        conn.prepare(sql)
            .and_then(|mut stmt| {
                let mut rows = stmt.query(params)?;
                while rows.next()?.is_some() {}
                Ok(())
            })
            .expect_err(sql)
    }

    /// With its place put back, a diagnostic reads exactly as `DuckDB`'s own
    /// text form of the same error: the statement line, cut where `DuckDB`
    /// cuts a long one, and the caret. Errors at bind and at execute, on
    /// short lines, long lines cut on either side or both, runs with no
    /// break to cut at, `\r\n` and lone `\r` line ends, tabs, and lines of
    /// one-column non-ASCII characters, two-column East Asian characters,
    /// combining marks, emoji (alone, with a skin tone, joined into one
    /// cluster, as flag pairs) and decomposed Hangul syllables, each on
    /// either side of the error.
    #[test]
    fn a_located_diagnostic_reads_as_duckdb_s_text_form() {
        let columns = |sep: &str, item: &dyn Fn(usize) -> String| {
            (0..40).map(item).collect::<Vec<_>>().join(sep)
        };
        let wide = columns(", ", &|i| format!("{i} AS c{i:02}"));
        let tabbed = columns(",\t", &|i| format!("{i}\tAS\tc{i:02}"));
        let accented = columns(", ", &|i| format!("{i} AS \"é{i:02}\""));
        let east_asian = columns(", ", &|i| format!("{i} AS \"界{i:02}\""));
        let run = "x".repeat(150);
        let mut graphemes = Vec::new();
        for cluster in [
            "界",
            "e\u{301}",
            "a\u{301}\u{323}\u{308}",
            "😀",
            "👍🏽",
            "👨\u{200d}👩\u{200d}👧",
            "🇨🇿",
            "\u{1100}\u{1161}\u{11a8}",
            "❤\u{fe0f}",
        ] {
            let ident = cluster.repeat(70);
            graphemes.push(format!(
                "SELECT 1 AS \"{ident}\", nope FROM (SELECT 1 AS x)"
            ));
            graphemes.push(format!(
                "SELECT nope, 1 AS \"{ident}\" FROM (SELECT 1 AS x)"
            ));
            graphemes.push(format!(
                "SELECT 1 AS \"{ident}\", nope, 2 AS \"{ident}\" FROM (SELECT 1 AS x)"
            ));
        }
        let five: &dyn duckdb::ToSql = &5_i64;
        let cases: Vec<(String, Vec<&dyn duckdb::ToSql>)> = vec![
            ("SELECT nonexistent_col FROM (SELECT 1 AS x)".into(), vec![]),
            (
                "SELECT 1 AS a,\n  2 AS b\nFROM (SELECT 1 AS x)\nWHERE nonexistent_col = 1".into(),
                vec![],
            ),
            (
                "SELECT 1\r\nFROM (SELECT 1 AS x)\r\nWHERE nope = 1".into(),
                vec![],
            ),
            (
                "SELECT 1\rFROM (SELECT 1 AS x)\rWHERE nope = 1".into(),
                vec![],
            ),
            (format!("SELECT {wide}, nope FROM (SELECT 1 AS x)"), vec![]),
            (format!("SELECT nope, {wide} FROM (SELECT 1 AS x)"), vec![]),
            (
                format!("SELECT {wide}, nope, {wide} FROM (SELECT 1 AS x)"),
                vec![],
            ),
            (
                format!("SELECT 1 AS {run}, nope{run} FROM (SELECT 1 AS x)"),
                vec![],
            ),
            (
                format!("SELECT {tabbed},\tnope FROM (SELECT 1 AS x)"),
                vec![],
            ),
            (
                format!("SELECT {accented}, nope FROM (SELECT 1 AS x)"),
                vec![],
            ),
            (
                format!("SELECT {east_asian}, nope FROM (SELECT 1 AS x)"),
                vec![],
            ),
            (
                format!("SELECT nope, {east_asian} FROM (SELECT 1 AS x)"),
                vec![],
            ),
            ("SELECT sum(h) FROM (SELECT 'a' AS h)".into(), vec![]),
            ("SELECT * FROM nonexistent_table".into(), vec![]),
            (
                "SELECT *\nFROM (SELECT 'a' AS h)\nWHERE h > ?".into(),
                vec![five],
            ),
        ];
        let cases = cases
            .into_iter()
            .chain(graphemes.into_iter().map(|sql| (sql, vec![])));
        for (sql, params) in &cases.collect::<Vec<_>>() {
            let text = raised(sql, params, false).to_string();
            assert!(text.contains("\nLINE "), "{sql}: a located error: {text}");
            let located = super::locate(raised(sql, params, true), sql);
            let diagnostic = DuckDbDiagnostic::of(&located).expect("still a diagnostic");
            assert_eq!(diagnostic.text(), text, "{sql}");
        }
    }

    /// `locate` keeps the exception type, and leaves alone what it cannot
    /// place: a driver variant, text that is not a diagnostic, a diagnostic
    /// with no position, one whose position is not a decimal string or is
    /// repeated, and a position at or past the statement's end.
    #[test]
    fn locate_keeps_the_type_and_leaves_the_unplaceable_alone() {
        let statement = "SELECT 1";
        let oom = super::locate(
            failure(r#"{"exception_type":"Out of Memory","exception_message":"x","position":"7"}"#),
            statement,
        );
        assert!(oom.to_string().contains("LINE 1: SELECT 1"), "{oom}");
        assert!(matches!(
            EngineError::from(oom),
            EngineError::ResourceExhausted
        ));
        let hostile = super::locate(
            failure(
                r#"{"exception_type":"Binder","exception_message":"Out of Memory","position":"0"}"#,
            ),
            statement,
        );
        assert!(matches!(
            EngineError::from(hostile),
            EngineError::Database(_)
        ));

        for message in [
            "Binder Error: x",
            r#"{"exception_type":"Binder","exception_message":"x"}"#,
            r#"{"exception_type":"Binder","exception_message":"x","position":7}"#,
            r#"{"exception_type":"Binder","exception_message":"x","position":"seven"}"#,
            r#"{"exception_type":"Binder","exception_message":"x","position":"7","position":"0"}"#,
            r#"{"exception_type":"Binder","exception_message":"x","position":"8"}"#,
            r#"{"exception_type":"Binder","exception_message":"x","position":"80"}"#,
        ] {
            let located = super::locate(failure(message), statement);
            assert_eq!(located.to_string(), message);
        }
        let driver = super::locate(duckdb::Error::InvalidColumnName("SELECT".into()), statement);
        assert!(matches!(driver, duckdb::Error::InvalidColumnName(_)));
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
