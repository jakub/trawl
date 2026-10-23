// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Source glob computation for parquet file selection.
//!
//! Narrows the `read_parquet()` source argument based on time filters
//! and service names extracted from the DSL query, avoiding full-directory
//! scans when possible.
//!
//! Two entry points share the narrowing. [`compute_source`] serves `/query`
//! and reads the window from the DSL's own `last=`. [`compute_window_source`]
//! serves a capture (ADR-0057), whose window the server resolved before
//! execution, and [`selection_bytes`] measures what that selection reaches.

use std::io;
use std::path::Path;

use chrono::{DateTime, NaiveDateTime, Timelike as _, Utc};
use trawl_core::ast::{FieldFilter, FilterOp, FilterValue, Query, SearchToken};

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
    let base = base_dir.trim_end_matches('/');

    let Ok(ast) = trawl_core::parser::parse(dsl) else {
        return no_match_source(base, ANY_SERVICE);
    };

    let Narrowing { file_pattern, envs } = narrowing(base, &ast);
    if envs.is_empty() {
        return no_match_source(base, &file_pattern);
    }

    let time_filter = ast.search.time_filter.as_ref().map(|tf| tf.node.duration);

    let mut globs: Vec<String> = Vec::new();
    for env in &envs {
        let env_base = format!("{base}/{env}");
        match time_filter {
            Some(duration) => {
                let scoped = time_scoped_globs(&env_base, duration, &file_pattern);
                if scoped.is_empty() {
                    globs.extend(date_scoped_globs(&env_base, &file_pattern));
                } else {
                    globs.extend(scoped);
                }
            }
            None => globs.extend(date_scoped_globs(&env_base, &file_pattern)),
        }
    }

    if globs.is_empty() {
        // Every env exists but none holds a matching date directory — the
        // same "nothing to read" state as a cold start, and the same reason
        // not to fall back to `{base}/**/` over `scheduled/`.
        return no_match_source(base, &file_pattern);
    }
    format!("[{}]", globs.join(", "))
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

/// The cold-side selection of a capture window: the partition directories
/// to read and the one file pattern every directory is read with.
///
/// It is structured rather than a source string because the capture needs
/// two things from it: the `read_parquet()` argument ([`Self::source`]) and
/// the bytes behind it ([`selection_bytes`]), and the second must expand
/// exactly the directories and pattern the first names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceSelection {
    /// The data root, without a trailing slash.
    base: String,
    /// `*.parquet`, or `{service}.parquet` for a pinned service.
    file_pattern: String,
    /// Existing partition directories, day- or hour-level, in walk order.
    dirs: Vec<String>,
}

impl SourceSelection {
    /// The `read_parquet()` source argument: a `DuckDB` list literal with
    /// one `'{dir}/{file_pattern}'` element per directory, or
    /// [`no_match_source`] when no directory was selected.
    pub(crate) fn source(&self) -> String {
        if self.dirs.is_empty() {
            return no_match_source(&self.base, &self.file_pattern);
        }
        let globs: Vec<String> = self
            .dirs
            .iter()
            .map(|dir| format!("'{dir}/{}'", self.file_pattern))
            .collect();
        format!("[{}]", globs.join(", "))
    }

    /// Whether a file name in a selected directory matches the pattern the
    /// way `DuckDB`'s `glob()` does: `*` matches any run of bytes,
    /// including an empty one and a leading dot, and the match is
    /// case-sensitive.
    fn matches(&self, name: &std::ffi::OsStr) -> bool {
        let name = name.as_encoded_bytes();
        if self.file_pattern == ANY_SERVICE {
            name.ends_with(b".parquet")
        } else {
            name == self.file_pattern.as_bytes()
        }
    }
}

