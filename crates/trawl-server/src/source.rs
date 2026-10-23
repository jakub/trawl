// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Source glob computation for parquet file selection.
//!
//! Narrows the `read_parquet()` source argument based on time filters
//! and service names extracted from the DSL query, avoiding full-directory
//! scans when possible.
//!
//! [`select`] computes the selection and [`compute_source`] renders it for
//! `/query`. A capture (ADR-0057) reads the same selection for the same
//! text, so it reads what `/query` of its recorded text reads, and
//! [`selection_bytes`] measures the part of it that can hold the
//! capture's window.

use std::io;
use std::path::Path;

use chrono::{DateTime, NaiveDateTime, Timelike as _, Utc};
use trawl_core::ast::{FieldFilter, FilterOp, FilterValue, Query, SearchToken};
use trawl_engine::cancel::CancelLatch;
use trawl_engine::error::EngineError;
use trawl_engine::parquet_stats::{ParquetStatsError, StatVal, TimestampUnit, read_file_stats};

/// Time filter padding (seconds) to account for WAL compaction delay.
/// Events ingested at time T may land in the hourly partition for T + ~20s,
/// so we widen the file selection window by one hour.
const TIME_FILTER_PADDING_SECS: u64 = 3600;

/// The file pattern that selects every service's file in a directory.
const ANY_SERVICE: &str = "*.parquet";

/// Extract an exact value for `field` from the search stage, if present.
///
/// Only returns `Some` for single-group queries with a simple equality
/// filter (`service=nginx`, `env=prod`). Multi-group (OR) queries can't
/// be narrowed safely, and glob/regex operators are also ignored.
fn extract_eq_filter<'a>(
    search: &'a trawl_core::ast::SearchStage,
    field_name: &str,
) -> Option<&'a str> {
    // Can't narrow when OR is involved — different groups may target different values.
    if search.groups.len() != 1 {
        return None;
    }
    search.groups[0].iter().find_map(|t| {
        if let SearchToken::FieldFilter(FieldFilter {
            field,
            op: FilterOp::Eq,
            value: FilterValue::Literal(s),
        }) = &t.node
            && field == field_name
        {
            return Some(s.as_str());
        }
        None
    })
}

/// Compute the `read_parquet()` source argument over the two path
/// dimensions (ADR-0009): `data/{env}/{date}/{HH}/{service}.parquet`,
/// day-level `data/{env}/{date}/{service}.parquet` after rollup.
///
/// `env=X` pins the outer directory; otherwise every env directory on
/// disk is searched. `service=X` narrows the file pattern verbatim (path
/// encoding is injective by validation, so `api.v2` and `api_v2` are
/// distinct files and pruning is exact), but only when the literal
/// satisfies the same `is_valid_service_name` predicate ingest enforces:
/// a value no on-disk file can carry is also a value that must never be
/// spliced into the glob list, so it falls back to the wildcard pattern.
/// A time filter scopes to the relevant date/hour directories; without
/// one, date-formatted dirs are enumerated per env.
///
/// Returns a `DuckDB` list literal like
/// `['data/prod/2026-08-02/14/*.parquet', ...]` when scoping is
/// possible. Pruning is an optimization only: the SQL WHERE clause always
/// re-filters, so a broader source is never incorrect. No exit may widen
/// to `{base}/**/*.parquet`, though, which reaches past the env dimension
/// into `scheduled/` (materialized saved-query output, whose schema is a
/// query's rather than an event's, so a union over it is a hard error
/// under ADR-0008). Every "nothing to read" exit is [`no_match_source`],
/// including unparseable DSL: the executor parses the same text with the
/// same parser before it reads, so a source built from DSL that does not
/// parse never reaches `read_parquet` at all.
pub(crate) fn compute_source(base_dir: &str, dsl: &str) -> String {
    select(base_dir, dsl).source()
}

/// The selection [`compute_source`] renders, before it is rendered: the
/// rules are [`compute_source`]'s.
pub(crate) fn select(base_dir: &str, dsl: &str) -> SourceSelection {
    let base = base_dir.trim_end_matches('/').to_owned();

    let Ok(ast) = trawl_core::parser::parse(dsl) else {
        return SourceSelection {
            base,
            file_pattern: ANY_SERVICE.to_owned(),
            elements: Vec::new(),
        };
    };

    let Narrowing { file_pattern, envs } = narrowing(&base, &ast);
    let time_filter = ast.search.time_filter.as_ref().map(|tf| tf.node.duration);

    let mut elements: Vec<Element> = Vec::new();
    for env in &envs {
        let env_base = format!("{base}/{env}");
        match time_filter {
            Some(duration) => {
                let scoped = time_scoped_dirs(&env_base, duration, &file_pattern);
                if scoped.is_empty() {
                    elements.extend(date_scoped_dirs(&env_base));
                } else {
                    elements.extend(scoped);
                }
            }
            None => elements.extend(date_scoped_dirs(&env_base)),
        }
    }

    // No elements is every env existing with no matching date directory,
    // or no env at all: the same "nothing to read" state as a cold start,
    // and the same reason not to fall back to `{base}/**/` over
    // `scheduled/`. [`SourceSelection::source`] renders it as
    // [`no_match_source`].
    SourceSelection {
        base,
        file_pattern,
        elements,
    }
}

/// The source for "no on-disk file can match": a path segment that is not a
/// legal env name, so no data directory can ever carry it. The executor
/// reads "no files" as an empty cold side and the SQL filter keeps hot rows
/// correct.
fn no_match_source(base: &str, file_pattern: &str) -> String {
    format!("{base}/.no-such-env/{file_pattern}")
}

/// What a query's equality filters pin: the file pattern and the env
/// directories to search. No envs means nothing on disk can match.
struct Narrowing {
    file_pattern: String,
    envs: Vec<String>,
}

/// Read the `service=` and `env=` pins off a parsed query (see
/// [`compute_source`] for the rules).
fn narrowing(base: &str, ast: &Query) -> Narrowing {
    // The literal is interpolated verbatim into single-quoted glob
    // entries, so anything outside the ingest-side charset (quotes,
    // separators, slashes, dots at the front) is rejected here rather
    // than allowed to close one path and open another.
    let service = extract_eq_filter(&ast.search, "service")
        .filter(|s| trawl_config::is_valid_service_name(s));
    let file_pattern = service.map_or_else(|| ANY_SERVICE.to_owned(), |s| format!("{s}.parquet"));

    let envs: Vec<String> = match extract_eq_filter(&ast.search, "env") {
        Some(env)
            if trawl_config::is_valid_env_name(env)
                && !trawl_config::RESERVED_ENV_NAMES.contains(&env) =>
        {
            vec![env.to_owned()]
        }
        // A value no on-disk env can carry (they were validated at
        // ingest) — match nothing; the executor treats "no files" as
        // an empty result and the SQL filter keeps hot rows correct.
        Some(_) => Vec::new(),
        // Cold start (no env directories yet) lists none: nothing to
        // read, and no widening to `{base}/**/`, which would reach
        // `scheduled/`.
        None => crate::env_dirs::list_env_names(Path::new(base)),
    };

    Narrowing { file_pattern, envs }
}

