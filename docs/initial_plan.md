# fleet — Implementation Plan

**Version:** 0.1
**Scope:** Complete project from workspace init through agent deployment
**Assumptions:** Solo developer using Claude Code, learning Rust along the way. Ruby/Python background. Homelab is the primary test environment.

---

## Phase 0: Foundation and Learning (Week 1–2)

The goal here isn't shipping features — it's getting comfortable enough with Rust and the toolchain that subsequent phases don't grind to a halt every time the borrow checker has opinions.

### 0.1 Environment Setup

- Install Rust toolchain via `rustup` (stable channel)
- Configure editor/IDE with `rust-analyzer` (this is non-negotiable — the inline type hints and error diagnostics save enormous time)
- Install `cargo-watch` for auto-recompilation on save: `cargo install cargo-watch`, then `cargo watch -x check` in a terminal
- Install `cargo-nextest` as a faster test runner: `cargo install cargo-nextest`
- Set up the workspace structure:

```
fleet/
├── Cargo.toml          # workspace root
├── crates/
│   ├── fleet-core/
│   ├── fleet-engine/
│   ├── fleet-auth/
│   ├── fleet-server/
│   ├── fleet-client/
│   ├── fleet-cli/
│   └── fleet-admin/
├── templates/          # example signed task templates (later)
└── tests/              # integration tests
```

Start with only `fleet-core` having any code. The other crates exist as empty shells with `Cargo.toml` and a stub `lib.rs` so the workspace compiles. This avoids premature dependency decisions.

### 0.2 Rust Familiarization

Work through these in the context of fleet-core, not as abstract exercises:

- Define the AST types as enums and structs. This teaches ownership, derive macros, and pattern matching on real project types.
- Write a few functions that transform AST nodes (e.g., a function that takes a `FieldFilter` and returns a SQL WHERE clause string). This teaches borrowing, string handling, and `Result`.
- Write tests for those functions using `#[test]` and `assert_eq!`. Get comfortable with `cargo test -p fleet-core`.
- Run `cargo clippy` and `cargo fmt` — adopt the habit immediately.

### 0.3 Deliverable

A compiling workspace with AST types defined in `fleet-core`, a handful of unit tests, and enough Rust familiarity to not panic at every compiler error.

---

## Phase 1: The Parser (Week 3–5)

This is the intellectual core of the project. Everything downstream depends on the parser producing a correct AST. Take the time to get this right.

### 1.1 Define the Complete AST

Before writing any parser code, define every AST node type the language needs. This is a design exercise, not a coding one. The AST is the contract between the parser and everything else.

```
QueryAst
├── SearchStage
│   ├── Vec<SearchToken>
│   │   ├── FieldFilter { field, operator, value }
│   │   ├── TextSearch { term, negated }
│   │   ├── TimeFilter { duration }
│   │   └── QuotedSearch { phrase }
│   └── (implicit AND between tokens)
│
└── Vec<PipeStage>
    ├── Stats { aggregations: Vec<AggExpr>, group_by: Vec<Field> }
    ├── Where { expression: Expr }
    ├── Sort { fields: Vec<(Field, Direction)> }
    ├── Limit { n: usize }
    ├── Table { fields: Vec<Field> }
    ├── Drop { fields: Vec<Field> }
    ├── Let { name: String, expression: Expr }
    ├── Extract { pattern: String, source: Option<Field>, captures: Vec<String> }
    ├── Timechart { aggregation: AggExpr, split_by: Option<Field>, span: Option<Duration> }
    ├── Top { n: usize, field: Field, by: Option<Field> }
    ├── Rare { field: Field, by: Option<Field> }
    ├── Dedup { fields: Vec<Field>, consecutive: bool }
    ├── Pivot { field: Field, on: Field }
    ├── FieldSummary
    └── Live
```

The `Expr` type (used in `Where` and `Let`) is its own mini-AST:

```
Expr
├── BinaryOp { left, op, right }    # arithmetic, comparison, boolean
├── UnaryOp { op, operand }         # not, negation
├── FunctionCall { name, args }     # lower(), split(), case()
├── FieldRef { name }               # reference to a data field
├── Literal { value }               # string, number, boolean, null
├── ArrayIndex { expr, index }      # split(host, ".")[0]
└── InList { expr, values }         # host in ("a", "b")
```

Write these as Rust enums and structs. Add `#[derive(Debug, Clone, PartialEq)]` to everything. Write Display implementations for nice error output. This is pure data modeling — no parser code yet.

### 1.2 Build the Parser in Layers

