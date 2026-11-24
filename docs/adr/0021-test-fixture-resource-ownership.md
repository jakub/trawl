# Test fixtures own their resources: ports, databases, pools, shared state — and admission is static

status: accepted (2026-08-25) — prep ruling record for #125; amended (2026-11-23): rulings moved from code comments, see the Amendment

The postgres-backed suites flaked under parallel load in five distinct
ways (#125): a TOCTOU'd ephemeral port, a `55006` teardown race on the
sqlx-owned fleet database, connection-pool starvation that disguised
itself as value assertions, a non-atomic shared TLS-pair publish, and a
shared `scheduled/` subtree deleted by sibling setups. All five are one
defect: a resource whose lifetime or publication nobody owns. This ADR
fixes the ownership model; the repin evidence half of #125 is ADR-0022.

## Rulings

1. **A port is owned from bind to serve.** `free_port()` (bind
   `127.0.0.1:0`, drop, return the number) is deleted. The fixture binds
   once, holds the listener, reads the address from it, and hands the
   listener to the transport through a supplied-listener entry point.
   The production daemon path is unchanged — it keeps building TLS
   before its own bind, and no config surface changes. Retry-on-
   `EADDRINUSE` is rejected: the fixture publishes the URL to the client
   before the server binds, so a retried port is a stale URL, and a
   rarer race is not a fixed race.

2. **A test database is minted by whoever must outlive it.**
   `#[sqlx::test]`'s teardown closes the pool under a 10-second timeout
   and then drops the database REGARDLESS (sqlx-core 0.8 `testing`),
   so handing its database to a server whose detached tasks hold
   connections is unsound no matter how the pool is shared. Full-server
   fixtures therefore mint their own fleet database exactly as they
   already mint app databases — `{prefix}_{run_marker}_{pid}_{rand}`
   naming, advisory-lock-gated dead-pid sweeper, `fleet_auth::MIGRATOR`
   run by the fixture — and `#[sqlx::test]` survives only in suites
   where the handed pool is the sole connection holder (store_pg,
   auth-store level, fleet-auth/fleet-admin).

3. **Servers under test receive pools; only production connects.**
   `AppState` construction is split from connection: the fixture builds
   the fleet keystore via `KeyStore::from_pool` and the app store from a
   pool it owns and sizes (2–3 connections each), and passes them in.
   Production keeps the connecting constructor. The eager-connect path
   (`KeyStore::connect`: DSN parse, unreachable host, connect+ping)
   gets one focused fleet-auth test — which is more coverage than the
   twenty incidental exercisers it replaces, at two connections instead
   of a hundred and sixty. Tests may not open raw `PgPool`s outside the
   fixture helpers: the pool constructors go into clippy
   `disallowed_methods` for test code, the same guard shape as
   `Parser::padded` (ADR-0014).

4. **Postgres admission is static, property-derived, and singular.**
   One nextest test group covers every postgres-touching binary
   wholesale, sized by the worst-case formula (per-test ceiling ×
   group width + headroom ≤ CI's `max_connections = 100`). A CI guard
   derives the member set from source properties — `#[sqlx::test]`
   usage and fixture imports — and fails when a pg-touching binary is
   not in the resolved group, because the previous hand-list silently
   omitted three of the heaviest binaries and mis-weighted a fourth.
   The runtime alternative (weighted permits held as pg session
   advisory locks) is **rejected, recorded**: a permit acquired in
   fixture code is too late to gate the pool `#[sqlx::test]` opens
   before the test body runs, and it duplicates nextest's scheduler
   with new cross-process machinery. Two throttles with different cost
   models is how the starvation went undiagnosed.

5. **Shared fixture state is immutable and published atomically.** A
   cross-process artifact (the parquet seed tree, the TLS pair) is
   staged as a complete directory and published with one rename;
   half-published state must be impossible, and a setup path never
   deletes shared state. Anything a test writes — including
   `scheduled/` output — lives under a per-test private root. The
   current TLS publish (two independent renames behind an exists-check
   on one file) and the shared-tree `scheduled/` wipe are both
   instances of the same violation.

6. **No blanket retries, still.** The two existing named single-test
   retries in `.config/nextest.toml` stay, with their inline
   justifications; nothing new gains one. A flake fixed by retry is a
   flake shipped.

7. **The harness diagnoses its own failures.** On panic/failure a
   fixture dumps its facts: test identity, database names, port, pool
   ceilings, and a grouped `pg_stat_activity` snapshot — redacted (no
   DSNs, no tokens). The #125 triage cost an afternoon per
   presentation because each race surfaced as a differently-shaped
   assertion; the next one should cost five minutes.

8. **"Fixed" is a soak number, not a green run.** Acceptance evidence
   for fixture changes is repeated zero-retry full-matrix runs
   (default and no-default-features suites), reported as attempts and
   failures — a finite soak bounds the flake rate, it does not prove
   zero.

## Amendment (2026-11-23): rulings moved from code comments

A comment audit on 2026-11-23 removed these rulings from the code, where they were the only record. They were already in force. None is a new decision.

**Pools and connection ceilings**

- **A pool handed to the app store skips only the connect, never the boot.** `StorageState::from_pool` takes the advisory lock and then migrates. `StorageState::connect` only builds the production pool and calls `from_pool`. Ruling 3's fixtures therefore still run the production boot sequence, and a change that moved the lock or the migration into `connect` would leave both untested.
- **A test shape is charged every pool it holds at its `max_connections`, plus every raw connection it keeps open.** The `*_CONNECTION_CEILING` constants in `crates/trawl-server/tests/common/mod.rs` follow this rule, and the worst of them sizes the group against `CI_MAX_CONNECTIONS` in `crates/trawl-server/tests/connection_budget.rs`. Ruling 4's formula is a worst case, so a shape is charged the connections it can hold, not the connections it usually uses.
- **The `postgres` test group has eight thread slots, and a `startup_pg` test takes two of them.** A `startup_pg` test holds 20 connections, twice `WORST_TEST_CONNECTION_CEILING`, so it costs two slots. Postgres connections limit the width: at nine slots, `postgres_group_width_fits_the_connection_budget` needs 106 connections against `CI_MAX_CONNECTIONS` of 100 and fails. Raise `max-threads` in `.config/nextest.toml` only after the `pg soak` workflow stays green at the new width, as ruling 8 requires for any fixture change.

**Failure diagnostics**

- **Ruling 7's dump prints names, numbers, and grouped counts, and nothing that identifies a connection.** `FixtureFacts` holds database names, the port, and pool ceilings, with no DSN or token. Its fields are private, so a test cannot put a DSN in a printed field. The snapshot connects with the admin DSN from `DATABASE_URL` at print time and never prints it. The `pg_stat_activity` census groups by `datname`, `backend_type`, `state`, and the wait event, and leaves out `usename`, `client_addr`, `application_name`, and `query`. SQL text and connection identities in CI logs are a leak, and the grouped counts alone tell pool exhaustion apart from many sessions that wait on one lock.

**Shared seed state**

- **A change to the parquet seed data bumps the version in its directory name (`parquet-v1`).** A tree from an older checkout is then a directory that no test reads. Ruling 5 forbids a setup path to delete shared state, and without the version a test would have to detect a stale tree and delete it.
- **`seed_data_root` copies the seed into the private data root and does not hardlink it.** The test's tempdir is on `/tmp`, which is a tmpfs on the dev machine, so a hard link from the repository's filesystem fails with `EXDEV`.

**The admission guard (`xtask/src/pg_admission.rs`)**

- **The guard derives the member set and asks nextest for the group membership, as two independent sides.** Derivation reads `cargo metadata` and makes one lexer pass over each target's sources. A target is pg-touching when it has an `#[sqlx::test]` attribute, when it pulls in a `common` module that defines `fixture_pool`, or when its test code names a connection-opening API such as `KeyStore::connect` or `PgPool`. The third class catches a plain `#[tokio::test]` that connects by hand. In a `src/` tree the pass reads only test regions: the body of a `#[cfg(test)]` item and the body of a fn with a test attribute. Membership comes from `cargo nextest list -E 'group(postgres)'`, so the engine that enforces the group also answers the question. Parsing the override filters out of `.config/nextest.toml` is rejected, because it would be a second implementation of nextest's filterset semantics that could disagree with the first.
- **Correction to ruling 4: a binary is admitted whole only when it has target-level evidence.** A fixture module or a connection API is target-level evidence, and it makes every non-ignored test of the binary need `filter-match.status == "matches"`. When every finding for a target names tests, the guard checks exactly those names. `trawl-server::auth_pg` has both kinds of evidence: 12 of its 36 tests are `#[sqlx::test]`, and it imports the `common` fixture module. A check of its named tests alone would let a narrowed filter admit those 12 while the other 24 boot a server outside the group. Name matching stays for targets with only named tests, so `.config/nextest.toml` admits `from_saved::`, `scheduler::`, and the other listed Postgres modules from trawl-server's library target, and the pure unit tests run outside the group.
- **The guard is a CI tripwire for an accidental escape, not a security boundary.** The threat is an author who adds a test that opens a Postgres connection and forgets to extend the group. Anyone who edits the workspace can also edit `.config/nextest.toml`. Every judgement call fails in one direction. When it is unclear whether something is a pg test, the guard collects it. When it is unclear which target owns a file, the guard charges the file to both. A silent false green is the only defect class, and a false failure is noise.
- **Lexer gaps are accepted when they fail loudly or over-collect.** Unresolved `cfg` and `cfg_attr`, a nested `#[cfg(test)]` that yields two spans, a bodyless item that spans its own extent, a non-body brace at item depth, `# [test]` spacing (which `cargo fmt` rejects), and raw keywords all fall in this class. Eight shapes can hide a connection or misname a test, and they stay open because nobody here writes them by accident: an `include!` edge, a non-ASCII identifier, a `cfg_attr` path override on a `mod`, a `#[cfg(test)]` macro invocation, `#[::sqlx::test]`, an attributed `if` statement, a `>>` at angle depth 1 in `skip_to_body`, and a comment inside a `#[path]` value. Judge a new gap by one question: does it give a silent pass, or only noise?
- **The module walk decides module paths, and the sweep of leftover files is only a safety net.** `unit_files` adds each `.rs` file under the target's sweep root that `module_tree` did not reach, and `path_prefix` names it from its path. `derive` assigns the sweep roots. The library sweeps `src/` without `src/bin/`. A bin at `src/bin/<stem>.rs` sweeps `src/bin/<stem>/`. A bin at `src/main.rs` sweeps all of `src/` when the package has no library, and has no sweep when it does. A file that no `mod` reaches does not compile in, so including it can cost only a loud false failure. Leaving a file out could cost a silent escape. Integration targets under `tests/` have no sweep.

**Retries**

- **Browser tests do not retry either.** `crates/trawl-web-ui/e2e/playwright.config.ts` sets `retries: 0`. A flaky browser test is fixed or quarantined in a way that people see, the same as ruling 6 requires for nextest.
