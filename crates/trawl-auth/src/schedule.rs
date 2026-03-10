//! `SQLite`-backed storage for scheduled queries and report runs.
//!
//! [`ScheduleStore`] manages scheduled execution of saved queries and stores
//! their results. Shares the auth database file with [`KeyStore`] and
//! [`SavedQueryStore`] via separate connection (safe with WAL mode).

use std::path::Path;

use chrono::Utc;
use rusqlite::params;

use crate::error::AuthError;
use crate::saved::SavedQuery;

const MIN_INTERVAL_SECS: u64 = 60;

/// A schedule attached to a saved query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    pub id: i64,
    pub saved_query_id: i64,
    pub key_id: i64,
    pub interval_secs: u64,
    pub max_runs: Option<u64>,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// A single report run (result blob fetched separately via [`ScheduleStore::get_run_result`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportRun {
    pub id: i64,
    pub schedule_id: i64,
    pub saved_query_id: i64,
    pub query: String,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub duration_ms: Option<u64>,
    pub row_count: Option<usize>,
    pub error_message: Option<String>,
}

/// Parse a duration string (same syntax as the DSL `last:` filter) into seconds.
///
/// Supported units: `s` (seconds), `m` (minutes), `h` (hours), `d` (days), `w` (weeks).
/// Minimum interval is 60 seconds.
pub fn parse_interval(s: &str) -> Result<u64, AuthError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(AuthError::IntervalTooShort { secs: 0 });
    }

    let (digits, unit) = s.split_at(s.len() - 1);
    let value: u64 = digits
        .parse()
        .map_err(|_| AuthError::IntervalTooShort { secs: 0 })?;

    let secs = match unit {
        "s" => value,
        "m" => value * 60,
        "h" => value * 3600,
        "d" => value * 86400,
        "w" => value * 604_800,
        _ => return Err(AuthError::IntervalTooShort { secs: 0 }),
    };

    if secs < MIN_INTERVAL_SECS {
        return Err(AuthError::IntervalTooShort { secs });
    }

    Ok(secs)
}