Use `chumsky` (v1.x, the latest). Build bottom-up:

**Layer 1: Primitives.** Parse bare words, quoted strings, numbers, field names, operators. These are the atoms. Test each one independently.

**Layer 2: Search tokens.** Parse `field:value`, `field:>=value`, `last:2h`, negation (`-term`), comma-separated values. Each token type gets its own parser function and its own test file.

**Layer 3: The implicit search stage.** Combine search token parsers into "zero or more tokens separated by whitespace." Test with complete search strings.

**Layer 4: Expressions.** This is the hardest single piece. Use a Pratt parser (chumsky has built-in support via `pratt()`) for operator precedence. Handle: arithmetic (`+`, `-`, `*`, `/`), comparison (`>`, `>=`, `<`, `<=`, `==`, `!=`), boolean (`and`, `or`, `not`), function calls, field references, literals, `in` operator, `matches` operator. Test extensively — expression parsing bugs are subtle and downstream effects are severe.

**Layer 5: Pipe stages.** Each command (`stats`, `where`, `sort`, etc.) gets its own parser. The pipe stage parser is `| command_name followed_by arguments`. Start with `stats`, `where`, `sort`, `limit`, `table` — these cover the majority of use cases. Add the rest in Phase 6.

**Layer 6: Full query.** Combine search stage + zero or more pipe stages. This is the top-level parser.

### 1.3 Error Recovery and Messages

chumsky's error recovery is one of the main reasons to choose it. Invest time in:

- Custom error messages for common mistakes ("did you mean `host`?" when someone types `hots:`)
- Recovery strategies so partial parses still produce useful output (important for autocomplete later)
- Position tracking so errors report the exact character offset

### 1.4 Test Strategy

The parser should have the most tests of any component. Aim for:

- Every search token variant (field filter, text search, time filter, negated, quoted, wildcard, comma-separated)
- Every pipe stage with various argument combinations
- Every expression operator and precedence level
- Error cases: malformed queries, unknown commands, mismatched quotes
- Edge cases: empty queries, queries with only pipes, unicode in values

Use the `insta` crate for snapshot testing — it stores expected AST output as files, making it easy to review and update when the grammar changes.

### 1.5 Deliverable

`fleet-core` parses the core DSL (search + stats/where/sort/limit/table) into a complete AST, with comprehensive tests and good error messages. No SQL generation yet.

---

## Phase 2: SQL Generation (Week 5–7)

### 2.1 The Emitter State Machine

Build the SQL emitter as a struct that accumulates state as it walks the AST:

```
EmitterState {
    source: String,              // the FROM clause (parquet glob path)
    select: Vec<SelectExpr>,     // SELECT columns
    where_clauses: Vec<String>,  // WHERE conditions
    group_by: Vec<String>,       // GROUP BY fields
    having: Vec<String>,         // HAVING conditions
    order_by: Vec<(String, Dir)>,// ORDER BY
    limit: Option<usize>,
    ctes: Vec<CTE>,              // accumulated CTEs
    aggregated: bool,            // has aggregation happened?
    parameters: Vec<Value>,      // parameterized query values
}
```

Each pipe stage is a method that mutates the state. When a stage is incompatible with the current state, call `flush_to_cte()` which snapshots the current state as a CTE and resets.

### 2.2 Expression to SQL Translation

Build a recursive translator from the `Expr` AST to DuckDB SQL strings. This is mostly a mapping table:

- `BinaryOp(left, Add, right)` → `({left_sql} + {right_sql})`
- `FunctionCall("lower", [arg])` → `LOWER({arg_sql})`
- `FunctionCall("case", args)` → `CASE WHEN {cond} THEN {val} ... ELSE {default} END`
- `FunctionCall("p95", [field])` → `PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY {field})`
- `InList(expr, values)` → `{expr_sql} IN ({values})`
- `FieldRef("@timestamp")` → `timestamp` (map system fields)

Maintain a function registry that maps DSL function names to DuckDB SQL. This is extensible — adding a new function is adding an entry to the registry.

### 2.3 SQL Injection Prevention

The emitter MUST use parameterized queries for all user-provided values. Field names are validated against a known schema (allowlist). Operators are validated against a fixed set. This is the security boundary between user input and SQL execution.

### 2.4 Test Strategy

This is where snapshot testing really shines. Every test is a `(DSL input, expected SQL output)` pair:

