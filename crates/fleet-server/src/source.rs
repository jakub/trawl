//! Source glob computation for parquet file selection.
//!
//! Narrows the `read_parquet()` source argument based on time filters
//! and service names extracted from the DSL query, avoiding full-directory
//! scans when possible.

use chrono::Timelike as _;
use fleet_core::ast::{FieldFilter, FilterOp, FilterValue, SearchToken};

/// Time filter padding (seconds) to account for WAL compaction delay.
/// Events ingested at time T may land in the hourly partition for T + ~20s,
/// so we widen the file selection window by one hour.
const TIME_FILTER_PADDING_SECS: u64 = 3600;

/// Extract an exact service name from the search tokens, if present.
///
/// Only returns `Some` for simple equality filters (`service:nginx`).
/// Glob, regex, and other operators are ignored — we can't narrow the
/// file glob safely for those.
fn extract_service_filter(tokens: &[fleet_core::ast::Spanned<SearchToken>]) -> Option<&str> {
    tokens.iter().find_map(|t| {
        if let SearchToken::FieldFilter(FieldFilter {
            field,
            op: FilterOp::Eq,
            value: FilterValue::Literal(s),
        }) = &t.node
        {
            if field == "service" {
                return Some(s.as_str());
            }
        }
        None
    })
}

