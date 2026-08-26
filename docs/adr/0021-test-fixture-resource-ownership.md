# Test fixtures own their resources: ports, databases, pools, shared state — and admission is static

status: accepted (2026-08-25) — prep ruling record for #125

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
