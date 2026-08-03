// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Source glob computation for parquet file selection.
//!
//! Narrows the `read_parquet()` source argument based on time filters
//! and service names extracted from the DSL query, avoiding full-directory
//! scans when possible.

use chrono::Timelike as _;
use trawl_core::ast::{FieldFilter, FilterOp, FilterValue, SearchToken};

/// Time filter padding (seconds) to account for WAL compaction delay.
/// Events ingested at time T may land in the hourly partition for T + ~20s,
/// so we widen the file selection window by one hour.
const TIME_FILTER_PADDING_SECS: u64 = 3600;

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
/// disk is searched. `service=X` narrows the file pattern — VERBATIM
/// (path encoding is injective by validation, so `api.v2` and `api_v2`
/// are distinct files and pruning is exact), but only when the literal
/// satisfies the same `is_valid_service_name` predicate ingest enforces:
/// a value no on-disk file can carry is also a value that must never be
/// spliced into the glob list, so it falls back to the wildcard pattern.
/// A time filter scopes to the relevant date/hour directories; without
/// one, date-formatted dirs are enumerated per env.
///
/// Returns a `DuckDB` list literal like
/// `['data/prod/2026-08-02/14/*.parquet', ...]` when scoping is
/// possible, or falls back to a recursive glob. Pruning is an
/// optimization only: the SQL WHERE clause always re-filters, so a
/// broader source is never incorrect.
pub(crate) fn compute_source(base_dir: &str, dsl: &str, fallback_glob: &str) -> String {
    let Ok(ast) = trawl_core::parser::parse(dsl) else {
        return fallback_glob.to_owned();
    };

    // The literal is interpolated verbatim into single-quoted glob
    // entries, so anything outside the ingest-side charset (quotes,
    // separators, slashes, dots at the front) is rejected here rather
    // than allowed to close one path and open another.
    let service = extract_eq_filter(&ast.search, "service")
        .filter(|s| trawl_config::is_valid_service_name(s));
    let file_pattern = service.map_or_else(|| "*.parquet".to_owned(), |s| format!("{s}.parquet"));

    let base = base_dir.trim_end_matches('/');

    let envs: Vec<String> = match extract_eq_filter(&ast.search, "env") {
        Some(env)
            if trawl_config::is_valid_env_name(env)
                && !trawl_config::RESERVED_ENV_NAMES.contains(&env) =>
        {
            vec![env.to_owned()]
        }
        Some(_) => {
            // A value no on-disk env can carry (they were validated at
            // ingest) — match nothing; the executor treats "no files" as
            // an empty result and the SQL filter keeps hot rows correct.
            return format!("{base}/.no-such-env/{file_pattern}");
        }
        None => crate::env_dirs::list_env_names(std::path::Path::new(base)),
    };

    if envs.is_empty() {
        // Cold start (no env directories yet) — nothing to prune.
        return format!("{base}/**/{file_pattern}");
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
        return format!("{base}/**/{file_pattern}");
    }
    format!("[{}]", globs.join(", "))
}