/// The source for a capture whose window starts at `start` (ADR-0057).
///
/// `env=` and `service=` narrow exactly as in [`compute_source`]. The DSL's
/// own time clauses are not read: the caller resolved the window and
/// spliced it into `dsl` already, and `start` is that window's start.
///
/// The selection runs from `start` minus [`TIME_FILTER_PADDING_SECS`] (the
/// pad the `last=` path widens by) through NOW, never stopping at the
/// window's end. Partitions are keyed by compaction wall time
/// (`ingest::compaction`), so an event whose `_time` is inside the window
/// can sit in any later partition, today's included. The read's SQL bounds
/// do the exact cut.
///
/// The padded start's own date is listed from its hour on, where
/// `compute_source` takes a historical start day whole. Unlike
/// `compute_source`, an env with nothing in range contributes
/// nothing instead of falling back to every date directory it holds. The
/// fallback would hand an empty window the whole env, and the capture's
/// source budget would then refuse a quiet window over an old corpus.
pub(crate) fn compute_window_source(
    base_dir: &str,
    dsl: &str,
    start: DateTime<Utc>,
) -> SourceSelection {
    window_source_at(base_dir, dsl, start, Utc::now())
}

/// [`compute_window_source`] with the clock reading passed in.
fn window_source_at(
    base_dir: &str,
    dsl: &str,
    start: DateTime<Utc>,
    now: DateTime<Utc>,
) -> SourceSelection {
    let base = base_dir.trim_end_matches('/').to_owned();

    // Unparseable text never reaches `read_parquet`: the executor parses
    // the same text first (see `compute_source`).
    let Ok(ast) = trawl_core::parser::parse(dsl) else {
        return SourceSelection {
            base,
            file_pattern: ANY_SERVICE.to_owned(),
            dirs: Vec::new(),
        };
    };

    let Narrowing { file_pattern, envs } = narrowing(&base, &ast);
    let from = start
        .naive_utc()
        .checked_sub_signed(padding())
        .unwrap_or(NaiveDateTime::MIN);
    let dirs = envs
        .iter()
        .flat_map(|env| {
            partition_dirs(
                &format!("{base}/{env}"),
                from,
                now.naive_utc(),
                &file_pattern,
            )
        })
        .collect();

    SourceSelection {
        base,
        file_pattern,
        dirs,
    }
}

/// The bytes of parquet `selection` reaches: the summed size of every
/// regular file its pattern matches in its directories.
///
/// This is a budget estimate, not a promise. The caller takes it under the
/// publication read guard, before the engine runs, so no canonical Parquet
/// publication in this process lands between the measurement and the read.
/// Retention takes no such guard: a directory that is gone, or a file that
/// vanishes between the listing and its `stat`, counts as zero. Any other
/// IO error is returned, so the caller cannot mistake an unreadable
/// directory for an empty one.
///
/// Symlinks are not followed and count as zero. `DuckDB` would read
/// through a live one, but trawld never writes a symlink into the data
/// root, which is owner-only (ADR-0052), so a link there is not data this
/// budget measures. A dangling link matches nothing in `DuckDB` either.
pub(crate) fn selection_bytes(selection: &SourceSelection) -> io::Result<u64> {
    let mut total: u64 = 0;
    for dir in &selection.dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            if !selection.matches(&entry.file_name()) {
                continue;
            }
            // `DirEntry::metadata` does not traverse a symlink.
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if meta.file_type().is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Ok(total)
}

/// [`TIME_FILTER_PADDING_SECS`] as a span.
fn padding() -> chrono::TimeDelta {
    chrono::TimeDelta::seconds(
        i64::try_from(TIME_FILTER_PADDING_SECS).expect("the padding fits in a signed span"),
    )
}