```rust
#[test]
fn test_simple_filter() {
    let sql = compile("service:nginx level:error last:1h");
    assert_snapshot!(sql);
}

#[test]
fn test_stats_with_where() {
    let sql = compile("service:nginx last:1h | stats count() by host | where count > 10 | sort -count");
    assert_snapshot!(sql);
}
```

Test the CTE flushing logic explicitly: queries that require one CTE, two CTEs, no CTEs. Test the expression translator with nested expressions, all operator types, all functions.

### 2.5 Deliverable

`fleet-core` takes a DSL string and produces valid DuckDB SQL with parameters. No database interaction yet — this is still pure transformation. The full pipeline is: `DSL string → parse → AST → plan → SQL string + params`.

---

## Phase 3: DuckDB Integration and CLI (Week 7–9)

This is where it becomes a real, usable tool.

### 3.1 The Executor (fleet-engine)

Wire up DuckDB via the `duckdb-rs` crate. The executor:

- Opens a DuckDB connection (in-memory mode initially, no persistent database)
- Takes SQL + parameters from the emitter
- Executes via prepared statement
- Streams results as rows
- Handles query cancellation via DuckDB's interrupt API

Start with a synchronous executor. Async comes later when the daemon needs it.

### 3.2 Generate Test Parquet Files