/// What one `read_parquet()` source reads: the partition directories and
/// the one file pattern every directory is read with.
///
/// It is structured rather than a source string because a capture needs
/// two things from it: the `read_parquet()` argument ([`Self::source`]) and
/// the bytes behind it ([`selection_bytes`]), and the second must expand
/// exactly the directories and pattern the first names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceSelection {
    /// The data root, without a trailing slash.
    base: String,
    /// `*.parquet`, or `{service}.parquet` for a pinned service.
    file_pattern: String,
    /// The directories, in the order the source lists them.
    elements: Vec<Element>,
}

/// One directory of a [`SourceSelection`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct Element {
    /// The directory's path.
    dir: String,
    /// Read through `**`: the pattern matches files at any depth under
    /// `dir`, `dir` itself included. Otherwise only the files directly in
    /// `dir`.
    recursive: bool,
}

impl Element {
    fn hour_or_day(dir: String) -> Self {
        Self {
            dir,
            recursive: false,
        }
    }

    fn date(dir: String) -> Self {
        Self {
            dir,
            recursive: true,
        }
    }

    /// The element's `read_parquet()` list entry.
    fn glob(&self, file_pattern: &str) -> String {
        if self.recursive {
            format!("'{}/**/{file_pattern}'", self.dir)
        } else {
            format!("'{}/{file_pattern}'", self.dir)
        }
    }
}

impl SourceSelection {
    /// The `read_parquet()` source argument: a `DuckDB` list literal with
    /// one entry per directory, or [`no_match_source`] when no directory
    /// was selected.
    pub(crate) fn source(&self) -> String {
        if self.elements.is_empty() {
            return no_match_source(&self.base, &self.file_pattern);
        }
        let globs: Vec<String> = self
            .elements
            .iter()
            .map(|element| element.glob(&self.file_pattern))
            .collect();
        format!("[{}]", globs.join(", "))
    }

    /// Whether a file name matches the pattern the way `DuckDB`'s `glob()`
    /// does: `*` matches any run of bytes, including an empty one and a
    /// leading dot, and the match is case-sensitive.
    fn matches(&self, name: &std::ffi::OsStr) -> bool {
        let name = name.as_encoded_bytes();
        if self.file_pattern == ANY_SERVICE {
            name.ends_with(b".parquet")
        } else {
            name == self.file_pattern.as_bytes()
        }
    }
}

/// The bytes of parquet `selection` reaches that can hold an event of the
/// window `[start, end)`: the summed size of every regular file its pattern
/// matches in its directories, less each file whose footer statistics put
/// every `_time` it holds outside the window.
///
/// Partitions are named by when compaction wrote them and ingest keeps the
/// `_time` a sender says (`ingest::envelope`), so no directory name bounds
/// the `_time` of what it holds: the selection is the whole of what
/// `/query` reads for the same text, and only the files themselves can say
/// they hold nothing in the window. The engine's `_time` bounds skip the
/// same row groups by the same statistics, so a file left out here is one
/// whose rows the read would not keep. A file counts whole when its footer
/// gives no such proof: no `_time` column, `_time` not a timestamp in every
/// row group, no min or max, or a footer the reader refuses. Ticks are read
/// as UTC whether or not the file marks the column UTC-adjusted, as the
/// engine's UTC session casts them.
///
/// The walk lists the directories on disk; it never builds a path the disk
/// does not hold. It checks `cancel` before each entry and stops with
/// [`EngineError::Cancelled`] once the work is cancelled, so an abandoned
/// capture stops walking.
///
/// This is a budget estimate, not a promise. The caller takes it under the
/// publication read guard, before the engine runs, so no canonical Parquet
/// publication in this process lands between the measurement and the read.
/// Retention takes no such guard: a directory that is gone, or a file that
/// vanishes between the listing and its footer, counts as zero. Any other
/// IO error is returned, so the caller cannot mistake an unreadable
/// directory for an empty one.
///
/// Symlinks are not followed and count as zero. `DuckDB` would read
/// through a live link to a file, but trawld never writes a symlink into
/// the data root, which is owner-only (ADR-0052), so a link there is not
/// data this budget measures. A dangling link matches nothing in `DuckDB`
/// either.
pub(crate) fn selection_bytes(
    selection: &SourceSelection,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    cancel: &CancelLatch,
) -> Result<u64, EngineError> {
    let window = Window {
        start: i128::from(start.timestamp_micros()),
        end: i128::from(end.timestamp_micros()),
    };
    let mut total: u64 = 0;
    for element in &selection.elements {
        let bytes = dir_bytes(
            selection,
            Path::new(&element.dir),
            element.recursive,
            window,
            cancel,
        )?;
        total = total.saturating_add(bytes);
    }
    Ok(total)
}

/// A half-open window `[start, end)` in microseconds since the epoch.
#[derive(Debug, Clone, Copy)]
struct Window {
    start: i128,
    end: i128,
}

/// The bytes under one directory of `selection`, for [`selection_bytes`].
fn dir_bytes(
    selection: &SourceSelection,
    dir: &Path,
    recursive: bool,
    window: Window,
    cancel: &CancelLatch,
) -> Result<u64, EngineError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(EngineError::Io(e)),
    };
    let mut total: u64 = 0;
    for entry in entries {
        if cancel.latched() {
            return Err(EngineError::Cancelled);
        }
        let entry = entry.map_err(EngineError::Io)?;
        // `DirEntry::file_type` does not traverse a symlink.
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(EngineError::Io(e)),
        };
        let bytes = if file_type.is_dir() {
            // `**` descends into every directory, a dot-named one or one
            // whose own name matches the pattern included, and matches
            // only files.
            if !recursive {
                continue;
            }
            dir_bytes(selection, &entry.path(), true, window, cancel)?
        } else if file_type.is_file() && selection.matches(&entry.file_name()) {
            file_bytes(&entry, window)?
        } else {
            continue;
        };
        total = total.saturating_add(bytes);
    }
    Ok(total)
}