/// Time-scoped globs for one env root: hour-level for today, day-level
/// and/or hour-level for historical dates, existence-filtered. Returns
/// an empty vec when nothing on disk matches the window.
///
/// A historical start day is selected whole, every hour of it: the walk
/// starts at that day's midnight rather than at the padded start's hour.
fn time_scoped_globs(
    base: &str,
    duration: trawl_core::ast::TrawlDuration,
    file_pattern: &str,
) -> Vec<String> {
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
        .map(|dir| format!("'{dir}/{file_pattern}'"))
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

/// Date-scoped globs for one env root, for queries without a time filter.
///
/// Enumerates directories matching `YYYY-MM-DD` in the env base and
/// builds one recursive glob per date dir. Returns an empty vec when the
/// env base holds no date dirs.
fn date_scoped_globs(base: &str, file_pattern: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };

    let mut globs = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if is_date_dir_name(name_str) && entry.path().is_dir() {
            globs.push(format!("'{base}/{name_str}/**/{file_pattern}'"));
        }
    }

    // `read_dir` yields entries in filesystem order; sort so the emitted
    // source list is stable across calls.
    globs.sort();
    globs
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
    // Capture windows (ADR-0057): `compute_window_source`, `selection_bytes`
    // -----------------------------------------------------------------

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("test instant")
            .with_timezone(&Utc)
    }

    /// Create `{root}/{rel}` as a directory.
    fn mkdir(root: &Path, rel: &str) {
        std::fs::create_dir_all(root.join(rel)).unwrap();
    }

    /// Write `len` bytes to `{root}/{rel}`, creating its directory.
    fn write_len(root: &Path, rel: &str, len: usize) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; len]).unwrap();
    }

    /// The selected directories relative to `root`, for readable asserts.
    fn rel_dirs(selection: &SourceSelection, root: &Path) -> Vec<String> {
        let prefix = format!("{}/", root.to_str().unwrap());
        selection
            .dirs
            .iter()
            .map(|d| d.strip_prefix(&prefix).unwrap_or(d).to_owned())
            .collect()
    }

    #[test]
    fn a_window_starting_yesterday_selects_todays_partitions() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        for rel in [
            "prod/2026-10-22/22",
            "prod/2026-10-22/23",
            "prod/2026-10-23/00",
            "prod/2026-10-23/09",
        ] {
            mkdir(tmp.path(), rel);
        }

        let selection = window_source_at(
            base,
            "service=nginx",
            at("2026-10-22T23:50:00Z"),
            at("2026-10-23T09:30:00Z"),
        );

        assert_eq!(
            rel_dirs(&selection, tmp.path()),
            [
                "prod/2026-10-22/22",
                "prod/2026-10-22/23",
                "prod/2026-10-23/00",
                "prod/2026-10-23/09",
            ],
            "the padded start (22:50 yesterday) through today's hour"
        );
        assert_eq!(
            selection.source(),
            format!(
                "['{base}/prod/2026-10-22/22/nginx.parquet', \
                 '{base}/prod/2026-10-22/23/nginx.parquet', \
                 '{base}/prod/2026-10-23/00/nginx.parquet', \
                 '{base}/prod/2026-10-23/09/nginx.parquet']"
            )
        );
    }

    #[test]
    fn a_window_ending_last_week_still_selects_through_today() {
        // Only the start is passed: a late event with an old `_time` sits
        // in today's partition, so the window's end never trims the walk.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        write_len(tmp.path(), "prod/2026-10-15/nginx.parquet", 1);
        mkdir(tmp.path(), "prod/2026-10-16/10");
        mkdir(tmp.path(), "prod/2026-10-23/08");
        mkdir(tmp.path(), "prod/2026-10-14/23");

        let selection = window_source_at(
            base,
            "*",
            at("2026-10-15T12:00:00Z"),
            at("2026-10-23T08:05:00Z"),
        );

        assert_eq!(
            rel_dirs(&selection, tmp.path()),
            [
                "prod/2026-10-15",
                "prod/2026-10-16/10",
                "prod/2026-10-23/08"
            ],
            "a rolled-up start day, a later day and today; nothing before the padded start"
        );
    }

    #[test]
    fn the_padded_start_crosses_a_year_and_a_month_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        for rel in [
            "prod/2025-12-31/22",
            "prod/2025-12-31/23",
            "prod/2026-01-01/00",
            "prod/2026-01-01/02",
            "prod/2026-02-28/22",
            "prod/2026-02-28/23",
            "prod/2026-03-01/00",
        ] {
            mkdir(tmp.path(), rel);
        }

        let year = window_source_at(
            base,
            "*",
            at("2026-01-01T00:30:00Z"),
            at("2026-01-01T02:10:00Z"),
        );
        assert_eq!(
            rel_dirs(&year, tmp.path()),
            [
                "prod/2025-12-31/23",
                "prod/2026-01-01/00",
                "prod/2026-01-01/02"
            ],
            "00:30 minus the pad is 23:30 on the last day of the previous year"
        );

        let month = window_source_at(
            base,
            "*",
            at("2026-03-01T00:10:00Z"),
            at("2026-03-01T00:20:00Z"),
        );
        assert_eq!(
            rel_dirs(&month, tmp.path()),
            ["prod/2026-02-28/23", "prod/2026-03-01/00"],
            "00:10 minus the pad is 23:10 on the last day of February"
        );
    }

    #[test]
    fn a_missing_date_directory_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        // 2026-10-21 and 2026-10-22 never had data.
        mkdir(tmp.path(), "prod/2026-10-20/18");
        mkdir(tmp.path(), "prod/2026-10-23/07");

        let selection = window_source_at(
            base,
            "*",
            at("2026-10-20T18:30:00Z"),
            at("2026-10-23T07:45:00Z"),
        );

        assert_eq!(
            rel_dirs(&selection, tmp.path()),
            ["prod/2026-10-20/18", "prod/2026-10-23/07"]
        );
    }

    #[test]
    fn an_empty_window_selects_nothing_rather_than_every_date() {
        // `compute_source` falls back to every date directory when a
        // `last=` window finds none; a capture's budget must not see that.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        write_len(tmp.path(), "prod/2026-01-15/nginx.parquet", 10);

        let selection = window_source_at(
            base,
            "*",
            at("2026-10-23T09:00:00Z"),
            at("2026-10-23T09:15:00Z"),
        );

        assert!(selection.dirs.is_empty(), "{selection:?}");
        assert_eq!(selection.source(), format!("{base}/.no-such-env/*.parquet"));
        assert!(glob_matches(&selection.source()).is_empty());
        assert_eq!(selection_bytes(&selection).unwrap(), 0);
    }

    #[test]
    fn a_window_source_narrows_env_and_service_like_compute_source() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        mkdir(tmp.path(), "prod/2026-10-23/09");
        mkdir(tmp.path(), "lab/2026-10-23/09");
        let start = at("2026-10-23T09:10:00Z");
        let now = at("2026-10-23T09:20:00Z");

        let both = window_source_at(base, "env=lab service=nginx", start, now);
        assert_eq!(rel_dirs(&both, tmp.path()), ["lab/2026-10-23/09"]);
        assert_eq!(both.file_pattern, "nginx.parquet");

        let every_env = window_source_at(base, "*", start, now);
        let mut dirs = rel_dirs(&every_env, tmp.path());
        dirs.sort();
        assert_eq!(dirs, ["lab/2026-10-23/09", "prod/2026-10-23/09"]);
        assert_eq!(every_env.file_pattern, "*.parquet");

        // A value no on-disk env or file can carry selects nothing and is
        // never spliced into the source.
        let bad_env = window_source_at(base, "env=../evil", start, now);
        assert!(bad_env.dirs.is_empty());
        let bad_service = window_source_at(base, r#"service="x', '/etc""#, start, now);
        assert_eq!(bad_service.file_pattern, "*.parquet");
        assert!(!bad_service.source().contains("/etc"));

        let unparseable = window_source_at(base, "| | broken", start, now);
        assert_eq!(
            unparseable.source(),
            format!("{base}/.no-such-env/*.parquet")
        );
    }

    #[test]
    fn the_window_source_reads_the_clock_for_its_end() {
        // The public entry point: a window that started a minute ago
        // reaches the partition compaction is writing right now.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let now = Utc::now();
        let hour_dir = format!("prod/{}", now.format("%Y-%m-%d/%H"));
        write_len(tmp.path(), &format!("{hour_dir}/nginx.parquet"), 7);

        let selection =
            compute_window_source(base, "service=nginx", now - chrono::TimeDelta::minutes(1));

        assert!(
            rel_dirs(&selection, tmp.path()).contains(&hour_dir),
            "{selection:?}"
        );
        assert_eq!(selection_bytes(&selection).unwrap(), 7);
    }

    #[test]
    fn selection_bytes_sums_the_matching_regular_files() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        write_len(tmp.path(), "prod/2026-10-22/nginx.parquet", 100); // day-level
        write_len(tmp.path(), "prod/2026-10-22/23/postgres.parquet", 20);
        write_len(tmp.path(), "prod/2026-10-23/09/nginx.parquet", 3);
        write_len(tmp.path(), "prod/2026-10-23/09/redis.parquet", 4);
        // Never read by `*.parquet`: a compaction stage and a stray file.
        write_len(tmp.path(), "prod/2026-10-23/09/nginx.parquet.tmp", 1000);
        write_len(tmp.path(), "prod/2026-10-23/09/notes.txt", 1000);
        // Outside the selection: before the padded start.
        write_len(tmp.path(), "prod/2026-10-21/nginx.parquet", 1000);

        let start = at("2026-10-22T23:30:00Z");
        let now = at("2026-10-23T09:40:00Z");

        let every = window_source_at(base, "*", start, now);
        assert_eq!(selection_bytes(&every).unwrap(), 100 + 20 + 3 + 4);

        let nginx = window_source_at(base, "service=nginx", start, now);
        assert_eq!(selection_bytes(&nginx).unwrap(), 100 + 3);
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
        // A live link to a large file outside the data layout.
        write_len(tmp.path(), "elsewhere/big.parquet", 4096);
        std::os::unix::fs::symlink(
            tmp.path().join("elsewhere/big.parquet"),
            hour.join("linked.parquet"),
        )
        .unwrap();

        let selection = window_source_at(
            base,
            "*",
            at("2026-10-23T09:00:00Z"),
            at("2026-10-23T09:10:00Z"),
        );

        assert_eq!(rel_dirs(&selection, tmp.path()), ["prod/2026-10-23/09"]);
        assert_eq!(
            selection_bytes(&selection).unwrap(),
            5,
            "only the regular file counts; neither link is followed"
        );
    }

    #[test]
    fn selection_bytes_counts_a_vanished_directory_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        write_len(tmp.path(), "prod/2026-10-22/23/nginx.parquet", 9);
        write_len(tmp.path(), "prod/2026-10-23/09/nginx.parquet", 2);
        let selection = window_source_at(
            base,
            "*",
            at("2026-10-23T00:10:00Z"),
            at("2026-10-23T09:10:00Z"),
        );
        assert_eq!(selection_bytes(&selection).unwrap(), 11);

        // Retention removed the oldest hour after the walk listed it.
        std::fs::remove_dir_all(tmp.path().join("prod/2026-10-22/23")).unwrap();
        assert_eq!(selection_bytes(&selection).unwrap(), 2);
    }

    /// Drift guard: the file matcher `selection_bytes` sizes must reach
    /// the files `read_parquet` reaches through the same element, on every
    /// non-symlink shape a partition directory can hold.
    #[test]
    fn selection_bytes_matches_what_duckdb_glob_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let dir = "prod/2026-10-23/09";
        for name in [
            "nginx.parquet",
            "redis.parquet",
            ".parquet",
            ".hidden.parquet",
            "nginx.parquet.tmp",
            "upper.PARQUET",
            "nginx.parquetx",
        ] {
            write_len(tmp.path(), &format!("{dir}/{name}"), 1);
        }
        mkdir(tmp.path(), &format!("{dir}/dir.parquet"));

        for dsl in ["*", "service=nginx", "service=absent"] {
            let selection = window_source_at(
                base,
                dsl,
                at("2026-10-23T09:00:00Z"),
                at("2026-10-23T09:10:00Z"),
            );
            assert_eq!(rel_dirs(&selection, tmp.path()), [dir]);
            let element = format!("{base}/{dir}/{}", selection.file_pattern);
            let duckdb = u64::try_from(glob_matches(&element).len()).unwrap();
            assert_eq!(
                selection_bytes(&selection).unwrap(),
                duckdb,
                "one byte per file, so the counts must agree for {dsl:?}"
            );
        }
    }
}