/// Time-scoped globs for one env root: hour-level for today, day-level
/// and/or hour-level for historical dates, existence-filtered. Returns
/// an empty vec when nothing on disk matches the window.
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
    let today = now.format("%Y-%m-%d").to_string();

    let mut globs = Vec::new();
    let mut cursor = start
        .date_naive()
        .and_hms_opt(start.hour(), 0, 0)
        .expect("valid hour from Timelike::hour()");
    let end = now.naive_utc();

    while cursor <= end {
        let day = cursor.format("%Y-%m-%d").to_string();

        if day == today {
            // Today may have day-level consolidated files from a
            // previous server session or an earlier daily rollup.
            // Check once, then emit hourly globs for the time range.
            if has_day_level_files(base, &day, file_pattern) {
                globs.push(format!("'{base}/{day}/{file_pattern}'"));
            }
            while cursor <= end {
                let hour = cursor.format("%H");
                globs.push(format!("'{base}/{day}/{hour}/{file_pattern}'"));
                cursor += chrono::Duration::hours(1);
            }
        } else {
            // Historical date — check for consolidated day-level file
            // and/or remaining hourly directories. Both can coexist
            // during partial rollup (one service consolidated, another not).
            let has_day = has_day_level_files(base, &day, file_pattern);
            let has_hours = has_hour_dirs(base, &day);

            if has_day {
                globs.push(format!("'{base}/{day}/{file_pattern}'"));
            }
            if has_hours || !has_day {
                for h in 0..24_u32 {
                    globs.push(format!("'{base}/{day}/{h:02}/{file_pattern}'"));
                }
            }

            // Skip to next day (advance cursor past remaining hours of this day).
            cursor = (cursor.date() + chrono::Duration::days(1))
                .and_hms_opt(0, 0, 0)
                .expect("valid midnight from date + 1 day");
        }
    }

    // Filter out globs whose parent directory doesn't exist on disk.
    // This avoids sending DuckDB a list of entirely nonexistent paths,
    // which would trigger a "No files found" error before the executor
    // safety net catches it.
    globs
        .into_iter()
        .filter(|g| {
            // globs are formatted as 'path/to/file_pattern' — strip quotes
            // and check the parent dir.
            let path = g.trim_matches('\'');
            std::path::Path::new(path)
                .parent()
                .is_some_and(std::path::Path::exists)
        })
        .collect()
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
        // Match YYYY-MM-DD (exactly 10 chars, digits and dashes in right places).
        if is_date_dir_name(name_str) && entry.path().is_dir() {
            globs.push(format!("'{base}/{name_str}/**/{file_pattern}'"));
        }
    }

    // Sort for deterministic ordering.
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

    #[test]
    fn no_time_filter_returns_recursive_glob() {
        // No time filter, no service → broad glob.
        let source = compute_source("/data", "level=error", "/data/**/*.parquet");
        assert_eq!(source, "/data/**/*.parquet");
    }

    #[test]
    fn service_filter_narrows_glob() {
        // Exact service filter → narrow to service-specific file.
        let source = compute_source("/data", "service=nginx", "/data/**/*.parquet");
        assert_eq!(source, "/data/**/nginx.parquet");
    }

    #[test]
    fn service_glob_keeps_wildcard() {
        // Glob operator on service → can't narrow, keep *.parquet.
        let source = compute_source("/data", "service=ng*", "/data/**/*.parquet");
        assert_eq!(source, "/data/**/*.parquet");
    }

    #[test]
    fn bad_dsl_returns_fallback() {
        let source = compute_source("/data", "broken {{{ query", "/data/**/*.parquet");
        assert_eq!(source, "/data/**/*.parquet");
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
        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "last=1h", &fallback);
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
        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "service=nginx last=1h", &fallback);
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
        let source = compute_source("/data/", "last=1h", "/data/**/*.parquet");
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

        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "service=nginx last=48h", &fallback);

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

        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "service=nginx last=48h", &fallback);

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

        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "last=48h", &fallback);

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

        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "last=48h", &fallback);

        // Should include BOTH day-level glob and the existing hourly dir.
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
        // INVERTED (ADR-0009): filenames carry the service verbatim, so a
        // dotted service prunes to its own file — `api.v2` and `api_v2`
        // are distinct.
        let source = compute_source("/data", "service=api.v2", "/data/**/*.parquet");
        assert_eq!(
            source, "/data/**/api.v2.parquet",
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
        let fallback = format!("{base}/**/*.parquet");

        // env only.
        let source = compute_source(base, "env=prod", &fallback);
        assert!(source.contains("/prod/"), "got: {source}");
        assert!(
            !source.contains("/lab/"),
            "env=prod must prune lab: {source}"
        );

        // service only — both envs searched, file pattern pinned.
        let source = compute_source(base, "service=nginx", &fallback);
        assert!(source.contains("/prod/"), "got: {source}");
        assert!(source.contains("/lab/"), "got: {source}");
        assert!(source.contains("nginx.parquet"), "got: {source}");
        assert!(!source.contains("*.parquet"), "got: {source}");

        // both.
        let source = compute_source(base, "env=lab service=nginx", &fallback);
        assert!(source.contains("/lab/"), "got: {source}");
        assert!(!source.contains("/prod/"), "got: {source}");
        assert!(source.contains("nginx.parquet"), "got: {source}");

        // neither — every env, wildcard pattern.
        let source = compute_source(base, "level=error", &fallback);
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
        let fallback = format!("{base}/**/*.parquet");

        let dotted = compute_source(base, "service=api.v2", &fallback);
        let underscored = compute_source(base, "service=api_v2", &fallback);
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
        let fallback = format!("{base}/**/*.parquet");

        let source = compute_source(base, "env=../evil", &fallback);
        assert!(
            source.contains(".no-such-env"),
            "invalid env must yield a no-match source, got: {source}"
        );
        assert!(!source.contains("/prod/"), "got: {source}");
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

        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "service=trawld last=1h", &fallback);

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

        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "last=1h", &fallback);

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
        let fallback = format!("{base}/**/*.parquet");

        for dsl in [
            r#"service="nginx', '/**/*""#,
            r#"service="nginx', '/etc/shadow""#,
            "service=../../escaped",
            "service=.hidden",
        ] {
            let source = compute_source(base, dsl, &fallback);
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
        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "service=host.name last=1h", &fallback);

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
}
