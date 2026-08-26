// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The postgres admission budget, checked at compile-and-run time.
//!
//! `.config/nextest.toml` bounds every pg-touching binary with one group
//! width. That width, the per-test connection ceiling in `common`, and the
//! CI server's `max_connections` are three numbers that must keep agreeing;
//! nothing else notices when one of them moves. This test is the arithmetic
//! written down once.
//!
//! No tokio, no postgres, no fixture server: it reads two consts and a TOML
//! line. It lives in the `postgres` nextest group anyway, because it shares
//! the fixture module and the admission guard derives membership from that.

mod common;

/// The config is read with `include_str!`, not `fs::read_to_string`, on
/// purpose: if the file moves or this relative path rots, the build breaks
/// loudly instead of the test passing over an empty string.
const NEXTEST_CONFIG: &str = include_str!("../../../.config/nextest.toml");

/// Connections that are NOT charged to a test's own ceiling: the sweeper's
/// admin session, transient `dropdb`/`createdb` connections, and the
/// store-only suites' `#[sqlx::test]` pools. Slack, deliberately generous.
const HEADROOM: u32 = 20;

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
    let worst_case = common::PER_TEST_CONNECTION_CEILING * max_threads + HEADROOM;
    assert!(
        worst_case <= common::CI_MAX_CONNECTIONS,
        "postgres group is oversubscribed: PER_TEST_CONNECTION_CEILING ({}) \
         * max-threads ({max_threads}) + headroom ({HEADROOM}) = {worst_case} \
         > CI_MAX_CONNECTIONS ({}). Lower max-threads in .config/nextest.toml \
         or shrink the fixture pools.",
        common::PER_TEST_CONNECTION_CEILING,
        common::CI_MAX_CONNECTIONS,
    );
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
