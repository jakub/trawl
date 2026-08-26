// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The postgres admission budget, checked at compile-and-run time.
//!
//! `.config/nextest.toml` bounds every pg-touching binary with one group
//! width. That width, the per-shape connection ceilings in `common`, and
//! the CI server's `max_connections` are numbers that must keep agreeing;
//! nothing else notices when one of them moves. This test is the
//! arithmetic written down once.
//!
//! It multiplies the WIDEST shape, not an average and not the full-server
//! one: the group admits whatever mix of shapes nextest happens to
//! schedule, so the only safe assumption is that every slot holds the
//! widest.
//!
//! No tokio, no postgres, no fixture server: it reads a handful of consts
//! and a TOML line. It lives in the `postgres` nextest group anyway, because it shares
//! the fixture module and the admission guard derives membership from that.

mod common;

/// The config is read with `include_str!`, not `fs::read_to_string`, on
/// purpose: if the file moves or this relative path rots, the build breaks
/// loudly instead of the test passing over an empty string.
const NEXTEST_CONFIG: &str = include_str!("../../../.config/nextest.toml");

/// Connections the run needs that no single test shape holds.
///
/// Three named parts, none of them slack for its own sake:
///
/// * [`SUPERUSER_RESERVED`] is postgres' own `superuser_reserved_connections`,
///   which is subtracted from `max_connections` for ordinary roles. CI
///   connects as the superuser and would not pay it; a developer running
///   the same suite against the dev cluster as `fleet` does.
/// * `ADMIN_TRANSIENT * max-threads` covers the mint/sweep/kill sessions.
///   They live for one statement, so charging them to every shape would
///   double count, but with the group full each concurrent test may hold
///   one.
/// * [`SQLX_MASTER_CHURN`] covers sqlx's process-global master pool, which
///   creates and drops the per-test database. It closes each connection on
///   release and only runs during setup and teardown, when the test's own
///   shape is not yet at its peak.
fn headroom(max_threads: u32) -> u32 {
    /// `superuser_reserved_connections`, the postgres default.
    const SUPERUSER_RESERVED: u32 = 3;
    /// Concurrent setup/teardown windows in sqlx's master pool.
    const SQLX_MASTER_CHURN: u32 = 4;

    SUPERUSER_RESERVED + common::ADMIN_TRANSIENT * max_threads + SQLX_MASTER_CHURN
}

/// `max-threads` of the `[test-groups.postgres]` table.
fn postgres_group_max_threads() -> u32 {
    let mut in_group = false;
    for line in NEXTEST_CONFIG.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == "[test-groups.postgres]";
            continue;
        }
        if !in_group {
            continue;
        }
        if let Some(value) = line.strip_prefix("max-threads") {
            let value = value.trim_start().strip_prefix('=').expect("max-threads =");
            return value.trim().parse().expect("max-threads is an integer");
        }
    }
    panic!("no [test-groups.postgres] max-threads in .config/nextest.toml");
}

#[test]
fn postgres_group_width_fits_the_connection_budget() {
    let max_threads = postgres_group_max_threads();
    let headroom = headroom(max_threads);
    let worst_case = common::WORST_TEST_CONNECTION_CEILING * max_threads + headroom;
    assert!(
        worst_case <= common::CI_MAX_CONNECTIONS,
        "postgres group is oversubscribed: WORST_TEST_CONNECTION_CEILING ({}) \
         * max-threads ({max_threads}) + headroom ({headroom}) = {worst_case} \
         > CI_MAX_CONNECTIONS ({}). Shape ceilings: full server {}, \
         full server + direct store pool {}, sqlx store test {}, boot {}. \
         Lower max-threads in .config/nextest.toml, or shrink whichever \
         shape is widest.",
        common::WORST_TEST_CONNECTION_CEILING,
        common::CI_MAX_CONNECTIONS,
        common::FULL_SERVER_CONNECTION_CEILING,
        common::DIRECT_STORE_CONNECTION_CEILING,
        common::SQLX_STORE_CONNECTION_CEILING,
        common::BOOT_CONNECTION_CEILING,
    );
}

#[test]
fn the_worst_shape_is_the_widest_shape() {
    // `WORST_TEST_CONNECTION_CEILING` is a const `max` over the shapes; a
    // new shape added to `common` without being folded into it would leave
    // the budget quietly under-counting.
    for (name, ceiling) in [
        ("full server", common::FULL_SERVER_CONNECTION_CEILING),
        ("direct store", common::DIRECT_STORE_CONNECTION_CEILING),
        ("sqlx store test", common::SQLX_STORE_CONNECTION_CEILING),
        ("boot", common::BOOT_CONNECTION_CEILING),
    ] {
        assert!(
            ceiling <= common::WORST_TEST_CONNECTION_CEILING,
            "the {name} shape ({ceiling}) is wider than \
             WORST_TEST_CONNECTION_CEILING ({})",
            common::WORST_TEST_CONNECTION_CEILING,
        );
    }
}

#[test]
fn the_group_is_wide_enough_to_be_worth_having() {
    // A width of 1 would serialize the whole pg suite; that is a config
    // typo, not a budget. Catches a fat-fingered `max-threads = 1`.
    assert!(
        postgres_group_max_threads() >= 4,
        "postgres group is too narrow"
    );
}