Before you can test real queries, you need data. Write a small utility (can be Python, doesn't need to be Rust) that generates realistic Parquet files:

- Simulated nginx access logs (timestamp, host, service, level, status, duration, uri, method, message)
- Simulated sshd auth logs (timestamp, host, service, level, user, src_ip, message)
- Simulated system logs (timestamp, host, service, level, message)
- Various time ranges (last hour, last day, last week)
- Partitioned by hour into directories matching the Vector output structure

Store these in `tests/fixtures/parquet/`. Use them for integration tests.

### 3.3 The CLI (fleet-cli)

A minimal CLI using `clap` for argument parsing:

```bash
fleet --data "/path/to/parquet/**/*.parquet" "service:nginx last:1h"
fleet --data "..." "service:nginx | stats count() by host" --format json
fleet --data "..." "level:error last:24h" --format csv
fleet --data "..." "service:nginx last:1h" --format table
```

Output format auto-detection: table for TTY, JSON for pipes. The `--data` flag points at the parquet glob. Eventually this will be replaced by connecting to the daemon, but for now the CLI embeds the engine directly.

### 3.4 Integration Tests

End-to-end tests that go from DSL string through parsing, SQL generation, DuckDB execution, and result verification against the fixture Parquet files. These are slower than unit tests but catch integration issues (e.g., generated SQL that parses fine but DuckDB rejects).

### 3.5 Deliverable

A working CLI tool that queries Parquet files with the fleet DSL. You can `cargo run -p fleet-cli -- --data ./tests/fixtures/parquet "service:nginx level:error last:24h | stats count() by host"` and get results. This is the first "show someone and they get it" moment.

---

## Phase 4: Vector Integration (Week 9–10)

### 4.1 Vector Configuration

Write a reference Vector configuration that:

- Accepts syslog over TCP (RFC 5424)
- Tails local log files (configurable paths)
- Reads from journald
- Reads Docker container logs
- Transforms via VRL: parse common formats, normalize field names, add the `@` system fields
- Writes to Parquet files partitioned by `year/month/day/hour`

This is configuration, not code. Provide example `vector.toml` files for common setups (homelab all-in-one, docker host, bare metal server).

### 4.2 Test with Real Logs

Deploy Vector on your homelab, point it at real log sources, verify Parquet output. Then query with the CLI. This is the first real-world validation. You will discover:

- Fields you didn't anticipate in the schema
- Parquet files that are too small (one file per minute is too many) or too large
- VRL transforms that need adjustment for specific log formats
- Edge cases in the parser/emitter (real log data is messier than fixtures)

Iterate on the Vector config and the DSL/emitter based on what you find.

### 4.3 Deliverable

A working pipeline: real services → Vector → Parquet → fleet CLI. You're using fleet to search your own infrastructure's logs.

---

## Phase 5: Authentication (Week 10–12)

### 5.1 The Auth Database (fleet-auth)

SQLite database via `rusqlite`. Tables for API keys, roles, sessions (per the design spec). Implement:

- Key creation (generate random token, bcrypt hash, store)
- Key verification (given a token, find the matching key, check expiry, check active flag)
- Key listing (show prefix, name, role, last used — never the full key)
- Key revocation
- Role checking (given a verified key, does it have permission for this operation?)

### 5.2 The Admin CLI (fleet-admin)

Using `clap` with subcommands:

```
fleet-admin keys create --role analyst --name "web-frontend" --expires 90d
fleet-admin keys list
fleet-admin keys revoke <prefix>
```

At this phase, `fleet-admin` talks directly to the SQLite database (same process). Later, it'll talk to the daemon's API.

### 5.3 Deliverable

`fleet-auth` crate with complete key lifecycle management. `fleet-admin` CLI for key operations. Auth database schema finalized.

---

## Phase 6: The Daemon (Week 12–16)

This is the largest single phase. It transforms fleet from a CLI tool into a service.

### 6.1 Async Foundation (fleet-server)

Set up the tokio runtime and the core daemon structure:

- Configuration loading from TOML (`config` or `toml` crate)
- Signal handling (SIGTERM for shutdown, SIGHUP for config reload)
- Structured logging via `tracing` + `tracing-subscriber`
- Graceful shutdown (stop accepting connections, wait for in-flight queries, exit)

### 6.2 Unix Socket Listener

The first listener. Accept connections on a Unix socket, read ndjson messages, authenticate via API key, route to the query engine.

Implement the core connection lifecycle:

1. Accept connection
2. Read auth message, verify key
3. Read query messages, execute, stream results
4. Handle disconnect/cleanup

Test with `socat` or a quick script that connects and sends ndjson.

### 6.3 HTTP Listener

Add `axum` for the HTTP API. Implement:

- `POST /api/v1/query` — the main query endpoint
- `GET /api/v1/health` — health check
- `GET /api/v1/schema` — field catalog

Authentication via `Authorization: Bearer <key>` header, implemented as axum middleware.

### 6.4 TCP+TLS Listener

Add the TCP listener with mandatory TLS via `rustls`. Same protocol as Unix socket (ndjson), same auth flow, just over an encrypted TCP connection.

TLS configuration: accept a cert/key pair from config, or auto-generate a self-signed cert on first run. Store the auto-generated cert so it's stable across restarts.

### 6.5 Schema Catalog

The in-memory field catalog. On startup:

1. Scan the Parquet directory for files
2. Read Parquet file footers (schema metadata, row group statistics) via `parquet` crate or via DuckDB's `parquet_metadata()` function
3. Build the catalog: field names, types, cardinality estimates
4. Set up a filesystem watcher (`notify` crate) or periodic refresh timer

Expose via `GET /api/v1/schema` and the ndjson `schema` message type.

### 6.6 Connection and Query Tracking

- `DashMap` for active connections
- `DashMap` for active queries with their metadata
- Query timeout task: periodic sweep that cancels queries exceeding the timeout
- Query history ring buffer

### 6.7 Update the CLI

Rewrite `fleet-cli` to connect to the daemon instead of embedding the engine. Use the `fleet-client` crate for connection management. Fall back to embedded mode with `--data` flag for standalone use.

### 6.8 Deliverable

`fleetd` runs as a daemon, listens on all three transports, authenticates clients, executes queries against Parquet files, streams results. `fleet` CLI connects to the daemon. The system is fully operational for log search.

---

## Phase 7: Extended DSL (Week 16–18)

With the daemon working, add the remaining DSL features incrementally. Each feature follows the same pattern: AST type (already defined) → parser rule → planner logic → emitter logic → tests.

### 7.1 Priority Order

Implement in order of user value:

1. **`let`** — computed fields. Requires the expression translator (already built for `where`). Main new work: CTE flushing when a `let` precedes aggregation.
2. **`extract`** — regex field extraction. Maps to `regexp_extract()`. Add `kv` mode for key=value parsing.
3. **`timechart`** — time-series aggregation. Implement auto-bucketing heuristics. Maps to `time_bucket()` + GROUP BY.
4. **`top` / `rare`** — syntactic sugar over stats + sort + limit. Small parser addition, small emitter addition.
5. **`dedup`** — standard mode via `ROW_NUMBER()` window function. Consecutive mode via sequential execution (break out of SQL).
6. **`drop`** — inverse projection. Trivial once you have the field catalog to know what "everything except X" means.
7. **`pivot`** — maps to DuckDB's PIVOT. Medium complexity in the emitter.
8. **`fieldsummary`** — meta-command that generates multiple aggregation queries. Sequential execution.
9. **`live`** — presentation directive. The daemon re-executes the query on a timer, pushes deltas. SSE for HTTP, streaming ndjson for sockets.

### 7.2 Deliverable

The full DSL as specified in the design doc is functional. All commands parse, compile to SQL (or execute sequentially where appropriate), and return correct results.

---

## Phase 8: Operational Features (Week 18–20)

### 8.1 Retention Management

Background tokio task on a configurable interval:

1. List files in the Parquet directory with modification times
2. Delete files older than `max_age_days`
3. If free disk space is below threshold, delete oldest files regardless of age
4. Refresh schema catalog after deletion
5. Log all actions

Expose status and controls via admin API.

### 8.2 Metrics

Prometheus exposition format via the `metrics` crate + `metrics-exporter-prometheus`. Instrument: query count/latency, active connections, storage size, retention deletes, auth failures. Expose at `GET /api/v1/metrics`.

### 8.3 Basic Alerting

Alert rules stored in the metadata SQLite database. Background task evaluates rules on schedule:

1. Execute the DSL query
2. Check condition (result count, threshold)
3. POST to webhook URL(s) on trigger
4. Log alert state transitions

CRUD API for alert rules. Keep it simple — cron schedule, DSL query, condition, webhook URL.

### 8.4 Config Reload

Handle SIGHUP: re-read TOML config, apply non-structural changes (log level, query timeout, retention settings) without restart. Log what changed.

### 8.5 Deliverable

fleetd is production-ready for long-running deployment: manages its own storage, exposes metrics for monitoring, can alert on conditions, and can be reconfigured without restart.

---

## Phase 9: TUI (Week 20–24)

This is a separate repo using `ratatui` + `crossterm`, depending on the `fleet-client` crate.

### 9.1 Scaffold

Set up the Elm-architecture pattern: Model (app state), Update (handle events), View (render UI). Define the model:

```
AppState {
    mode: Mode,                    // Search, Results, Detail, Live
    query_input: TextInput,        // search bar state
    results: Vec<Row>,             // current result set
    columns: Vec<ColumnDef>,       // column metadata
    selected_row: usize,           // cursor position
    scroll_offset: usize,          // table scroll
    detail_panel: Option<Row>,     // expanded row
    status: StatusInfo,            // query time, count, connection
    connection: ClientConnection,  // daemon connection
    autocomplete: AutocompleteState,
}
```

### 9.2 Build Order

1. **Connection + basic query.** Connect to daemon, send a hardcoded query, print results to the terminal raw. Verify the client library works.
2. **Search bar.** Text input with keybindings. Enter executes. Display raw results below.
3. **Results table.** Scrollable table with auto-sized columns. j/k navigation.
4. **Detail panel.** Expand selected row, show all fields.
5. **Status bar.** Query timing, result count, connection state.
6. **Autocomplete.** Query schema endpoint as user types. Show dropdown. Tab to accept.
7. **Live mode.** Toggle with `l`. Polling re-query with appended results, auto-scroll.
8. **Polish.** Colors, highlight matching terms in results, error display, help overlay.

### 9.3 Deliverable

A fully functional TUI log viewer. SSH into a box, run `fleet-tui`, search your logs interactively.

---

## Phase 10: Web UI (Week 24–28)

Separate Rails application. This is the most familiar territory.

### 10.1 Foundation

Standard Rails 8 app. PostgreSQL for application data (user accounts, UI preferences). Communication with fleetd via HTTP using `httpx` or `faraday`.

Build a thin service object that wraps fleetd API calls:

```ruby
class fleetClient
  def query(dsl, limit: 500)
  def schema
  def field_values(field)
  def saved_queries
  # etc.
end
```

### 10.2 Search Page

The core experience:

- Text input for DSL queries
- Turbo Frame for results table (streams results as they arrive from fleetd via SSE proxy)
- Column sorting, row expansion for detail view
- URL-encoded queries so search results are linkable/shareable

### 10.3 Autocomplete

Stimulus controller that queries the schema endpoint as the user types. Show a dropdown with field names and value suggestions. This is a standard typeahead pattern.

### 10.4 Saved Searches and Dashboards

CRUD for saved queries (stored via fleetd API). Dashboard page that renders multiple saved queries in a configurable grid layout. Each panel is a Turbo Frame that independently loads its results.

### 10.5 Admin Pages

API key management, retention configuration, alert rule management, active connections/queries view. All proxied through fleetd's admin API. Gated to admin-role users.

### 10.6 Deliverable

A complete web interface for fleet. Search, saved queries, dashboards, admin. Deployed alongside fleetd.

---

## Phase 11: Agent Foundation (Week 28–32)

This is where it gets ambitious. New crates: `fleet-agent`, `fleet-signing`.

### 11.1 Signing Infrastructure (fleet-signing)

The `Signer` trait and the file-based backend. This is the minimum for the system to work:

- Ed25519 key generation (via `ed25519-dalek` crate)
- Key encryption at rest (passphrase → argon2 → encrypt key)
- Signing: canonical JSON serialization of template → sign → attach signature
- Verification: given a template + signature + public key, verify

Extend `fleet-admin`:

```
fleet-admin signing-key generate --out ~/.fleet/signing.key
fleet-admin signing-key register <pubkey-path> --name "jake-workstation"
fleet-admin signing-key list
```

### 11.2 Template Model

Define the template data structures (in `fleet-signing` so both admin and agent can use them):

- Template struct with all constraint fields
- Signed template wrapper (template + signature metadata)
- Canonical JSON serialization (deterministic, for signing)
- Template validation (are constraints consistent? is the command non-empty? is not_after in the future?)

Extend `fleet-admin`:

```
fleet-admin template create templates/disk-check.toml
fleet-admin template push templates/disk-check.toml  # sign + upload
fleet-admin template list
fleet-admin template revoke <id>
```

Server-side: add template storage to the metadata database. API endpoints for template CRUD. Signature verification on upload.

### 11.3 Enrollment Infrastructure

Internal CA implementation (in `fleet-auth` or a new `fleet-pki` crate):

- CA key generation and storage (in the auth SQLite database)
- CSR signing: agent sends a CSR, server signs it with the CA key, returns the certificate
- Enrollment tokens: short-lived, scoped tokens that authorize a new agent to enroll
- Certificate revocation list: track revoked agent certs

Extend `fleet-admin`:

```
fleet-admin enroll-token create --tags linux,webserver --expires 1h
fleet-admin agents list
fleet-admin agents revoke <agent-id>
```

Server-side: enrollment endpoint (`POST /api/v1/agents/enroll`), agent registry in the metadata database.

### 11.4 Deliverable

The signing and enrollment infrastructure works end-to-end. You can generate signing keys, create and sign templates, generate enrollment tokens, and the server validates everything. No agent binary yet.

---

## Phase 12: The Agent (Week 32–36)

### 12.1 Agent Core (fleet-agent)

The agent binary. Minimal dependencies. Build order:

1. **Config loading.** Read the agent TOML config (server address, cert paths, capabilities, allowlists).
2. **mTLS connection.** Connect to fleetd over HTTPS with client certificate authentication. Verify server cert against the CA cert.
3. **Check-in loop.** Long poll: `POST /api/v1/agent/checkin`, include agent metadata, receive tasks or wait. Handle connection drops, reconnection with backoff.
4. **Signature verification.** On receiving a task, deserialize the signed template, verify the signature against the local public key, validate all constraints.
5. **Task execution.** Shell executor: spawn process, capture stdout/stderr, enforce timeout, check exit code. Start with shell tasks only.
6. **Output writing.** Write structured JSON result to the output directory for Vector pickup.
7. **Execution tracking.** Local SQLite database tracking task execution counts (for `max_executions_per_agent` enforcement) and recently seen task IDs (for replay prevention).

### 12.2 Server-Side Task Dispatch

Add to fleetd:

- Check-in endpoint: accept agent metadata, return queued tasks matching the agent's tags/capabilities
- Task submission endpoint: accept a template ID + target + parameters, validate parameters against signed schema, queue the task
- Task status tracking: pending → dispatched → completed/failed/timed-out
- Task timeout reaper: background task marking timed-out tasks

### 12.3 Agent Task Types

Implement in order:

1. **stats** — read-only system metrics from /proc. Safe, no shell involved.
2. **package_list** — detect package manager, query installed packages. Safe, read-only.
3. **file_read** — read allowed files. Validate path against allowlist before reading.
4. **shell** — execute a command. Validate against command allowlist. Enforce timeout. Capture output.
5. **script** — write script to temp file, execute, clean up. Same security model as shell.
6. **file_write** — write to allowed paths. Backup original first. Validate path against allowlist.
7. **signed_package** — download, verify signature, unpack, execute manifest steps. Most complex, implement last.

### 12.4 Vector Configuration for Agent Output

Add a Vector source that tails the agent output directory. Transform to add `service: "fleet-agent"` metadata. Ship alongside normal logs. Provide example Vector config snippets.

### 12.5 Integration Testing

End-to-end test:

1. Start fleetd with test config
2. Create signing key, sign a template
3. Create enrollment token, enroll a test agent
4. Submit a task targeting the test agent
5. Agent checks in, receives task, verifies signature, executes
6. Agent writes output
7. Verify output appears (check the file, or if Vector is running, query via fleet)

### 12.6 Deliverable

A working agent that enrolls, checks in, receives signed tasks, executes them, and reports results through the log pipeline. Fleet management basics are operational.

---

## Phase 13: Hardware Signing Backends (Week 36–38)

### 13.1 YubiKey Backend

Add a YubiKey PIV backend to the `Signer` trait using the `yubikey` crate:

- Detect connected YubiKey
- Generate ed25519 key in PIV slot 9c (key never leaves hardware)
- Sign operations: send data to YubiKey, receive signature
- PIN handling: prompt for PIN, handle lockout/retry

Extend `fleet-admin`:

```
fleet-admin signing-key generate --backend yubikey --slot 9c
fleet-admin template push templates/disk-check.toml --backend yubikey
```

### 13.2 FIDO2 Backend

Add FIDO2 credential-based key unlocking:

- Generate a FIDO2 credential tied to fleet
- Use HMAC-secret extension to derive an encryption key
- Encrypt a software ed25519 signing key with the derived key
- On signing: FIDO2 assertion → derive key → decrypt signing key → sign → zero key

This is more complex than YubiKey PIV. Use `ctap-hid-fido2` crate for the FIDO2 protocol.

### 13.3 Key Policies

Add to the server:

- Per-key restrictions: which template categories a key can sign
- Dual authorization: certain categories require two signatures from different keys
- `fleet-admin template cosign` for the second signature

### 13.4 Deliverable

Templates can be signed with hardware-backed keys. The signing workflow is: edit TOML → one command to sign and push, with hardware confirmation (PIN/touch/biometric).

---

## Phase 14: Playbooks (Week 38–42)

### 14.1 Playbook Engine

Server-side orchestration. Playbooks are stored in the metadata database as JSON definitions.

Build the playbook executor:

1. Playbook is triggered (manually via API, or by an alert rule)
2. Engine evaluates the first step: look up the template, resolve the target, dispatch
3. Engine watches for task results in the log stream (query fleet for `task_id:xxx`)
4. When results arrive, evaluate the next step's condition against the results
5. If condition is met, dispatch the next step's template with resolved parameters
6. If `approval_required`, pause and notify via webhook. Resume on approval API call.
7. Continue until all steps complete or a step fails with no fallback.

### 14.2 Expression Evaluator for Conditions

Playbook conditions (`{{ steps.check_disk.output | parse_df | any(pcent > 90) }}`) need a simple expression evaluator. Options:

- Reuse the DSL expression evaluator from fleet-core (it already handles comparisons, functions, etc.)
- Use a lightweight template language (handlebars/tera for Rust)
- Keep it very simple: just support basic comparisons against task output fields

Start simple. Conditions like `steps.check_disk.exit_code == 0` and `steps.check_disk.row_count > 0` cover most cases. Add complexity only when real playbooks demand it.

### 14.3 Playbook Management

API endpoints:

```
POST   /api/v1/playbooks           # create/update
GET    /api/v1/playbooks           # list
GET    /api/v1/playbooks/{id}      # detail
POST   /api/v1/playbooks/{id}/run  # trigger
POST   /api/v1/playbooks/{id}/steps/{n}/approve  # approve pending step
GET    /api/v1/playbooks/{id}/runs # execution history
```

`fleet-admin` commands:

```
fleet-admin playbook create playbooks/investigate-disk.json
fleet-admin playbook run investigate-disk --hostname web01
fleet-admin playbook approve <run-id> --step 3
fleet-admin playbook runs investigate-disk
```

### 14.4 Deliverable

Server-side playbooks that chain signed templates together based on task results. Human-in-the-loop approval for dangerous steps. Execution history queryable through the log pipeline.

---

## Phase 15: Polish and Packaging (Week 42–44)

### 15.1 Container Image

Multi-stage Dockerfile:

- Build stage: compile all Rust binaries (fleetd, fleet, fleet-admin, fleet-agent)
- Runtime stage: minimal base image (distroless or alpine) with binaries + Vector
- Default config for common use cases
- Health check configured
- Volume mount points for data, config, certs

Publish to a container registry (GitHub Container Registry is free for public packages).

### 15.2 Systemd Units

Service files for:

- `fleetd.service` — the daemon
- `fleet-agent.service` — the agent (on managed endpoints)
- `vector.service` — if not already managed separately

With proper dependencies, restart policies, and security hardening (PrivateTmp, NoNewPrivileges, etc.).

### 15.3 Documentation

- README with quick start guide
- Configuration reference (every TOML option documented)
- DSL reference (every command, with examples)
- Template authoring guide
- Agent deployment guide
- Architecture overview for contributors

### 15.4 OpenAPI Spec

Generate from axum route definitions using `utoipa`. Publish alongside the daemon. Rails web UI and any third-party integrations can use this for client code generation.

### 15.5 Deliverable

fleet is packaged, documented, and deployable by someone other than you.

---

## Ongoing / Cross-Cutting Concerns

These aren't phases — they're practices that apply throughout.

**Testing.** Unit tests in every crate, integration tests in the workspace `tests/` directory, end-to-end tests that exercise the full pipeline. Run `cargo nextest run` before every commit. CI via GitHub Actions.

**Security review.** At minimum, review auth boundaries (are all endpoints properly gated?), SQL generation (is parameterization consistent?), agent command execution (are allowlists enforced?), and signing verification (are signatures checked before any execution?).

**Dogfooding.** From Phase 4 onward, use fleet to search your own homelab logs. From Phase 12 onward, use the agent to manage your own endpoints. Real usage drives real bug reports.

**Performance baseline.** After Phase 3, establish query performance baselines with the fixture data. After Phase 4, baseline with real data. Track regressions. The numbers in the design spec (sub-second filtered queries, 10-60s broad scans) are the targets.

---

## Timeline Summary

| Phase | What | Weeks | Cumulative |
|---|---|---|---|
| 0 | Foundation / learning | 1–2 | 2 |
| 1 | Parser | 3–5 | 5 |
| 2 | SQL generation | 5–7 | 7 |
| 3 | DuckDB + CLI | 7–9 | 9 |
| 4 | Vector integration | 9–10 | 10 |
| 5 | Authentication | 10–12 | 12 |
| 6 | Daemon | 12–16 | 16 |
| 7 | Extended DSL | 16–18 | 18 |
| 8 | Operational features | 18–20 | 20 |
| 9 | TUI | 20–24 | 24 |
| 10 | Web UI | 24–28 | 28 |
| 11 | Agent foundation | 28–32 | 32 |
| 12 | Agent | 32–36 | 36 |
| 13 | Hardware signing | 36–38 | 38 |
| 14 | Playbooks | 38–42 | 42 |
| 15 | Polish + packaging | 42–44 | 44 |

**Key milestones:**

- **Week 9** — first usable artifact (CLI queries Parquet files)
- **Week 16** — daemon is operational (authenticated, multi-transport, log search works)
- **Week 24** — TUI is functional (the "wow, this is actually useful" moment)
- **Week 28** — web UI is live (feature parity with commercial log viewers for basic use)
- **Week 36** — agent is deployed (fleet management via signed templates)
- **Week 44** — full system including playbooks, hardware signing, packaging

These timelines assume roughly half-time effort with heavy Claude Code usage. Full-time would compress by 40-50%. The phases are ordered so that every phase produces something independently useful — you never go more than a few weeks without a tangible deliverable.

---

## Risk Register

| Risk | Impact | Mitigation |
|---|---|---|
| Rust learning curve stalls progress | Phase 1–3 take 2x estimated time | Phase 0 exists specifically for this. Claude Code absorbs mechanical complexity. Timeboxed: if Phase 1 isn't done by week 7, simplify the initial grammar. |
| DuckDB Rust bindings have gaps | Can't execute needed SQL patterns | duckdb-rs wraps the C API comprehensively. Fallback: use the C API directly via FFI. Test early in Phase 3. |
| Parquet file performance at scale | Queries slower than predicted | Profile early with realistic data volumes. DuckDB's EXPLAIN ANALYZE shows where time is spent. Partitioning strategy is the primary lever. |
| chumsky learning curve | Parser is hard to debug/modify | chumsky v1 has better docs than v0. Fallback: hand-rolled recursive descent parser (more code but more control, and arguably easier to understand). |
| Agent security model has gaps | RCE vulnerability in production | The signing model is defense-in-depth by design. Threat model review at Phase 11 start. Start with read-only task types (stats, package_list) before enabling shell. Allowlists are non-negotiable. |
| Scope creep | Project never finishes | Each phase has a clear deliverable. Phases 11–14 (agent, playbooks) are genuinely optional — fleet is useful without them. Treat them as stretch goals. |
| Web UI is less engaging to build than the Rust parts | Web UI is half-finished | Phase 10 is deliberately after the fun stuff. The Rails work is familiar and straightforward — it's a thin proxy with standard CRUD. Timebox it: 4 weeks, ship what's done. |