/// Compute the `read_parquet()` source argument, scoped to relevant
/// hour-directories when the query contains a time filter and/or
/// narrowed to a single service file when `service:X` is present.
///
/// Supports two-tier parquet layout: day-level files for consolidated
/// historical dates and hour-level files for today/unconsolidated dates.
/// Checks for day-level files with a cheap `stat()` before falling back
/// to hourly expansion.
///
/// Returns a `DuckDB` list literal like `['path/14/*.parquet', 'path/15/*.parquet']`
/// when time-scoping is possible, or falls back to the recursive glob.
pub(crate) fn compute_source(base_dir: &str, dsl: &str, fallback_glob: &str) -> String {
    let Ok(ast) = fleet_core::parser::parse(dsl) else {
        return fallback_glob.to_owned();
    };

    let service = extract_service_filter(&ast.search.tokens);
    let file_pattern = service.map_or_else(
        || "*.parquet".to_owned(),
        |s| {
            format!(
                "{}.parquet",
                crate::ingest::wal::sanitize_service_for_filename(s)
            )
        },
    );

    let time_filter = ast.search.tokens.iter().find_map(|t| {
        if let SearchToken::TimeFilter(tf) = &t.node {
            Some(tf.duration)
        } else {
            None
        }
    });

    let base = base_dir.trim_end_matches('/');

    let Some(duration) = time_filter else {
        // No time filter — use recursive glob, possibly service-narrowed.
        return format!("{base}/**/{file_pattern}");
    };

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
            // Today stays hourly — emit one glob per hour in range.
            let hour = cursor.format("%H");
            globs.push(format!("'{base}/{day}/{hour}/{file_pattern}'"));
            cursor += chrono::Duration::hours(1);
        } else {
            // Historical date — check for consolidated day-level file
            // and/or remaining hourly directories. Both can coexist
            // during partial rollup (one service consolidated, another not).
            let has_day = has_day_level_files(base, &day, &file_pattern);
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

    if globs.is_empty() {
        return format!("{base}/**/{file_pattern}");
    }

    // Filter out globs whose parent directory doesn't exist on disk.
    // This avoids sending DuckDB a list of entirely nonexistent paths,
    // which would trigger a "No files found" error before the executor
    // safety net catches it.
    let globs: Vec<String> = globs
        .into_iter()
        .filter(|g| {
            // globs are formatted as 'path/to/file_pattern' — strip quotes
            // and check the parent dir.
            let path = g.trim_matches('\'');
            std::path::Path::new(path)
                .parent()
                .is_some_and(std::path::Path::exists)
        })
        .collect();

    if globs.is_empty() {
        // All glob dirs were nonexistent — fall back to recursive glob.
        // The SQL time filter still provides correctness.
        return format!("{base}/**/{file_pattern}");
    }

    format!("[{}]", globs.join(", "))
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
        let source = compute_source("/data", "level:error", "/data/**/*.parquet");
        assert_eq!(source, "/data/**/*.parquet");
    }

    #[test]
    fn service_filter_narrows_glob() {
        // Exact service filter → narrow to service-specific file.
        let source = compute_source("/data", "service:nginx", "/data/**/*.parquet");
        assert_eq!(source, "/data/**/nginx.parquet");
    }

    #[test]
    fn service_glob_keeps_wildcard() {
        // Glob operator on service → can't narrow, keep *.parquet.
        let source = compute_source("/data", "service:ng*", "/data/**/*.parquet");
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
        let today = now.format("%Y-%m-%d").to_string();
        // Create hour dirs for current and recent hours.
        for h_offset in 0..=3 {
            let h = (now - chrono::Duration::hours(h_offset))
                .format("%H")
                .to_string();
            std::fs::create_dir_all(tmp.path().join(&today).join(&h)).unwrap();
        }
        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "last:1h", &fallback);
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
        // With 1h + 1h padding, should have ~2-3 hour entries.
        let count = source.matches("*.parquet").count();
        assert!(
            (2..=4).contains(&count),
            "expected 2-4 hour globs for last:1h, got {count}: {source}"
        );
    }

    #[test]
    fn service_and_time_filter_compose() {
        // Create temp dir with today's hour directories.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let now = chrono::Utc::now();
        let today = now.format("%Y-%m-%d").to_string();
        for h_offset in 0..=3 {
            let h = (now - chrono::Duration::hours(h_offset))
                .format("%H")
                .to_string();
            std::fs::create_dir_all(tmp.path().join(&today).join(&h)).unwrap();
        }
        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "service:nginx last:1h", &fallback);
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
        let source = compute_source("/data/", "last:1h", "/data/**/*.parquet");
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
        let day_dir = tmp.path().join(&yesterday);
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("nginx.parquet"), b"data").unwrap();

        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "service:nginx last:48h", &fallback);

        // Should include day-level path for yesterday (no /HH/ component).
        let expected_day_glob = format!("'{base}/{yesterday}/nginx.parquet'");
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
        let hour_dir = tmp.path().join(&yesterday).join("14");
        std::fs::create_dir_all(&hour_dir).unwrap();
        std::fs::write(hour_dir.join("nginx.parquet"), b"data").unwrap();

        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "service:nginx last:48h", &fallback);

        // Only hour 14 exists on disk, so only that glob survives filtering.
        let hourly_pattern = format!("{base}/{yesterday}/14/nginx.parquet");
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
        let day_dir = tmp.path().join(&yesterday);
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("nginx.parquet"), b"data").unwrap();
        std::fs::write(day_dir.join("postgres.parquet"), b"data").unwrap();

        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "last:48h", &fallback);

        // Should use day-level glob (*.parquet at date level).
        let expected_day_glob = format!("'{base}/{yesterday}/*.parquet'");
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
        let day_dir = tmp.path().join(&yesterday);
        std::fs::create_dir_all(&day_dir).unwrap();

        // nginx consolidated at day level.
        std::fs::write(day_dir.join("nginx.parquet"), b"data").unwrap();

        // postgres still in hourly dirs.
        let hour_dir = day_dir.join("14");
        std::fs::create_dir_all(&hour_dir).unwrap();
        std::fs::write(hour_dir.join("postgres.parquet"), b"data").unwrap();

        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "last:48h", &fallback);

        // Should include BOTH day-level glob and the existing hourly dir.
        let day_glob = format!("'{base}/{yesterday}/*.parquet'");
        let hourly_glob = format!("'{base}/{yesterday}/14/*.parquet'");
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
        // Dotted service name should be sanitized to match WAL/compaction filenames.
        let source = compute_source("/data", "service:api.v2", "/data/**/*.parquet");
        assert_eq!(
            source, "/data/**/api_v2.parquet",
            "dots should be replaced with underscores in file pattern"
        );
    }

    #[test]
    fn sanitized_service_with_time_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "service:host.name last:1h", &fallback);

        // Should use sanitized filename pattern.
        assert!(
            source.contains("host_name.parquet"),
            "expected sanitized service in time-scoped glob, got: {source}"
        );
        assert!(
            !source.contains("host.name.parquet"),
            "should not contain unsanitized service name, got: {source}"
        );
    }
}
