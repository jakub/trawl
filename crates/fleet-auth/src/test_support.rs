// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared Postgres test fixture for fleet-auth consumers (`test-support`
//! feature).
//!
//! Lives in the library (rather than `tests/common/`) so sibling crates that
//! integrate against the keystore — e.g. trawl-server's pg-backed router
//! tests — can reuse the fixture instead of copy-pasting it. fleet-auth's
//! own integration tests re-export this module via `tests/common/mod.rs`.
//!
//! Requires a reachable Postgres at `$FLEET_DATABASE_URL` (or `$DATABASE_URL`
//! as a fallback) with `CREATEDB` privilege. Each test gets a fresh
//! ephemeral database — created with the schema migration applied, used,
//! then dropped on `Drop`. Skipped (with a clear message) when no DB URL is
//! set, unless `FLEET_TESTS_REQUIRED=1` flips skip into hard-fail (CI
//! invariant — protects against silent green no-ops).
//!
//! We hand-roll the fixture instead of using `#[sqlx::test]` because
//! `sqlx-macros` transitively triggers a cargo links-uniqueness conflict
//! with the workspace's rusqlite (see `migrations.rs` for context).

use sqlx::Executor as _;
use sqlx::postgres::{PgConnectOptions, PgConnection, PgPool, PgPoolOptions};

use crate::MIGRATOR;

/// Reads the base database URL from the environment.
///
/// Empty values are treated as unset — important so a CI secret that fails
/// to inject (lands as empty string) doesn't silently become "no DB", which
/// in turn would make `FLEET_TESTS_REQUIRED=1` mis-fire.
#[must_use]
pub fn base_database_url() -> Option<String> {
    for var in ["FLEET_DATABASE_URL", "DATABASE_URL"] {
        if let Ok(v) = std::env::var(var)
            && !v.is_empty()
        {
            return Some(v);
        }
    }
    None
}

/// If set, missing/unreachable Postgres is a hard failure instead of a skip.
#[must_use]
pub fn require_database() -> bool {
    std::env::var("FLEET_TESTS_REQUIRED").is_ok_and(|v| !v.is_empty())
}

/// RAII fixture: create + migrate a fresh per-test database, hand out a
/// pool, drop the database on `Drop`.
pub struct PgFixture {
    admin_url: String,
    admin_opts: PgConnectOptions,
    test_db: String,
    pool: Option<PgPool>,
}

impl std::fmt::Debug for PgFixture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgFixture")
            .field("test_db", &self.test_db)
            .finish_non_exhaustive()
    }
}

impl PgFixture {
    /// Create + migrate a fresh per-test database.
    ///
    /// Returns `None` when no database URL is configured (callers decide
    /// between skip and hard-fail via [`require_database`]).
    ///
    /// # Panics
    /// Panics when the database URL is malformed, unreachable, the role
    /// lacks `CREATEDB`, or migrations fail — all environment defects that
    /// should fail the test loudly rather than skip.
    pub async fn setup() -> Option<Self> {
        use sqlx::Connection as _;

        let admin_url = base_database_url()?;
        let admin_opts: PgConnectOptions = admin_url
            .parse()
            .expect("FLEET_DATABASE_URL is not a valid Postgres URL");
        let test_db = format!("fleet_auth_test_{}", random_db_suffix());

        let mut admin: PgConnection = PgConnection::connect_with(&admin_opts)
            .await
            .expect("connect to admin DB (requires reachable Postgres + CREATEDB)");

        admin
            .execute(format!(r#"CREATE DATABASE "{test_db}""#).as_str())
            .await
            .expect("CREATE DATABASE — does the role have CREATEDB?");

        let test_opts = admin_opts.clone().database(&test_db);
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(test_opts)
            .await
            .expect("connect to per-test DB");

        MIGRATOR.run(&pool).await.expect("apply migrations");

        Some(Self {
            admin_url,
            admin_opts,
            test_db,
            pool: Some(pool),
        })
    }

    /// URL of the ephemeral per-test database, for consumers that construct
    /// their own connection from configuration (e.g. trawld's
    /// `[auth] database_url` boot path).
    #[must_use]
    pub fn database_url(&self) -> String {
        swap_database(&self.admin_url, &self.test_db)
    }

    /// A pool connected to the ephemeral per-test database.
    ///
    /// # Panics
    /// Panics if called after the fixture's pool has been torn down.
    #[must_use]
    pub fn pool(&self) -> PgPool {
        self.pool.as_ref().expect("fixture live").clone()
    }

    /// Forcibly drop the ephemeral database NOW, terminating every live
    /// connection — simulates the auth backend dying under a running
    /// consumer (e.g. trawld's pg-down → 503 contract tests). The fixture's
    /// `Drop` re-drops harmlessly (`IF EXISTS`).
    ///
    /// # Panics
    /// Panics when the admin connection or the drop itself fails.
    pub async fn kill_database(&self) {
        use sqlx::Connection as _;
        let mut admin = PgConnection::connect_with(&self.admin_opts)
            .await
            .expect("connect to admin DB");
        admin
            .execute(format!(r#"DROP DATABASE IF EXISTS "{}" WITH (FORCE)"#, self.test_db).as_str())
            .await
            .expect("force-drop test database");
    }
}

impl Drop for PgFixture {
    fn drop(&mut self) {
        // Teardown on a dedicated OS thread so we never start a nested
        // runtime inside the test's own tokio runtime. `DROP DATABASE ...
        // WITH (FORCE)` terminates any lingering connections from the per-
        // test pool (its own Drop closes them lazily).
        self.pool.take(); // release our handle so WITH (FORCE) can reclaim
        let admin_opts = self.admin_opts.clone();
        let test_db = self.test_db.clone();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("teardown runtime");
            rt.block_on(async move {
                use sqlx::Connection as _;
                if let Ok(mut admin) = PgConnection::connect_with(&admin_opts).await {
                    let _ = admin
                        .execute(
                            format!(r#"DROP DATABASE IF EXISTS "{test_db}" WITH (FORCE)"#).as_str(),
                        )
                        .await;
                }
            });
        });
        let _ = handle.join();
    }
}

/// Replace the database path of a postgres URL, preserving any query string.
fn swap_database(url: &str, db: &str) -> String {
    let (base, query) = url
        .split_once('?')
        .map_or((url, None), |(b, q)| (b, Some(q)));
    let after_scheme = base.find("://").map_or(0, |i| i + 3);
    let authority = base[after_scheme..]
        .find('/')
        .map_or(base, |i| &base[..after_scheme + i]);
    match query {
        Some(q) => format!("{authority}/{db}?{q}"),
        None => format!("{authority}/{db}"),
    }
}

fn random_db_suffix() -> String {
    use rand::Rng as _;
    let mut rng = rand::thread_rng();
    (0..12)
        .map(|_| {
            let n: u8 = rng.gen_range(0..26);
            (b'a' + n) as char
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::swap_database;

    #[test]
    fn swap_database_replaces_path_and_keeps_query() {
        assert_eq!(
            swap_database("postgres://u:p@h:5432/olddb", "newdb"),
            "postgres://u:p@h:5432/newdb"
        );
        assert_eq!(
            swap_database("postgres://u:p@h:5432/olddb?sslmode=require", "newdb"),
            "postgres://u:p@h:5432/newdb?sslmode=require"
        );
        assert_eq!(swap_database("postgres://h", "newdb"), "postgres://h/newdb");
    }
}
