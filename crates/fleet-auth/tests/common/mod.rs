// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared Postgres test fixture for fleet-auth integration tests.
//!
//! The fixture itself lives in the library as `fleet_auth::test_support`
//! (behind the `test-support` feature) so sibling crates — e.g.
//! trawl-server's pg-backed router tests — can reuse it. This module
//! re-exports it and carries the `pg_test!` macro. Running fleet-auth's
//! own integration tests therefore requires `--features test-support`
//! (a missing feature is a loud compile error, never a silent skip).

#![allow(dead_code)] // each test binary uses a subset of these items
#![allow(unused_imports)] // ditto for the re-exports below

pub use fleet_auth::test_support::{PgFixture, base_database_url, require_database};

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