/// One matching regular file's bytes, or zero when its footer proves it
/// holds no `_time` in `window` or it vanished (see [`selection_bytes`]).
fn file_bytes(entry: &std::fs::DirEntry, window: Window) -> Result<u64, EngineError> {
    let len = match entry.metadata() {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(EngineError::Io(e)),
    };
    match read_file_stats(&entry.path()) {
        Ok(stats) => {
            let outside = stats
                .columns
                .iter()
                .find(|column| column.name == trawl_core::schema::TIME)
                .is_some_and(|time| outside_window(time.samples.min(), time.samples.max(), window));
            Ok(if outside { 0 } else { len })
        }
        Err(ParquetStatsError::Open { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Ok(0)
        }
        Err(ParquetStatsError::Open { source, .. }) => Err(EngineError::Io(source)),
        // A footer the reader refuses reaches the engine whole, which
        // reads or refuses it on its own terms.
        Err(_) => Ok(len),
    }
}

/// Whether a column whose values lie in `[min, max]` holds nothing in
/// `window`: both bounds are timestamps, and the latest is before the
/// window or the earliest at or after its end. A bound read at a finer unit
/// than the microsecond is widened outward to whole microseconds, so a cast
/// that truncates or rounds it cannot move a value across the window.
fn outside_window(min: Option<&StatVal>, max: Option<&StatVal>, window: Window) -> bool {
    let (Some(min), Some(max)) = (
        min.and_then(|min| micros(min, false)),
        max.and_then(|max| micros(max, true)),
    ) else {
        return false;
    };
    max < window.start || min >= window.end
}

/// A timestamp sample in microseconds since the epoch, rounded down, or up
/// when `ceil`. `None` for any other sample.
fn micros(sample: &StatVal, ceil: bool) -> Option<i128> {
    let StatVal::Timestamp { raw, unit, .. } = sample else {
        return None;
    };
    let raw = i128::from(*raw);
    Some(match unit {
        TimestampUnit::Millis => raw * 1_000,
        TimestampUnit::Micros => raw,
        TimestampUnit::Nanos if ceil => {
            raw.div_euclid(1_000) + i128::from(raw.rem_euclid(1_000) != 0)
        }
        TimestampUnit::Nanos => raw.div_euclid(1_000),
    })
}

/// Time-scoped directories for one env root: hour-level for today,
/// day-level and/or hour-level for historical dates, existence-filtered.
/// Returns an empty vec when nothing on disk matches the window.
///
/// A historical start day is selected whole, every hour of it: the walk
/// starts at that day's midnight rather than at the padded start's hour.
fn time_scoped_dirs(
    base: &str,
    duration: trawl_core::ast::TrawlDuration,
    file_pattern: &str,
) -> Vec<Element> {
    let total_secs = duration
        .to_seconds()
        .saturating_add(TIME_FILTER_PADDING_SECS);

    let now = chrono::Utc::now();
    let start = now - chrono::Duration::seconds(i64::try_from(total_secs).unwrap_or(i64::MAX));
    let from = if start.date_naive() == now.date_naive() {
        start.naive_utc()
    } else {
        start.date_naive().and_time(chrono::NaiveTime::MIN)
    };

    // Only directories that exist on disk are kept, which avoids sending
    // DuckDB a list of entirely nonexistent paths: that would trigger a
    // "No files found" error before the executor safety net catches it.
    partition_dirs(base, from, now.naive_utc(), file_pattern)
        .into_iter()
        .map(Element::hour_or_day)
        .collect()
}

/// The existing partition directories of one env root from `from`'s hour
/// through `now`'s, in walk order: per date, the date directory when it
/// holds day-level files, then its hour directories.
///
/// Today's hours are listed up to `now`'s. A historical date is checked
/// for a consolidated day-level file and for remaining hour directories,
/// since both coexist during a partial rollup (one service consolidated,
/// another not). `from`'s own date is listed from `from`'s hour, every
/// later date from midnight.
fn partition_dirs(
    base: &str,
    from: NaiveDateTime,
    now: NaiveDateTime,
    file_pattern: &str,
) -> Vec<String> {
    let today = now.date();

    let mut dirs = Vec::new();
    let mut cursor = from
        .date()
        .and_hms_opt(from.hour(), 0, 0)
        .expect("valid hour from Timelike::hour()");

    while cursor <= now {
        let day = cursor.format("%Y-%m-%d").to_string();

        if cursor.date() == today {
            // Today may have day-level consolidated files from a
            // previous server session or an earlier daily rollup.
            // Check once, then list the hours of the time range.
            if has_day_level_files(base, &day, file_pattern) {
                dirs.push(format!("{base}/{day}"));
            }
            while cursor <= now {
                let hour = cursor.format("%H");
                dirs.push(format!("{base}/{day}/{hour}"));
                cursor += chrono::Duration::hours(1);
            }
        } else {
            let has_day = has_day_level_files(base, &day, file_pattern);
            let has_hours = has_hour_dirs(base, &day);

            if has_day {
                dirs.push(format!("{base}/{day}"));
            }
            if has_hours || !has_day {
                for h in cursor.hour()..24 {
                    dirs.push(format!("{base}/{day}/{h:02}"));
                }
            }

            // Skip to next day (advance cursor past remaining hours of this day).
            cursor = (cursor.date() + chrono::Duration::days(1))
                .and_hms_opt(0, 0, 0)
                .expect("valid midnight from date + 1 day");
        }
    }

    dirs.retain(|dir| Path::new(dir).exists());
    dirs
}

/// Date-scoped directories for one env root, for queries without a
/// `last=` filter.
///
/// Enumerates directories matching `YYYY-MM-DD` in the env base, each read
/// through `**`. Returns an empty vec when the env base holds no date dirs.
fn date_scoped_dirs(base: &str) -> Vec<Element> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };

    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if is_date_dir_name(name_str) && entry.path().is_dir() {
            dirs.push(format!("{base}/{name_str}"));
        }
    }

    // `read_dir` yields entries in filesystem order; sort so the emitted
    // source list is stable across calls.
    dirs.sort();
    dirs.into_iter().map(Element::date).collect()
}

/// Check if a directory name looks like `YYYY-MM-DD`.
fn is_date_dir_name(name: &str) -> bool {
    if name.len() != 10 {
        return false;
    }
    let bytes = name.as_bytes();
    bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
}

