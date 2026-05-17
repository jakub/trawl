// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared Postgres test fixture for fleet-auth integration tests.
//!
//! Each integration test file (`tests/store_pg.rs`, `tests/middleware_pg.rs`,
//! `tests/handlers_pg.rs`) declares `mod common;` to pull in this module.
//! Cargo's per-target compilation discovers it via the directory layout, so
//! no `[[test]]` registration is needed.
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

#![allow(dead_code)] // each test binary uses a subset of these items

use fleet_auth::MIGRATOR;
use sqlx_core::executor::Executor as _;
use sqlx_postgres::{PgConnectOptions, PgConnection, PgPool, PgPoolOptions};

/// Reads the base database URL from the environment.
///
/// Empty values are treated as unset — important so a CI secret that fails
/// to inject (lands as empty string) doesn't silently become "no DB", which
/// in turn would make `FLEET_TESTS_REQUIRED=1` mis-fire.
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
pub fn require_database() -> bool {
    std::env::var("FLEET_TESTS_REQUIRED").is_ok_and(|v| !v.is_empty())
}

/// RAII fixture: create + migrate a fresh per-test database, hand out a
/// pool, drop the database on `Drop`.
pub struct PgFixture {
    admin_opts: PgConnectOptions,
    test_db: String,
    pool: Option<PgPool>,
}

impl PgFixture {
    pub async fn setup() -> Option<Self> {
        use sqlx_core::connection::Connection as _;

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
            admin_opts,
            test_db,
            pool: Some(pool),
        })
    }

    pub fn pool(&self) -> PgPool {
        self.pool.as_ref().expect("fixture live").clone()
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
                use sqlx_core::connection::Connection as _;
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

/// Skip-or-fail wrapper for an integration test that needs Postgres.
///
/// Receives a `KeyStore` derived from a fresh per-test database. The body
/// is a closure so the macro stays callsite-friendly even when the test
/// needs to capture more state via `move`.
#[macro_export]
macro_rules! pg_test {
    ($name:ident, $body:expr) => {
        #[tokio::test]
        async fn $name() {
            let Some(fx) = $crate::common::PgFixture::setup().await else {
                let msg = format!(
                    "fleet-auth integration test '{}' skipped: FLEET_DATABASE_URL not set or empty",
                    stringify!($name)
                );
                if $crate::common::require_database() {
                    panic!("{msg} — but FLEET_TESTS_REQUIRED is set, so this is a hard failure",);
                }
                eprintln!("{msg}");
                return;
            };
            let pool = fx.pool();
            let store = fleet_auth::KeyStore::from_pool(pool);
            #[allow(clippy::redundant_closure_call)]
            ($body)(store).await;
            drop(fx);
        }
    };
}