/// Format seconds into a human-readable duration string (e.g. "5m", "1h", "24h").
pub fn format_interval(secs: u64) -> String {
    if secs.is_multiple_of(604_800) {
        format!("{}w", secs / 604_800)
    } else if secs.is_multiple_of(86400) {
        format!("{}d", secs / 86400)
    } else if secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn row_to_schedule(row: &rusqlite::Row<'_>) -> Result<Schedule, rusqlite::Error> {
    Ok(Schedule {
        id: row.get(0)?,
        saved_query_id: row.get(1)?,
        key_id: row.get(2)?,
        interval_secs: row.get(3)?,
        max_runs: row.get(4)?,
        enabled: row.get::<_, i64>(5)? != 0,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn row_to_report_run(row: &rusqlite::Row<'_>) -> Result<ReportRun, rusqlite::Error> {
    Ok(ReportRun {
        id: row.get(0)?,
        schedule_id: row.get(1)?,
        saved_query_id: row.get(2)?,
        query: row.get(3)?,
        status: row.get(4)?,
        started_at: row.get(5)?,
        finished_at: row.get(6)?,
        duration_ms: row.get(7)?,
        row_count: row.get(8)?,
        error_message: row.get(9)?,
    })
}

/// `SQLite`-backed storage for schedules and report runs.
#[derive(Debug)]
pub struct ScheduleStore {
    conn: rusqlite::Connection,
}

impl ScheduleStore {
    /// Open the auth database at the given path and ensure schedule tables exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AuthError> {
        let conn = rusqlite::Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
        )?;
        let store = Self { conn };
        store.initialize()?;
        Ok(store)
    }

    /// Open an in-memory database (for testing).
    pub fn open_in_memory() -> Result<Self, AuthError> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;")?;
        let store = Self { conn };
        store.initialize()?;
        Ok(store)
    }

    /// Ensure schedule and `report_runs` tables exist.
    fn initialize(&self) -> Result<(), AuthError> {
        // We need the saved_queries table to exist for FK references.
        // In production it's created by SavedQueryStore on the same db file.
        // For tests, create a minimal version.
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS saved_queries (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                key_id      INTEGER NOT NULL,
                name        TEXT    NOT NULL,
                query       TEXT    NOT NULL,
                created_at  TEXT    NOT NULL,
                updated_at  TEXT    NOT NULL,
                UNIQUE (key_id, name)
            );

            CREATE TABLE IF NOT EXISTS schedules (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                saved_query_id  INTEGER NOT NULL UNIQUE,
                key_id          INTEGER NOT NULL,
                interval_secs   INTEGER NOT NULL,
                max_runs        INTEGER,
                enabled         INTEGER NOT NULL DEFAULT 1,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL,
                FOREIGN KEY (saved_query_id) REFERENCES saved_queries(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_schedules_enabled ON schedules (enabled);

            CREATE TABLE IF NOT EXISTS report_runs (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                schedule_id     INTEGER NOT NULL,
                saved_query_id  INTEGER NOT NULL,
                query           TEXT NOT NULL,
                status          TEXT NOT NULL,
                started_at      TEXT NOT NULL,
                finished_at     TEXT,
                duration_ms     INTEGER,
                row_count       INTEGER,
                error_message   TEXT,
                result_data     BLOB,
                FOREIGN KEY (schedule_id) REFERENCES schedules(id) ON DELETE CASCADE,
                FOREIGN KEY (saved_query_id) REFERENCES saved_queries(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_report_runs_schedule
                ON report_runs (schedule_id, started_at DESC);",
        )?;
        Ok(())
    }

    /// Create a schedule for a saved query.
    pub fn create_schedule(
        &self,
        saved_query_id: i64,
        key_id: i64,
        interval_secs: u64,
        max_runs: Option<u64>,
    ) -> Result<Schedule, AuthError> {
        let now = Utc::now().to_rfc3339();

        match self.conn.execute(
            "INSERT INTO schedules (saved_query_id, key_id, interval_secs, max_runs, enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)",
            params![
                saved_query_id,
                key_id,
                i64::try_from(interval_secs).unwrap_or(i64::MAX),
                max_runs.map(|v| i64::try_from(v).unwrap_or(i64::MAX)),
                &now,
                &now,
            ],
        ) {
            Ok(_) => {
                let id = self.conn.last_insert_rowid();

                tracing::info!(
                    event_type = "schedule_created",
                    schedule_id = id,
                    saved_query_id,
                    key_id,
                    interval_secs,
                    "Schedule created"
                );

                Ok(Schedule {
                    id,
                    saved_query_id,
                    key_id,
                    interval_secs,
                    max_runs,
                    enabled: true,
                    created_at: now.clone(),
                    updated_at: now,
                })
            }
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(AuthError::ScheduleExists { saved_query_id })
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Update an existing schedule. Returns `NotFound` if not owned by `key_id`.
    pub fn update_schedule(
        &self,
        id: i64,
        key_id: i64,
        interval_secs: u64,
        max_runs: Option<u64>,
        enabled: bool,
    ) -> Result<Schedule, AuthError> {
        let now = Utc::now().to_rfc3339();

        let updated = self.conn.execute(
            "UPDATE schedules
             SET interval_secs = ?1, max_runs = ?2, enabled = ?3, updated_at = ?4
             WHERE id = ?5 AND key_id = ?6",
            params![
                i64::try_from(interval_secs).unwrap_or(i64::MAX),
                max_runs.map(|v| i64::try_from(v).unwrap_or(i64::MAX)),
                i64::from(enabled),
                &now,
                id,
                key_id,
            ],
        )?;

        if updated == 0 {
            return Err(AuthError::NotFound {
                id,
                resource: "schedule".into(),
            });
        }

        tracing::info!(
            event_type = "schedule_updated",
            schedule_id = id,
            key_id,
            interval_secs,
            enabled,
            "Schedule updated"
        );

        let mut stmt = self.conn.prepare(
            "SELECT id, saved_query_id, key_id, interval_secs, max_runs, enabled, created_at, updated_at
             FROM schedules WHERE id = ?1",
        )?;
        stmt.query_row(params![id], row_to_schedule)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => AuthError::NotFound {
                    id,
                    resource: "schedule".into(),
                },
                e => e.into(),
            })
    }

    /// Delete a schedule by its saved query id. Returns `NotFound` if not owned by `key_id`.
    pub fn delete_schedule(&self, saved_query_id: i64, key_id: i64) -> Result<(), AuthError> {
        let deleted = self.conn.execute(
            "DELETE FROM schedules WHERE saved_query_id = ?1 AND key_id = ?2",
            params![saved_query_id, key_id],
        )?;

        if deleted == 0 {
            return Err(AuthError::NotFound {
                id: saved_query_id,
                resource: "schedule".into(),
            });
        }

        tracing::info!(
            event_type = "schedule_deleted",
            saved_query_id,
            key_id,
            "Schedule deleted"
        );

        Ok(())
    }

    /// Get the schedule for a given saved query, if it exists and is owned by `key_id`.
    pub fn get_schedule_for_saved_query(
        &self,
        saved_query_id: i64,
        key_id: i64,
    ) -> Result<Option<Schedule>, AuthError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, saved_query_id, key_id, interval_secs, max_runs, enabled, created_at, updated_at
             FROM schedules
             WHERE saved_query_id = ?1 AND key_id = ?2",
        )?;

        match stmt.query_row(params![saved_query_id, key_id], row_to_schedule) {
            Ok(schedule) => Ok(Some(schedule)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// List all enabled schedules with their associated saved queries.
    /// Used by the scheduler — no ownership check (internal/cross-user).
    pub fn list_enabled_schedules(&self) -> Result<Vec<(Schedule, SavedQuery)>, AuthError> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.saved_query_id, s.key_id, s.interval_secs, s.max_runs, s.enabled,
                    s.created_at, s.updated_at,
                    sq.id, sq.key_id, sq.name, sq.query, sq.created_at, sq.updated_at
             FROM schedules s
             JOIN saved_queries sq ON sq.id = s.saved_query_id
             WHERE s.enabled = 1",
        )?;

        let results = stmt
            .query_map([], |row| {
                let schedule = Schedule {
                    id: row.get(0)?,
                    saved_query_id: row.get(1)?,
                    key_id: row.get(2)?,
                    interval_secs: row.get(3)?,
                    max_runs: row.get(4)?,
                    enabled: row.get::<_, i64>(5)? != 0,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                };
                let saved = SavedQuery {
                    id: row.get(8)?,
                    key_id: row.get(9)?,
                    name: row.get(10)?,
                    query: row.get(11)?,
                    created_at: row.get(12)?,
                    updated_at: row.get(13)?,
                };
                Ok((schedule, saved))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(results)
    }

    /// Start a new run. Returns `None` if a run with status `running` already exists
    /// for this schedule (prevents overlapping executions).
    pub fn start_run(
        &self,
        schedule_id: i64,
        saved_query_id: i64,
        query: &str,
    ) -> Result<Option<i64>, AuthError> {
        let now = Utc::now().to_rfc3339();

        // Atomic: only insert if no running run exists for this schedule.
        let inserted = self.conn.execute(
            "INSERT INTO report_runs (schedule_id, saved_query_id, query, status, started_at)
             SELECT ?1, ?2, ?3, 'running', ?4
             WHERE NOT EXISTS (
                 SELECT 1 FROM report_runs
                 WHERE schedule_id = ?1 AND status = 'running'
             )",
            params![schedule_id, saved_query_id, query, &now],
        )?;

        if inserted == 0 {
            return Ok(None);
        }

        let id = self.conn.last_insert_rowid();

        tracing::info!(
            event_type = "report_run_started",
            run_id = id,
            schedule_id,
            saved_query_id,
            "Report run started"
        );

        Ok(Some(id))
    }

    /// Finish a run with status, timing, and optional compressed result blob.
    #[allow(clippy::too_many_arguments)]
    pub fn finish_run(
        &self,
        run_id: i64,
        status: &str,
        duration_ms: u64,
        row_count: Option<usize>,
        error_message: Option<&str>,
        result_data: Option<&[u8]>,
    ) -> Result<(), AuthError> {
        let now = Utc::now().to_rfc3339();

        self.conn.execute(
            "UPDATE report_runs
             SET status = ?1, finished_at = ?2, duration_ms = ?3, row_count = ?4,
                 error_message = ?5, result_data = ?6
             WHERE id = ?7",
            params![
                status,
                &now,
                i64::try_from(duration_ms).unwrap_or(i64::MAX),
                row_count.map(|v| i64::try_from(v).unwrap_or(i64::MAX)),
                error_message,
                result_data,
                run_id,
            ],
        )?;

        tracing::info!(
            event_type = "report_run_finished",
            run_id,
            status,
            duration_ms,
            "Report run finished"
        );

        Ok(())
    }

    /// List runs for a saved query, paginated. Excludes result blobs.
    pub fn list_runs(
        &self,
        saved_query_id: i64,
        key_id: i64,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ReportRun>, AuthError> {
        let mut stmt = self.conn.prepare(
            "SELECT r.id, r.schedule_id, r.saved_query_id, r.query, r.status,
                    r.started_at, r.finished_at, r.duration_ms, r.row_count, r.error_message
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             WHERE r.saved_query_id = ?1 AND s.key_id = ?2
             ORDER BY r.started_at DESC
             LIMIT ?3 OFFSET ?4",
        )?;

        let runs = stmt
            .query_map(
                params![
                    saved_query_id,
                    key_id,
                    i64::try_from(limit).unwrap_or(i64::MAX),
                    i64::try_from(offset).unwrap_or(0),
                ],
                row_to_report_run,
            )?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(runs)
    }

    /// Get a single run by id (no result blob). Checks ownership via schedule.
    pub fn get_run(&self, run_id: i64, key_id: i64) -> Result<Option<ReportRun>, AuthError> {
        let mut stmt = self.conn.prepare(
            "SELECT r.id, r.schedule_id, r.saved_query_id, r.query, r.status,
                    r.started_at, r.finished_at, r.duration_ms, r.row_count, r.error_message
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             WHERE r.id = ?1 AND s.key_id = ?2",
        )?;

        match stmt.query_row(params![run_id, key_id], row_to_report_run) {
            Ok(run) => Ok(Some(run)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get the compressed result blob for a run. Checks ownership via schedule.
    pub fn get_run_result(&self, run_id: i64, key_id: i64) -> Result<Option<Vec<u8>>, AuthError> {
        let mut stmt = self.conn.prepare(
            "SELECT r.result_data
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             WHERE r.id = ?1 AND s.key_id = ?2",
        )?;

        match stmt.query_row(params![run_id, key_id], |row| {
            row.get::<_, Option<Vec<u8>>>(0)
        }) {
            Ok(data) => Ok(data),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Count runs for a schedule.
    pub fn count_runs(&self, schedule_id: i64) -> Result<u64, AuthError> {
        let count: u64 = self.conn.query_row(
            "SELECT COUNT(*) FROM report_runs WHERE schedule_id = ?1",
            params![schedule_id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Get the most recent run for a schedule (for display in schedule responses).
    pub fn latest_run(&self, schedule_id: i64) -> Result<Option<ReportRun>, AuthError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, schedule_id, saved_query_id, query, status,
                    started_at, finished_at, duration_ms, row_count, error_message
             FROM report_runs
             WHERE schedule_id = ?1
             ORDER BY started_at DESC
             LIMIT 1",
        )?;

        match stmt.query_row(params![schedule_id], row_to_report_run) {
            Ok(run) => Ok(Some(run)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Mark any runs with status `running` as `error` (crash recovery on startup).
    pub fn cleanup_stale_runs(&self) -> Result<usize, AuthError> {
        let now = Utc::now().to_rfc3339();
        let updated = self.conn.execute(
            "UPDATE report_runs
             SET status = 'error', finished_at = ?1, error_message = 'interrupted by server restart'
             WHERE status = 'running'",
            params![&now],
        )?;

        if updated > 0 {
            tracing::warn!(
                event_type = "stale_runs_cleaned",
                count = updated,
                "Marked stale running report runs as error"
            );
        }

        Ok(updated)
    }

    /// Delete old runs for retention. Keeps at most `max_per_schedule` runs per schedule,
    /// and deletes any runs older than `max_age_days`.
    pub fn delete_old_runs(
        &self,
        max_age_days: u64,
        max_per_schedule: u64,
    ) -> Result<usize, AuthError> {
        let cutoff =
            Utc::now() - chrono::Duration::days(i64::try_from(max_age_days).unwrap_or(365));
        let cutoff_str = cutoff.to_rfc3339();

        // Delete runs older than max_age_days.
        let age_deleted = self.conn.execute(
            "DELETE FROM report_runs WHERE started_at < ?1",
            params![&cutoff_str],
        )?;

        // Delete excess runs per schedule (keep most recent max_per_schedule).
        let excess_deleted = self.conn.execute(
            "DELETE FROM report_runs
             WHERE id NOT IN (
                 SELECT id FROM (
                     SELECT id, ROW_NUMBER() OVER (
                         PARTITION BY schedule_id ORDER BY started_at DESC
                     ) AS rn
                     FROM report_runs
                 )
                 WHERE rn <= ?1
             )",
            params![i64::try_from(max_per_schedule).unwrap_or(i64::MAX)],
        )?;

        let total = age_deleted + excess_deleted;
        if total > 0 {
            tracing::info!(
                event_type = "old_runs_deleted",
                age_deleted,
                excess_deleted,
                "Deleted old report runs"
            );
        }

        Ok(total)
    }

    /// Count total runs for a saved query (for pagination).
    pub fn count_runs_for_saved_query(
        &self,
        saved_query_id: i64,
        key_id: i64,
    ) -> Result<usize, AuthError> {
        let count: usize = self.conn.query_row(
            "SELECT COUNT(*)
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             WHERE r.saved_query_id = ?1 AND s.key_id = ?2",
            params![saved_query_id, key_id],
            |row| row.get(0),
        )?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> ScheduleStore {
        ScheduleStore::open_in_memory().expect("failed to open in-memory store")
    }

    /// Insert a saved query directly for test purposes.
    fn insert_saved_query(store: &ScheduleStore, key_id: i64, name: &str) -> i64 {
        let now = Utc::now().to_rfc3339();
        store
            .conn
            .execute(
                "INSERT INTO saved_queries (key_id, name, query, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![key_id, name, "level=error", &now, &now],
            )
            .unwrap();
        store.conn.last_insert_rowid()
    }

    #[test]
    fn open_in_memory_succeeds() {
        let _store = test_store();
    }

    // --- interval parsing ---

    #[test]
    fn parse_interval_valid() {
        assert_eq!(parse_interval("60s").unwrap(), 60);
        assert_eq!(parse_interval("5m").unwrap(), 300);
        assert_eq!(parse_interval("1h").unwrap(), 3600);
        assert_eq!(parse_interval("1d").unwrap(), 86400);
        assert_eq!(parse_interval("1w").unwrap(), 604_800);
    }

    #[test]
    fn parse_interval_rejects_too_short() {
        assert!(matches!(
            parse_interval("30s"),
            Err(AuthError::IntervalTooShort { secs: 30 })
        ));
        assert!(matches!(
            parse_interval("0s"),
            Err(AuthError::IntervalTooShort { secs: 0 })
        ));
    }

    #[test]
    fn parse_interval_rejects_invalid() {
        assert!(parse_interval("").is_err());
        assert!(parse_interval("abc").is_err());
        assert!(parse_interval("5x").is_err());
    }

    #[test]
    fn format_interval_roundtrip() {
        assert_eq!(format_interval(60), "1m");
        assert_eq!(format_interval(300), "5m");
        assert_eq!(format_interval(3600), "1h");
        assert_eq!(format_interval(86400), "1d");
        assert_eq!(format_interval(604_800), "1w");
        assert_eq!(format_interval(90), "90s");
    }

    // --- schedule CRUD ---

    #[test]
    fn create_and_get_schedule() {
        let store = test_store();
        let sq_id = insert_saved_query(&store, 1, "test query");

        let schedule = store.create_schedule(sq_id, 1, 300, None).unwrap();
        assert_eq!(schedule.saved_query_id, sq_id);
        assert_eq!(schedule.interval_secs, 300);
        assert!(schedule.enabled);
        assert!(schedule.max_runs.is_none());

        let fetched = store
            .get_schedule_for_saved_query(sq_id, 1)
            .unwrap()
            .expect("schedule should exist");
        assert_eq!(fetched.id, schedule.id);
    }

    #[test]
    fn duplicate_schedule_returns_error() {
        let store = test_store();
        let sq_id = insert_saved_query(&store, 1, "test");

        store.create_schedule(sq_id, 1, 300, None).unwrap();
        let result = store.create_schedule(sq_id, 1, 600, None);

        assert!(matches!(result, Err(AuthError::ScheduleExists { .. })));
    }

    #[test]
    fn update_schedule() {
        let store = test_store();
        let sq_id = insert_saved_query(&store, 1, "test");

        let created = store.create_schedule(sq_id, 1, 300, None).unwrap();
        let updated = store
            .update_schedule(created.id, 1, 600, Some(10), false)
            .unwrap();

        assert_eq!(updated.interval_secs, 600);
        assert_eq!(updated.max_runs, Some(10));
        assert!(!updated.enabled);
    }

    #[test]
    fn update_other_users_schedule_returns_not_found() {
        let store = test_store();
        let sq_id = insert_saved_query(&store, 1, "test");

        let created = store.create_schedule(sq_id, 1, 300, None).unwrap();
        let result = store.update_schedule(created.id, 2, 600, None, true);

        assert!(matches!(result, Err(AuthError::NotFound { .. })));
    }

    #[test]
    fn delete_schedule() {
        let store = test_store();
        let sq_id = insert_saved_query(&store, 1, "test");

        store.create_schedule(sq_id, 1, 300, None).unwrap();
        store.delete_schedule(sq_id, 1).unwrap();

        let fetched = store.get_schedule_for_saved_query(sq_id, 1).unwrap();
        assert!(fetched.is_none());
    }

    #[test]
    fn delete_other_users_schedule_returns_not_found() {
        let store = test_store();
        let sq_id = insert_saved_query(&store, 1, "test");

        store.create_schedule(sq_id, 1, 300, None).unwrap();
        let result = store.delete_schedule(sq_id, 2);

        assert!(matches!(result, Err(AuthError::NotFound { .. })));
    }

    #[test]
    fn user_isolation() {
        let store = test_store();
        let sq1 = insert_saved_query(&store, 1, "user1 query");
        let sq2 = insert_saved_query(&store, 2, "user2 query");

        store.create_schedule(sq1, 1, 300, None).unwrap();
        store.create_schedule(sq2, 2, 600, None).unwrap();

        // User 1 can't see user 2's schedule.
        assert!(
            store
                .get_schedule_for_saved_query(sq2, 1)
                .unwrap()
                .is_none()
        );
        // User 2 can't see user 1's schedule.
        assert!(
            store
                .get_schedule_for_saved_query(sq1, 2)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cascade_on_saved_query_delete() {
        let store = test_store();
        let sq_id = insert_saved_query(&store, 1, "test");
        let schedule = store.create_schedule(sq_id, 1, 300, None).unwrap();

        // Start a run so we have data in report_runs too.
        let run_id = store
            .start_run(schedule.id, sq_id, "level=error")
            .unwrap()
            .unwrap();
        store
            .finish_run(run_id, "success", 100, Some(5), None, None)
            .unwrap();

        // Delete the saved query — should cascade to schedule and runs.
        store
            .conn
            .execute("DELETE FROM saved_queries WHERE id = ?1", params![sq_id])
            .unwrap();

        assert!(
            store
                .get_schedule_for_saved_query(sq_id, 1)
                .unwrap()
                .is_none()
        );
        assert_eq!(store.count_runs(schedule.id).unwrap(), 0);
    }

    // --- run lifecycle ---

    #[test]
    fn start_and_finish_run() {
        let store = test_store();
        let sq_id = insert_saved_query(&store, 1, "test");
        let schedule = store.create_schedule(sq_id, 1, 300, None).unwrap();

        let run_id = store
            .start_run(schedule.id, sq_id, "level=error")
            .unwrap()
            .expect("should start run");

        // Run should be in progress.
        let run = store.get_run(run_id, 1).unwrap().unwrap();
        assert_eq!(run.status, "running");

        // Finish it.
        store
            .finish_run(
                run_id,
                "success",
                150,
                Some(42),
                None,
                Some(b"compressed-data"),
            )
            .unwrap();

        let finished = store.get_run(run_id, 1).unwrap().unwrap();
        assert_eq!(finished.status, "success");
        assert_eq!(finished.duration_ms, Some(150));
        assert_eq!(finished.row_count, Some(42));

        // Check result blob.
        let blob = store.get_run_result(run_id, 1).unwrap().unwrap();
        assert_eq!(blob, b"compressed-data");
    }

    #[test]
    fn concurrent_run_prevention() {
        let store = test_store();
        let sq_id = insert_saved_query(&store, 1, "test");
        let schedule = store.create_schedule(sq_id, 1, 300, None).unwrap();

        let run_id = store.start_run(schedule.id, sq_id, "q").unwrap();
        assert!(run_id.is_some());

        // Second start should return None (already running).
        let second = store.start_run(schedule.id, sq_id, "q").unwrap();
        assert!(second.is_none());

        // After finishing, should be able to start again.
        store
            .finish_run(run_id.unwrap(), "success", 100, None, None, None)
            .unwrap();
        let third = store.start_run(schedule.id, sq_id, "q").unwrap();
        assert!(third.is_some());
    }

    #[test]
    fn list_runs_paginated() {
        let store = test_store();
        let sq_id = insert_saved_query(&store, 1, "test");
        let schedule = store.create_schedule(sq_id, 1, 300, None).unwrap();

        for i in 0usize..5 {
            let run_id = store.start_run(schedule.id, sq_id, "q").unwrap().unwrap();
            #[allow(clippy::cast_possible_truncation)]
            store
                .finish_run(run_id, "success", (i as u64) * 100, Some(i), None, None)
                .unwrap();
        }

        let all = store.list_runs(sq_id, 1, 100, 0).unwrap();
        assert_eq!(all.len(), 5);

        let page = store.list_runs(sq_id, 1, 2, 1).unwrap();
        assert_eq!(page.len(), 2);
    }

    #[test]
    fn list_runs_user_isolation() {
        let store = test_store();
        let sq_id = insert_saved_query(&store, 1, "test");
        let schedule = store.create_schedule(sq_id, 1, 300, None).unwrap();

        let run_id = store.start_run(schedule.id, sq_id, "q").unwrap().unwrap();
        store
            .finish_run(run_id, "success", 100, None, None, None)
            .unwrap();

        // User 2 should see no runs.
        let runs = store.list_runs(sq_id, 2, 100, 0).unwrap();
        assert!(runs.is_empty());
    }

    #[test]
    fn cleanup_stale_runs() {
        let store = test_store();
        let sq_id = insert_saved_query(&store, 1, "test");
        let schedule = store.create_schedule(sq_id, 1, 300, None).unwrap();

        // Start a run but don't finish it (simulates crash).
        store.start_run(schedule.id, sq_id, "q").unwrap();

        let cleaned = store.cleanup_stale_runs().unwrap();
        assert_eq!(cleaned, 1);

        // Run should now be marked as error.
        let runs = store.list_runs(sq_id, 1, 100, 0).unwrap();
        assert_eq!(runs[0].status, "error");
        assert_eq!(
            runs[0].error_message.as_deref(),
            Some("interrupted by server restart")
        );
    }

    #[test]
    fn count_runs() {
        let store = test_store();
        let sq_id = insert_saved_query(&store, 1, "test");
        let schedule = store.create_schedule(sq_id, 1, 300, None).unwrap();

        assert_eq!(store.count_runs(schedule.id).unwrap(), 0);

        let run_id = store.start_run(schedule.id, sq_id, "q").unwrap().unwrap();
        store
            .finish_run(run_id, "success", 100, None, None, None)
            .unwrap();

        assert_eq!(store.count_runs(schedule.id).unwrap(), 1);
    }

    #[test]
    fn latest_run() {
        let store = test_store();
        let sq_id = insert_saved_query(&store, 1, "test");
        let schedule = store.create_schedule(sq_id, 1, 300, None).unwrap();

        assert!(store.latest_run(schedule.id).unwrap().is_none());

        let run_id = store.start_run(schedule.id, sq_id, "q1").unwrap().unwrap();
        store
            .finish_run(run_id, "success", 100, None, None, None)
            .unwrap();

        let run_id = store.start_run(schedule.id, sq_id, "q2").unwrap().unwrap();
        store
            .finish_run(run_id, "error", 50, None, Some("boom"), None)
            .unwrap();

        let latest = store.latest_run(schedule.id).unwrap().unwrap();
        assert_eq!(latest.status, "error");
        assert_eq!(latest.query, "q2");
    }

    #[test]
    fn list_enabled_schedules() {
        let store = test_store();
        let sq1 = insert_saved_query(&store, 1, "enabled query");
        let sq2 = insert_saved_query(&store, 2, "disabled query");

        store.create_schedule(sq1, 1, 300, None).unwrap();
        let disabled = store.create_schedule(sq2, 2, 600, None).unwrap();
        store
            .update_schedule(disabled.id, 2, 600, None, false)
            .unwrap();

        let enabled = store.list_enabled_schedules().unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].1.name, "enabled query");
    }

    #[test]
    fn count_runs_for_saved_query() {
        let store = test_store();
        let sq_id = insert_saved_query(&store, 1, "test");
        let schedule = store.create_schedule(sq_id, 1, 300, None).unwrap();

        assert_eq!(store.count_runs_for_saved_query(sq_id, 1).unwrap(), 0);

        let run_id = store.start_run(schedule.id, sq_id, "q").unwrap().unwrap();
        store
            .finish_run(run_id, "success", 100, None, None, None)
            .unwrap();

        assert_eq!(store.count_runs_for_saved_query(sq_id, 1).unwrap(), 1);
        // Wrong user sees 0.
        assert_eq!(store.count_runs_for_saved_query(sq_id, 2).unwrap(), 0);
    }
}
