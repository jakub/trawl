//! Shared test utilities for fleet-engine integration tests.
//!
//! Generates deterministic fixtures (parquet and ndjson) using `DuckDB`
//! itself, so we don't need arrow/parquet crates or external tooling.
//!
//! Handles nextest's parallel process model: each test runs in its own
//! process, so we use filesystem-level coordination (existence check +
//! unique temp files) instead of in-process synchronization.

use std::path::{Path, PathBuf};

use duckdb::Connection;

/// Return the glob path for test parquet files, generating fixtures if needed.
pub fn fixture_glob() -> String {
    let dir = fixtures_dir();
    ensure_fixture(&dir, "logs.parquet", "PARQUET");
    format!("{}/**/*.parquet", dir.display())
}

/// Return the glob path for test ndjson files, generating fixtures if needed.
pub fn fixture_glob_json() -> String {
    let dir = fixtures_dir();
    ensure_fixture(&dir, "logs.ndjson", "JSON");
    format!("{}/**/*.ndjson", dir.display())
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("parquet")
}

/// Generate a fixture file if it doesn't already exist.
///
/// Uses a process-unique temp file to avoid lock conflicts when
/// multiple nextest processes run concurrently.
fn ensure_fixture(dir: &Path, filename: &str, format: &str) {
    let final_path = dir.join(filename);

    if final_path.exists() {
        return;
    }

    std::fs::create_dir_all(dir).expect("failed to create fixture directory");

    let tmp_path = dir.join(format!("{}_{}.tmp", filename, std::process::id()));

    generate_to(&tmp_path, format).expect("failed to generate test fixtures");

    if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
        let _ = std::fs::remove_file(&tmp_path);
        assert!(final_path.exists(), "failed to place fixture file: {e}");
    }
}

fn generate_to(path: &Path, format: &str) -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::open_in_memory()?;

    conn.execute_batch(
        "CREATE TABLE logs (
            timestamp TIMESTAMP,
            host VARCHAR,
            service VARCHAR,
            level VARCHAR,
            status INTEGER,
            duration DOUBLE,
            uri VARCHAR,
            method VARCHAR,
            message VARCHAR,
            user_name VARCHAR,
            src_ip VARCHAR
        )",
    )?;

    // deterministic test data: 13 rows across 3 services
    conn.execute_batch(
        "INSERT INTO logs VALUES
        -- nginx access logs (6 rows, 2 hosts)
        ('2024-01-15 10:00:00', 'web01', 'nginx', 'info',  200, 0.045, '/api/users',  'GET',  'GET /api/users 200 0.045s', NULL, '192.168.1.10'),
        ('2024-01-15 10:00:01', 'web01', 'nginx', 'info',  200, 0.023, '/api/health', 'GET',  'GET /api/health 200 0.023s', NULL, '192.168.1.11'),
        ('2024-01-15 10:00:02', 'web02', 'nginx', 'error', 500, 1.234, '/api/users',  'POST', 'POST /api/users 500 1.234s internal server error', NULL, '192.168.1.12'),
        ('2024-01-15 10:00:03', 'web01', 'nginx', 'warn',  404, 0.001, '/missing',    'GET',  'GET /missing 404 0.001s not found', NULL, '192.168.1.10'),
        ('2024-01-15 10:00:04', 'web02', 'nginx', 'info',  301, 0.002, '/old-path',   'GET',  'GET /old-path 301 0.002s redirect', NULL, '192.168.1.13'),
        ('2024-01-15 10:00:05', 'web01', 'nginx', 'error', 502, 5.000, '/api/slow',   'GET',  'GET /api/slow 502 5.000s bad gateway', NULL, '192.168.1.14'),
        -- sshd auth logs (4 rows)
        ('2024-01-15 10:01:00', 'bastion', 'sshd', 'info',  NULL, NULL, NULL, NULL, 'Accepted publickey for admin from 10.0.0.5 port 22', 'admin', '10.0.0.5'),
        ('2024-01-15 10:01:01', 'bastion', 'sshd', 'warn',  NULL, NULL, NULL, NULL, 'Failed password for root from 10.0.0.99 port 22', 'root', '10.0.0.99'),
        ('2024-01-15 10:01:02', 'bastion', 'sshd', 'warn',  NULL, NULL, NULL, NULL, 'Failed password for root from 10.0.0.99 port 22', 'root', '10.0.0.99'),
        ('2024-01-15 10:01:03', 'bastion', 'sshd', 'error', NULL, NULL, NULL, NULL, 'Connection refused from 10.0.0.99', NULL, '10.0.0.99'),
        -- system logs (3 rows)
        ('2024-01-15 10:02:00', 'db01',  'systemd', 'info', NULL, NULL, NULL, NULL, 'Started PostgreSQL database server', NULL, NULL),
        ('2024-01-15 10:02:01', 'db01',  'kernel',  'warn', NULL, NULL, NULL, NULL, 'Out of memory: Killed process 1234 (java)', NULL, NULL),
        ('2024-01-15 10:02:02', 'web01', 'systemd', 'info', NULL, NULL, NULL, NULL, 'nginx.service: Started NGINX web server', NULL, NULL)",
    )?;

    conn.execute_batch(&format!(
        "COPY logs TO '{}' (FORMAT {format})",
        path.display()
    ))?;

    Ok(())
}