/// Check if a date directory has day-level parquet files (consolidated).
///
/// For a known service, does a single `stat()`. For wildcards, checks
/// if the date directory contains any direct `.parquet` files.
fn has_day_level_files(base: &str, day: &str, file_pattern: &str) -> bool {
    let day_dir = std::path::Path::new(base).join(day);

    if file_pattern == "*.parquet" {
        // Wildcard: check if any .parquet files exist directly in date dir.
        let Ok(entries) = std::fs::read_dir(&day_dir) else {
            return false;
        };
        entries.flatten().any(|e| {
            let p = e.path();
            !p.is_dir() && p.extension().is_some_and(|ext| ext == "parquet")
        })
    } else {
        // Known service: single stat() call.
        day_dir.join(file_pattern).exists()
    }
}

/// Check if a date directory still has hourly subdirectories (00-23).
///
/// Used to detect mixed-state dirs where some services are consolidated
/// at day level while others remain in hourly dirs.
fn has_hour_dirs(base: &str, day: &str) -> bool {
    let day_dir = std::path::Path::new(base).join(day);
    let Ok(entries) = std::fs::read_dir(&day_dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        let p = e.path();
        if !p.is_dir() {
            return false;
        }
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.len() == 2 && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolve a bare (non-list) source pattern with the same matcher
    /// `read_parquet` uses, so a test can assert what a source really
    /// reaches on disk rather than what its text looks like.
    fn glob_matches(pattern: &str) -> Vec<String> {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let mut stmt = conn
            .prepare(&format!("SELECT file FROM glob('{pattern}')"))
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn no_time_filter_over_empty_root_matches_nothing() {
        // No env directory on disk → nothing to read.
        let source = compute_source("/data", "_severity=error");
        assert_eq!(source, "/data/.no-such-env/*.parquet");
    }

    #[test]
    fn service_filter_narrows_glob() {
        // Exact service filter → narrow to service-specific file.
        let source = compute_source("/data", "service=nginx");
        assert_eq!(source, "/data/.no-such-env/nginx.parquet");
    }

    #[test]
    fn service_glob_keeps_wildcard() {
        // Glob operator on service → can't narrow, keep *.parquet.
        let source = compute_source("/data", "service=ng*");
        assert_eq!(source, "/data/.no-such-env/*.parquet");
    }

    #[test]
    fn bad_dsl_matches_nothing() {
        // Unparseable DSL takes the no-match shape like every other
        // "nothing to read" exit, never the `**` glob that reaches
        // `scheduled/`. The executor rejects the same text before it reads.
        let source = compute_source("/data", "| | invalid");
        assert_eq!(source, "/data/.no-such-env/*.parquet");
        assert!(!source.contains("**"));
    }

    #[test]
    fn with_time_filter_returns_list() {
        // Create temp dir with today's hour directories so the filter doesn't prune them.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let now = chrono::Utc::now();
        // Create hour dirs for current and recent hours, deriving both date
        // and hour from the offset time (handles UTC midnight correctly).
        for h_offset in 0..=3 {
            let dt = now - chrono::Duration::hours(h_offset);
            let date = dt.format("%Y-%m-%d").to_string();
            let hour = dt.format("%H").to_string();
            std::fs::create_dir_all(tmp.path().join("prod").join(&date).join(&hour)).unwrap();
        }
        let source = compute_source(base, "last=1h");
        // Should be a list of hour-directory globs, not the fallback.
        assert!(
            source.starts_with('['),
            "expected list format, got: {source}"
        );
        assert!(source.ends_with(']'), "expected list format, got: {source}");
        assert!(
            source.contains("*.parquet"),
            "expected parquet globs, got: {source}"
        );
        // With 1h + 1h padding, should have at least 2 hour entries.
        let count = source.matches("*.parquet").count();
        assert!(
            count >= 2,
            "expected >=2 hour globs for last=1h, got {count}: {source}"
        );
    }

    #[test]
    fn service_and_time_filter_compose() {
        // Create hour dirs, deriving both date and hour from the offset
        // time (handles UTC midnight correctly).
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let now = chrono::Utc::now();
        for h_offset in 0..=3 {
            let dt = now - chrono::Duration::hours(h_offset);
            let date = dt.format("%Y-%m-%d").to_string();
            let hour = dt.format("%H").to_string();
            std::fs::create_dir_all(tmp.path().join("prod").join(&date).join(&hour)).unwrap();
        }
        let source = compute_source(base, "service=nginx last=1h");
        assert!(
            source.starts_with('['),
            "expected list format, got: {source}"
        );
        // Should narrow to nginx.parquet, not *.parquet.
        assert!(
            source.contains("nginx.parquet"),
            "expected service-scoped globs, got: {source}"
        );
        assert!(
            !source.contains("*.parquet"),
            "should not contain wildcard when service is known, got: {source}"
        );
    }

    #[test]
    fn strips_trailing_slash() {
        let source = compute_source("/data/", "last=1h");
        assert!(!source.contains("//"), "double slashes in source: {source}");
    }

    #[test]
    fn prefers_day_level_for_historical_service() {
        // Create a temp data dir with a consolidated day-level file.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let day_dir = tmp.path().join("prod").join(&yesterday);
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("nginx.parquet"), b"data").unwrap();

        let source = compute_source(base, "service=nginx last=48h");

        // Should include day-level path for yesterday (no /HH/ component).
        let expected_day_glob = format!("'{base}/prod/{yesterday}/nginx.parquet'");
        assert!(
            source.contains(&expected_day_glob),
            "expected day-level glob for {yesterday}, got: {source}"
        );
    }

    #[test]
    fn falls_back_to_hourly_when_no_day_file() {
        // Create a temp data dir with only hourly files (no day-level).
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        // Create hour dirs but no day-level file.
        let hour_dir = tmp.path().join("prod").join(&yesterday).join("14");
        std::fs::create_dir_all(&hour_dir).unwrap();
        std::fs::write(hour_dir.join("nginx.parquet"), b"data").unwrap();

        let source = compute_source(base, "service=nginx last=48h");

        // Only hour 14 exists on disk, so only that glob survives filtering.
        let hourly_pattern = format!("{base}/prod/{yesterday}/14/nginx.parquet");
        assert!(
            source.contains(&hourly_pattern),
            "expected hourly glob for existing dir, got: {source}"
        );
    }

    #[test]
    fn wildcard_detects_day_level() {
        // Wildcard service with consolidated day-level files.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let day_dir = tmp.path().join("prod").join(&yesterday);
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("nginx.parquet"), b"data").unwrap();
        std::fs::write(day_dir.join("postgres.parquet"), b"data").unwrap();

        let source = compute_source(base, "last=48h");

        // Should use day-level glob (*.parquet at date level).
        let expected_day_glob = format!("'{base}/prod/{yesterday}/*.parquet'");
        assert!(
            source.contains(&expected_day_glob),
            "expected day-level wildcard glob for {yesterday}, got: {source}"
        );
    }

    #[test]
    fn day_level_files_service_specific() {
        let tmp = tempfile::tempdir().unwrap();
        let day_dir = tmp.path().join("2026-01-15");
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("nginx.parquet"), b"data").unwrap();

        assert!(has_day_level_files(
            tmp.path().to_str().unwrap(),
            "2026-01-15",
            "nginx.parquet"
        ));
        assert!(!has_day_level_files(
            tmp.path().to_str().unwrap(),
            "2026-01-15",
            "postgres.parquet"
        ));
    }

    #[test]
    fn day_level_files_wildcard() {
        let tmp = tempfile::tempdir().unwrap();
        let day_dir = tmp.path().join("2026-01-15");
        std::fs::create_dir_all(&day_dir).unwrap();

        // No files yet.
        assert!(!has_day_level_files(
            tmp.path().to_str().unwrap(),
            "2026-01-15",
            "*.parquet"
        ));

        // Add a parquet file.
        std::fs::write(day_dir.join("nginx.parquet"), b"data").unwrap();
        assert!(has_day_level_files(
            tmp.path().to_str().unwrap(),
            "2026-01-15",
            "*.parquet"
        ));
    }

    #[test]
    fn day_level_files_ignores_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let day_dir = tmp.path().join("2026-01-15");
        let hour_dir = day_dir.join("00");
        std::fs::create_dir_all(&hour_dir).unwrap();
        std::fs::write(hour_dir.join("nginx.parquet"), b"data").unwrap();

        // Hour subdir files should not count as day-level.
        assert!(!has_day_level_files(
            tmp.path().to_str().unwrap(),
            "2026-01-15",
            "*.parquet"
        ));
    }

    #[test]
    fn hour_dirs_detects_hour_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let day_dir = tmp.path().join("2026-01-15");

        // No dir at all.
        assert!(!has_hour_dirs(tmp.path().to_str().unwrap(), "2026-01-15"));

        // Dir exists but empty.
        std::fs::create_dir_all(&day_dir).unwrap();
        assert!(!has_hour_dirs(tmp.path().to_str().unwrap(), "2026-01-15"));

        // Hour subdir present.
        std::fs::create_dir_all(day_dir.join("01")).unwrap();
        assert!(has_hour_dirs(tmp.path().to_str().unwrap(), "2026-01-15"));
    }

    #[test]
    fn wildcard_mixed_state() {
        // Mixed state: day-level file for one service + hourly dir for another.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let day_dir = tmp.path().join("prod").join(&yesterday);
        std::fs::create_dir_all(&day_dir).unwrap();

        // nginx consolidated at day level.
        std::fs::write(day_dir.join("nginx.parquet"), b"data").unwrap();

        // postgres still in hourly dirs.
        let hour_dir = day_dir.join("14");
        std::fs::create_dir_all(&hour_dir).unwrap();
        std::fs::write(hour_dir.join("postgres.parquet"), b"data").unwrap();

        let source = compute_source(base, "last=48h");

        // Should include both the day-level glob and the existing hourly dir.
        let day_glob = format!("'{base}/prod/{yesterday}/*.parquet'");
        let hourly_glob = format!("'{base}/prod/{yesterday}/14/*.parquet'");
        assert!(
            source.contains(&day_glob),
            "expected day-level glob in mixed state, got: {source}"
        );
        assert!(
            source.contains(&hourly_glob),
            "expected hourly glob for existing dir in mixed state, got: {source}"
        );
    }

    #[test]
    fn sanitizes_dotted_service() {
        // Filenames carry the service verbatim (ADR-0009), so a dotted
        // service prunes to its own file: `api.v2` and `api_v2` are
        // distinct.
        let source = compute_source("/data", "service=api.v2");
        assert_eq!(
            source, "/data/.no-such-env/api.v2.parquet",
            "the file pattern carries the service name verbatim"
        );
    }

    #[test]
    fn env_and_service_pruning_matrix() {
        // Two envs on disk, one date dir each. env=X pins the env
        // directory; service=Y pins the file pattern; both compose;
        // neither searches every env.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        for env in ["prod", "lab"] {
            std::fs::create_dir_all(tmp.path().join(env).join("2026-01-15")).unwrap();
        }

        // env only.
        let source = compute_source(base, "env=prod");
        assert!(source.contains("/prod/"), "got: {source}");
        assert!(
            !source.contains("/lab/"),
            "env=prod must prune lab: {source}"
        );

        // service only — both envs searched, file pattern pinned.
        let source = compute_source(base, "service=nginx");
        assert!(source.contains("/prod/"), "got: {source}");
        assert!(source.contains("/lab/"), "got: {source}");
        assert!(source.contains("nginx.parquet"), "got: {source}");
        assert!(!source.contains("*.parquet"), "got: {source}");

        // both.
        let source = compute_source(base, "env=lab service=nginx");
        assert!(source.contains("/lab/"), "got: {source}");
        assert!(!source.contains("/prod/"), "got: {source}");
        assert!(source.contains("nginx.parquet"), "got: {source}");

        // neither — every env, wildcard pattern.
        let source = compute_source(base, "_severity=error");
        assert!(source.contains("/prod/"), "got: {source}");
        assert!(source.contains("/lab/"), "got: {source}");
    }

    #[test]
    fn dotted_and_underscored_services_prune_distinctly() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let date_dir = tmp.path().join("prod").join("2026-01-15");
        std::fs::create_dir_all(&date_dir).unwrap();
        std::fs::write(date_dir.join("api.v2.parquet"), b"a").unwrap();
        std::fs::write(date_dir.join("api_v2.parquet"), b"b").unwrap();

        let dotted = compute_source(base, "service=api.v2");
        let underscored = compute_source(base, "service=api_v2");
        assert!(dotted.contains("api.v2.parquet"), "got: {dotted}");
        assert!(underscored.contains("api_v2.parquet"), "got: {underscored}");
        assert_ne!(dotted, underscored, "the two services must prune apart");
    }

    #[test]
    fn invalid_env_value_matches_nothing() {
        // `env=../evil` can never match on-disk data (env names were
        // validated at ingest) — the source must not glob anything real.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(tmp.path().join("prod").join("2026-01-15")).unwrap();

        let source = compute_source(base, "env=../evil");
        assert!(
            source.contains(".no-such-env"),
            "invalid env must yield a no-match source, got: {source}"
        );
        assert!(!source.contains("/prod/"), "got: {source}");
    }

    /// A post-cutover install before its first compaction: `data/` holds the
    /// EPOCH marker and `scheduled/` report runs, and no env directory yet.
    /// The source must not reach into `scheduled/` — those parquet files are
    /// materialized saved-query output, not events, and unioning them into
    /// the log side turns into a hard query error under ADR-0008.
    #[test]
    fn cold_start_source_never_reaches_scheduled_report_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        std::fs::write(tmp.path().join("EPOCH"), b"2").unwrap();
        let runs = tmp.path().join("scheduled").join("errors-by-host");
        std::fs::create_dir_all(&runs).unwrap();
        let run = runs.join("run_1.parquet");
        std::fs::write(&run, b"report run").unwrap();

        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "_severity=error");

        assert_eq!(source, format!("{base}/.no-such-env/*.parquet"));
        assert!(
            glob_matches(&source).is_empty(),
            "cold-start source must match nothing, got: {source}"
        );
        // Sanity check: a `**` glob over the data root really does sweep
        // the report run in, which is why no exit may emit one.
        assert!(
            glob_matches(&fallback).contains(&run.to_string_lossy().into_owned()),
            "sanity: the `**` fallback is what reached the scheduled run"
        );
    }

    /// Same, one dimension in: env directories exist, but none holds a date
    /// directory the query could read. Still nothing to read, still no
    /// excuse to glob the whole data root.
    #[test]
    fn no_matching_date_dir_never_reaches_scheduled_report_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        std::fs::write(tmp.path().join("EPOCH"), b"2").unwrap();
        let runs = tmp.path().join("scheduled").join("errors-by-host");
        std::fs::create_dir_all(&runs).unwrap();
        std::fs::write(runs.join("run_1.parquet"), b"report run").unwrap();
        // Env dirs exist (so the cold-start branch is not the one under
        // test) but neither carries a `YYYY-MM-DD` directory.
        for env in ["prod", "lab"] {
            std::fs::create_dir_all(tmp.path().join(env)).unwrap();
        }

        let source = compute_source(base, "_severity=error");

        assert_eq!(source, format!("{base}/.no-such-env/*.parquet"));
        assert!(
            glob_matches(&source).is_empty(),
            "date-less source must match nothing, got: {source}"
        );
    }

    #[test]
    fn today_day_level_files_included() {
        // When today's data is consolidated at day level (no hourly dirs),
        // the time-scoped path should include the day-level glob.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let now = chrono::Utc::now();
        let today = now.format("%Y-%m-%d").to_string();

        // Create day-level file only (no hourly subdirs).
        let day_dir = tmp.path().join("prod").join(&today);
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("trawld.parquet"), b"data").unwrap();

        let source = compute_source(base, "service=trawld last=1h");

        // Day-level glob must be present so the consolidated file is found.
        let day_glob = format!("'{base}/prod/{today}/trawld.parquet'");
        assert!(
            source.contains(&day_glob),
            "expected day-level glob for today, got: {source}"
        );
    }

    #[test]
    fn today_mixed_day_and_hourly() {
        // Today has both day-level files and hourly dirs — both should appear.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let now = chrono::Utc::now();
        let today = now.format("%Y-%m-%d").to_string();
        let hour = now.format("%H").to_string();

        let day_dir = tmp.path().join("prod").join(&today);
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("nginx.parquet"), b"data").unwrap();
        std::fs::create_dir_all(day_dir.join(&hour)).unwrap();

        let source = compute_source(base, "last=1h");

        let day_glob = format!("'{base}/prod/{today}/*.parquet'");
        let hourly_glob = format!("{base}/prod/{today}/{hour}/");
        assert!(
            source.contains(&day_glob),
            "expected day-level glob, got: {source}"
        );
        assert!(
            source.contains(&hourly_glob),
            "expected hourly glob, got: {source}"
        );
    }

    #[test]
    fn invalid_service_value_cannot_inject_paths() {
        // A quoted DSL literal can carry `', '` — the list-item separator.
        // It must never reach the glob list: the pattern falls back to the
        // wildcard, so no attacker-chosen path is ever emitted.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(tmp.path().join("prod").join("2026-01-15")).unwrap();

        for dsl in [
            r#"service="nginx', '/**/*""#,
            r#"service="nginx', '/etc/shadow""#,
            "service=../../escaped",
            "service=.hidden",
        ] {
            let source = compute_source(base, dsl);
            assert!(
                source.contains("*.parquet"),
                "invalid service must fall back to the wildcard, got: {source}"
            );
            assert!(
                !source.contains("nginx") && !source.contains("hidden"),
                "invalid service must not reach the glob list, got: {source}"
            );
            assert!(
                !source.contains("/etc/") && !source.contains(".."),
                "invalid service must not escape the data root, got: {source}"
            );
        }
    }

    #[test]
    fn verbatim_service_with_time_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let source = compute_source(base, "service=host.name last=1h");

        // Filenames carry the service verbatim (ADR-0009).
        assert!(
            source.contains("host.name.parquet"),
            "expected the verbatim service in the glob, got: {source}"
        );
        assert!(
            !source.contains("host_name.parquet"),
            "must not sanitize the service name, got: {source}"
        );
    }

    // -----------------------------------------------------------------
    // Capture windows (ADR-0057): `select`, `selection_bytes`
    // -----------------------------------------------------------------

    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("test instant")
            .with_timezone(&Utc)
    }

    /// Create `{root}/{rel}` as a directory.
    fn mkdir(root: &Path, rel: &str) {
        std::fs::create_dir_all(root.join(rel)).unwrap();
    }

    /// Write `len` bytes to `{root}/{rel}`, creating its directory: a file
    /// whose footer no reader accepts.
    fn write_len(root: &Path, rel: &str, len: usize) -> u64 {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; len]).unwrap();
        u64::try_from(len).unwrap()
    }

    /// Write `{root}/{rel}` as parquet with one row per `_time`, each a
    /// `DuckDB` timestamp of `ty`, and return its size.
    fn write_times(root: &Path, rel: &str, ty: &str, times: &[&str]) -> u64 {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let rows: Vec<String> = times
            .iter()
            .map(|t| format!("(CAST('{t}' AS {ty}), 'm')"))
            .collect();
        duckdb::Connection::open_in_memory()
            .unwrap()
            .execute_batch(&format!(
                "COPY (SELECT * FROM (VALUES {}) AS t(_time, message)) \
                 TO '{}' (FORMAT PARQUET)",
                rows.join(", "),
                path.display()
            ))
            .unwrap();
        std::fs::metadata(path).unwrap().len()
    }

    /// The selected directories relative to `root`, for readable asserts.
    fn rel_dirs(selection: &SourceSelection, root: &Path) -> Vec<String> {
        let prefix = format!("{}/", root.to_str().unwrap());
        selection
            .elements
            .iter()
            .map(|e| e.dir.strip_prefix(&prefix).unwrap_or(&e.dir).to_owned())
            .collect()
    }

    /// A latch nothing sets.
    fn running() -> CancelLatch {
        CancelLatch::never()
    }

    /// A capture's executed text for `[earliest, latest)` over `head`.
    fn executed(head: &str, earliest: &str, latest: &str) -> String {
        let text = format!("{head} earliest={earliest} latest={latest}");
        trawl_core::parser::parse(&text).expect("the executed text parses");
        text
    }

    #[test]
    fn a_window_selects_every_date_directory_as_query_does() {
        // The window's clauses never prune by directory name: a partition
        // is named by when compaction wrote it, not by the `_time` it holds.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        mkdir(tmp.path(), "prod/2026-10-21/13");
        mkdir(tmp.path(), "prod/2026-10-23/09");
        mkdir(tmp.path(), "prod/notes");
        mkdir(tmp.path(), "lab/2026-10-22");
        let text = executed(
            "env=prod service=nginx",
            "2026-10-23T09:00:00Z",
            "2026-10-23T09:30:00Z",
        );

        let selection = select(base, &text);

        assert_eq!(
            rel_dirs(&selection, tmp.path()),
            ["prod/2026-10-21", "prod/2026-10-23"]
        );
        assert!(selection.elements.iter().all(|e| e.recursive));
        assert_eq!(selection.source(), compute_source(base, &text));
        assert_eq!(
            selection.source(),
            format!(
                "['{base}/prod/2026-10-21/**/nginx.parquet', \
                 '{base}/prod/2026-10-23/**/nginx.parquet']"
            )
        );
    }

    #[test]
    fn an_ancient_window_selects_only_the_directories_on_disk() {
        // Year 1 to now is some 17 million hours; none is built.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let held = write_times(
            tmp.path(),
            "prod/2026-10-23/09/nginx.parquet",
            "TIMESTAMP",
            &["2026-10-23 09:10:00"],
        );
        write_times(
            tmp.path(),
            "prod/2026-10-22/nginx.parquet",
            "TIMESTAMP",
            &["2026-10-22 12:00:00"],
        );
        let text = executed(
            "service=nginx",
            "0001-01-01T00:00:00Z",
            "0001-01-01T01:00:00Z",
        );

        let begun = std::time::Instant::now();
        let selection = select(base, &text);
        let ancient = selection_bytes(
            &selection,
            at("0001-01-01T00:00:00Z"),
            at("0001-01-01T01:00:00Z"),
            &running(),
        )
        .unwrap();
        let elapsed = begun.elapsed();

        assert_eq!(
            rel_dirs(&selection, tmp.path()),
            ["prod/2026-10-22", "prod/2026-10-23"]
        );
        assert_eq!(ancient, 0, "no stored event is that old");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "took {elapsed:?}"
        );
        let today = selection_bytes(
            &selection,
            at("2026-10-23T09:00:00Z"),
            at("2026-10-23T10:00:00Z"),
            &running(),
        )
        .unwrap();
        assert_eq!(today, held);
    }

    #[test]
    fn selection_bytes_counts_the_files_that_can_hold_the_window() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        // A day-level file two days old whose sender ran ahead: its
        // `_time` is inside the window.
        let skewed = write_times(
            tmp.path(),
            "prod/2026-10-21/nginx.parquet",
            "TIMESTAMP",
            &["2026-10-21 13:00:00", "2026-10-23 09:10:00"],
        );
        // This hour, every `_time` before the window.
        write_times(
            tmp.path(),
            "prod/2026-10-23/09/postgres.parquet",
            "TIMESTAMP",
            &["2026-10-23 08:00:00", "2026-10-23 08:59:59.999999"],
        );
        // The last microsecond inside the window.
        let last = write_times(
            tmp.path(),
            "prod/2026-10-23/09/redis.parquet",
            "TIMESTAMP",
            &["2026-10-23 09:29:59.999999"],
        );
        // The window's end, which it excludes.
        write_times(
            tmp.path(),
            "prod/2026-10-23/10/api.parquet",
            "TIMESTAMP",
            &["2026-10-23 09:30:00"],
        );
        // UTC-adjusted ticks are read as instants too.
        let zoned = write_times(
            tmp.path(),
            "prod/2026-10-23/10/edge.parquet",
            "TIMESTAMPTZ",
            &["2026-10-23 09:00:00+00"],
        );
        // No footer to prove anything: counted whole.
        let unreadable = write_len(tmp.path(), "prod/2026-10-23/10/torn.parquet", 13);
        let (start, end) = (at("2026-10-23T09:00:00Z"), at("2026-10-23T09:30:00Z"));

        let every = select(
            base,
            &executed("*", "2026-10-23T09:00:00Z", "2026-10-23T09:30:00Z"),
        );
        assert_eq!(
            selection_bytes(&every, start, end, &running()).unwrap(),
            skewed + last + zoned + unreadable
        );
        let nginx = select(
            base,
            &executed(
                "service=nginx",
                "2026-10-23T09:00:00Z",
                "2026-10-23T09:30:00Z",
            ),
        );
        assert_eq!(
            selection_bytes(&nginx, start, end, &running()).unwrap(),
            skewed
        );
    }

    #[test]
    fn a_footer_without_timestamp_bounds_counts_whole() {
        let window = Window {
            start: i128::from(at("2026-10-23T09:00:00Z").timestamp_micros()),
            end: i128::from(at("2026-10-23T09:30:00Z").timestamp_micros()),
        };
        let early = StatVal::Timestamp {
            raw: at("2026-10-23T08:00:00Z").timestamp_micros(),
            unit: TimestampUnit::Micros,
            utc: false,
        };
        assert!(outside_window(Some(&early), Some(&early), window));
        assert!(!outside_window(None, Some(&early), window), "no min");
        assert!(!outside_window(Some(&early), None, window), "no max");
        let text = StatVal::Bytes(b"2026-10-23 08:00:00".to_vec());
        assert!(
            !outside_window(Some(&text), Some(&text), window),
            "a text `_time` orders as text, not as instants"
        );
        assert!(!outside_window(
            Some(&StatVal::Int(0)),
            Some(&StatVal::Int(1)),
            window
        ));
    }

    #[test]
    fn finer_timestamps_widen_outward_to_whole_microseconds() {
        let start = at("2026-10-23T09:00:00Z");
        let window = Window {
            start: i128::from(start.timestamp_micros()),
            end: i128::from(at("2026-10-23T09:30:00Z").timestamp_micros()),
        };
        let nanos = |offset: i64| StatVal::Timestamp {
            raw: start.timestamp_nanos_opt().unwrap() + offset,
            unit: TimestampUnit::Nanos,
            utc: true,
        };
        // 500 ns before the start could round onto it: not provably out.
        let close = nanos(-500);
        assert!(!outside_window(Some(&close), Some(&close), window));
        // A whole microsecond before it is out.
        let before = nanos(-1_000);
        assert!(outside_window(Some(&before), Some(&before), window));
        let millis = StatVal::Timestamp {
            raw: start.timestamp_millis() - 1,
            unit: TimestampUnit::Millis,
            utc: false,
        };
        assert!(outside_window(Some(&millis), Some(&millis), window));
        assert_eq!(micros(&nanos(-1), false), Some(window.start - 1));
        assert_eq!(micros(&nanos(-1), true), Some(window.start));
        assert_eq!(micros(&nanos(1), false), Some(window.start));
        assert_eq!(micros(&nanos(1), true), Some(window.start + 1));
    }

    #[test]
    fn an_empty_selection_reads_nothing_and_measures_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let (start, end) = (at("2026-10-23T09:00:00Z"), at("2026-10-23T09:15:00Z"));

        let cold_start = select(
            base,
            &executed("*", "2026-10-23T09:00:00Z", "2026-10-23T09:15:00Z"),
        );
        assert!(cold_start.elements.is_empty(), "{cold_start:?}");
        assert_eq!(
            cold_start.source(),
            format!("{base}/.no-such-env/*.parquet")
        );
        assert!(glob_matches(&cold_start.source()).is_empty());
        assert_eq!(
            selection_bytes(&cold_start, start, end, &running()).unwrap(),
            0
        );

        mkdir(tmp.path(), "prod/2026-10-23/09");
        let bad_env = select(base, "env=../evil");
        assert!(bad_env.elements.is_empty());
        let bad_service = select(base, r#"service="x', '/etc""#);
        assert_eq!(bad_service.file_pattern, "*.parquet");
        assert!(!bad_service.source().contains("/etc"));
        let unparseable = select(base, "| | broken");
        assert_eq!(
            unparseable.source(),
            format!("{base}/.no-such-env/*.parquet")
        );
    }

    #[test]
    #[cfg(unix)]
    fn selection_bytes_follows_no_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        write_len(tmp.path(), "prod/2026-10-23/09/nginx.parquet", 5);
        let hour = tmp.path().join("prod/2026-10-23/09");
        std::os::unix::fs::symlink(
            tmp.path().join("nowhere.parquet"),
            hour.join("dangling.parquet"),
        )
        .unwrap();
        // A live link to a large file, and one to a directory of them,
        // outside the data layout.
        write_len(tmp.path(), "elsewhere/big.parquet", 4096);
        std::os::unix::fs::symlink(
            tmp.path().join("elsewhere/big.parquet"),
            hour.join("linked.parquet"),
        )
        .unwrap();
        std::os::unix::fs::symlink(tmp.path().join("elsewhere"), hour.join("linkdir")).unwrap();

        let selection = select(base, "*");

        assert_eq!(rel_dirs(&selection, tmp.path()), ["prod/2026-10-23"]);
        assert_eq!(
            selection_bytes(
                &selection,
                at("2026-10-23T09:00:00Z"),
                at("2026-10-23T09:10:00Z"),
                &running()
            )
            .unwrap(),
            5,
            "only the regular file counts; no link is followed"
        );
    }

    #[test]
    fn selection_bytes_counts_a_vanished_directory_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        write_len(tmp.path(), "prod/2026-10-22/23/nginx.parquet", 9);
        write_len(tmp.path(), "prod/2026-10-23/09/nginx.parquet", 2);
        let selection = select(base, "*");
        let (start, end) = (at("2026-10-23T00:10:00Z"), at("2026-10-23T09:10:00Z"));
        assert_eq!(
            selection_bytes(&selection, start, end, &running()).unwrap(),
            11
        );

        // Retention removed the oldest day after the walk listed it.
        std::fs::remove_dir_all(tmp.path().join("prod/2026-10-22")).unwrap();
        assert_eq!(
            selection_bytes(&selection, start, end, &running()).unwrap(),
            2
        );
    }

    #[test]
    fn a_cancelled_walk_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        write_len(tmp.path(), "prod/2026-10-23/09/nginx.parquet", 2);
        let selection = select(base, "*");
        let cancelled = CancelLatch::new(Arc::new(AtomicBool::new(true)));

        let walked = selection_bytes(
            &selection,
            at("2026-10-23T09:00:00Z"),
            at("2026-10-23T09:10:00Z"),
            &cancelled,
        );

        assert!(matches!(walked, Err(EngineError::Cancelled)), "{walked:?}");
    }

    /// Drift guard: the files `selection_bytes` sizes must be the files
    /// `read_parquet` reaches through the same elements, on every
    /// non-symlink shape a date directory can hold.
    #[test]
    fn selection_bytes_matches_what_duckdb_glob_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let day = "prod/2026-10-23";
        for name in [
            "nginx.parquet",
            "09/nginx.parquet",
            "09/redis.parquet",
            "09/.parquet",
            "09/.hidden.parquet",
            "09/nginx.parquet.tmp",
            "09/upper.PARQUET",
            "09/nginx.parquetx",
            "09/deeper/nginx.parquet",
            ".stage/nginx.parquet",
            "dir.parquet/nginx.parquet",
            "notes.txt",
        ] {
            write_len(tmp.path(), &format!("{day}/{name}"), 1);
        }

        for dsl in ["*", "service=nginx", "service=absent"] {
            let selection = select(base, dsl);
            assert_eq!(rel_dirs(&selection, tmp.path()), [day]);
            let duckdb: usize = selection
                .elements
                .iter()
                .map(|e| glob_matches(&e.glob(&selection.file_pattern).replace('\'', "")).len())
                .sum();
            assert_eq!(
                selection_bytes(
                    &selection,
                    at("2026-10-23T09:00:00Z"),
                    at("2026-10-23T09:10:00Z"),
                    &running()
                )
                .unwrap(),
                u64::try_from(duckdb).unwrap(),
                "one unreadable byte per file, so the counts must agree for {dsl:?}"
            );
        }
    }
}
