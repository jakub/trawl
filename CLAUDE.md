# trawl

self-hosted log collection, storage, and search platform for homelabs and small-to-medium infra. splunk-like DSL, zero licensing cost, single-node by design.

## stack

- **core**: rust workspace — parser, SQL emitter, DuckDB executor, daemon, CLI, TUI
- **ingestion**: vector → parquet (columnar, compressed, partitioned by hour)
- **query engine**: custom DSL → AST → DuckDB SQL (parameterized)
- **web ui**: leptos 0.8 CSR SPA (`trawl-web-ui`) served by the `trawl-web` session proxy (cookie sessions → bearer tokens)
- **shared fleet substrate** (ADR-0030, consumed by coastwatch via sibling path deps): `fleet-auth` (postgres keystore + roles-as-data RBAC + session AEAD — its `session` feature also backs trawl-web's `fleet_session` SSO cookie in-repo), `fleet-ui` (leptos design system), `fleet-admin` (ops CLI)
- **agent** (v2 scope): signed-template execution on managed endpoints, mTLS, ed25519 signing — not yet started

## workspace layout

```
crates/
  trawl-core/            # DSL parser, AST, SQL emitter (pure, no I/O); also owns the envelope column catalog + `CanonicalType`/`FieldTypes` pin vocabulary (src/schema.rs — incl. `catalog_key` (alias resolution + ASCII fold) and `FieldTypes::pin_for`, the one lookup the emitter and the live matcher share; the map is `Arc`-shared, copy-on-write, so a per-pass clone is a refcount bump) and the OTel severity ladder + syslog inversion (src/severity.rs) shared by the emitter and the in-memory SSE filter (ADR-0009). the hot+cold emitter takes catalog pins and conforms the HOT branch only (TRY_CAST for typed pins, untyped json_extract_string(to_json(x), '$') for VARCHAR — probed by execution in trawl-engine/tests/duckdb_probe.rs). src/compare.rs is the ONE pin-aware comparison rule table (ADR-0011 slice A) with two consumers — the emitter's field-filter arm and `filter::CompiledFilter` — plus the live mirrors of DuckDB's cast domain (`try_cast_double`/`double_cmp`/`try_cast_bigint`/`try_cast_boolean`) and the one canonical pattern text per pin; `eval::duckdb_double_to_string` is that DOUBLE text (and now keeps `-0.0`'s sign) and `try_cast_double` is the single owner of the cast domain `tonumber()` also uses. src/filter.rs evaluates three-valued (`Truth` = true/false/UNKNOWN propagating through NOT/AND/OR by SQL's rules), so an absent field can no longer be inverted into a match
  trawl-engine/          # DuckDB integration, query execution; owns the list-source pruning retry on partial glob miss, the `_raw`-free retry for sources that lack `_raw` (embedded mode over foreign parquet — decided by re-binding, not by error text, ADR-0009) and the no-silent-cold-drop outcome policy shared by the query and parquet-export paths (src/executor.rs, ADR-0008). there is NO read-time type reconciliation: the coerced-retry ladder is deleted (ADR-0009 slice 2) — a union type conflict means a nonconformant corpus and errors loudly. `Executor::describe_schema` survives solely for the embedded `--data` path (server schema routes read the catalog, ADR-0009 slice 3); `parquet_stats` is stats-only. every query/export lane now takes pins explicitly (ADR-0011 slice A) — `run_query`/`export_parquet` take the comparison snapshot, `run_query_with_hot`/`export_parquet_with_hot` take BOTH sets (`hot_pins` = pins ∩ snapshot keys, conformance; `pins` = full catalog, comparison typing) and carry the same interpretation through the pruned retry AND the hot-only fallback, which goes through `emitter::emit_hot_only` (same REPLACE conformance as the union's hot branch) so an answer can't flip the moment the first parquet lands
  trawl-api/             # shared wire types (request/response structs); envelope-aware result column ordering (`WELL_KNOWN_LOG_FIELDS` leading, `TRAILING_LOG_FIELDS` demoted — mirrored, not imported, by trawl-core/trawl-cli with parity tests) and the single home of schema display order (`value::field_display_rank`/`sort_by_display_rank`, shared by every /schema* route and the CLI field tables); also the catalog wire types (`CatalogFieldsResponse`/`CatalogFieldResponse`/`CatalogConflictsResponse`)
  trawl-config/          # shared config.toml types (no heavy deps); also `fs::open_with_mode` — the ONE owner-only file open (mode at creation + tighten a pre-existing looser file, `O_NOFOLLOW` so a symlink planted at the path is refused rather than followed and chmodded), used by trawld's query debug log and the CLI's TUI trace log, which each keep their own chmod-failure policy; also the injective path-encoding predicates (`is_valid_env_name`, `is_valid_service_name`, `RESERVED_ENV_NAMES`) every ingest path funnels names through (ADR-0009)
  trawl-server/          # daemon (axum, HTTPS via tokio-rustls); owns the postgres app-state store (history/saved/schedule + report runs) in its own `trawl` database — sole-writer via session advisory lock, auto-migrated at boot (src/store/, ADR-0004 slice 3; trawl-auth crate deleted). authz is permission-only (src/policy.rs — no `Role` enum, ADR-0006 slice 1). ingest canonicalizes every event into the declared envelope (src/ingest/envelope.rs, ADR-0009), compaction repairs `_time`/`_ingested` per-row from WAL-filename provenance (src/ingest/compaction.rs, ADR-0008) and the boot-time storage-epoch cutover gates the data root (src/epoch.rs, ADR-0009). the field catalog lives here too: postgres pin store + read model (src/store/catalog.rs, migrations 0002-0005), in-process pin cache + compaction context (src/catalog/), the boot conformance pass with the dual-sided data/CATALOG identity marker and its `field_services` backfill (src/catalog/conform.rs, ADR-0009 slices 2-3), and the catalog-backed schema routes (src/handlers.rs; src/schema_refresh.rs takes its types from the pin cache and touches neither postgres nor DuckDB). the pin cache also types comparisons (ADR-0011 slice A): `FieldCatalog::all()` is the FULL unfiltered snapshot (never `intersect`, whose emptiness would make `status>=400` depend on ingest timing), taken once per query by `ExecutorPool` (`with_field_catalog`, wired for query-only nodes too) and once per stream by `stream_query` — held for the stream's life, so a repin waits for reconnect. self-observation lives here too: src/telemetry.rs owns the `DEFAULT_LOG_FILTER` packaging contract, the `UNMETERED_TARGETS`/`is_persisted_target`/`wal_filter` persistence exclusion, and the WAL-durable-before-visible flush pipeline (blocking-pool writes, FIFO retry queue, one shared memory budget); src/query_log.rs is the opt-in owner-only, size-capped query debug log; src/error.rs owns `error_class` (the content-free classification default telemetry persists in place of a message); src/policy.rs counts every auth rejection into `trawl_auth_failures_total{reason}`
  trawl-client/          # typed async HTTP client library
  trawl-cli/             # unified CLI + TUI; the whole command surface (incl. `schema fields|field|conflicts`) lives in src/lib.rs and the `trawl` bin is a shim, so integration tests drive it in-process. TUI tracing goes to `~/.config/trawl/tui.log`, opened owner-only through `trawl_config::fs::open_with_mode` — a chmod it cannot perform is fatal here (the file is created fresh per run), unlike trawld's inherited query log
  trawl-admin/           # admin CLI (TLS cert generation only — key mgmt lives in fleet-admin)
  trawl-web/             # browser-facing session proxy (serves SPA, cookie → bearer); fleet_session SSO cookie AEAD comes from fleet-auth's session feature, hand-rolled session.rs retired (ADR-0004 slice 2). owns its OWN `DEFAULT_LOG_FILTER` (`trawl_web=info,fleet_auth=info`) — a separate process under a separate target, so inheriting trawld's target-only filter would silence it entirely; stdout-only (no internal telemetry)
  trawl-web-ui/          # leptos 0.8 CSR SPA (wasm32)
  trawl-dashboard/       # shared ratatui dashboard rendering
  trawl-crashdump/       # minidump capture for trawld (linux fatal-signal handler)
  fleet-auth/            # postgres-backed keystore + roles-as-data RBAC (roles/role_permissions/key_roles/app_permissions tables; `verify_key` resolves key→roles→permissions, ADR-0006) + session cookie AEAD + present-only origin validation + axum middleware (ADR-0030); session feature = pure primitives (no pg), consumed by trawl-web
  fleet-ui/              # shared leptos design tokens + components for fleet apps (wasm32)
  fleet-admin/           # fleet keystore ops CLI (migrations, session keys, key lifecycle, `roles` subcommands + `keys assign-role`/`unassign-role`)
  coastwatch-api-types/  # vendored coastwatch API wire types
```

## key design decisions

- single-node only. no clustering, sharding, or multi-tenancy.
- pipeline-oriented DSL: `service=nginx level=error last=2h | stats count() by host | where count > 10`
- SQL injection prevention via parameterized queries + field allowlists
- agent tasks are cryptographically signed offline — compromised server can't create novel execution authority
- **roles are data, permissions are code** (ADR-0006): a role is a named, cross-app bundle of `app:permission` strings living in the fleet keystore; a key holds any number of roles and its effective permissions are the union. handlers gate on compile-time `Permission` variants only — new role = `fleet-admin roles create`, new permission = deploy. unrecognized permission strings are ignored (fail closed), and a key resolving zero recognized trawl permissions 403s. there is no `Role` enum and no `(app, role)` grant anywhere: `/whoami` carries `roles` (names, display/audit only) + `permissions` (gating), and role names never reach `/metrics` labels since it sits outside the auth stack
- **per-key rate limiting**: one token bucket per key id per route class (`default_rpm` for interactive routes, `ingest_rpm` for `/api/v1/ingest`, the latter earned by the `ingest` permission). a role's optional `rate_rpm` overrides — never combines with — the class default; effective ceiling = max `rate_rpm` across the key's roles when any role sets it
- **real-time event bus**: ingested events are published to a `broadcast::channel`-backed bus and stored in a hot buffer, making them queryable within milliseconds of ingest (before WAL compaction to parquet)
- **hot buffer**: batch-keyed in-memory store that makes fresh events visible to ALL queries via `UNION ALL BY NAME` with the parquet source; drained automatically after compaction. the snapshot writer hoists one event per novel key to the front of the ndjson so the full key set lands inside DuckDB's default schema-detection prefix — the reader stays on the cheap default sample instead of paying `sample_size=-1` on every query and SSE poll (ADR-0008). `snapshot()` returns an atomic `HotSnapshot { file, field_types }` where `field_types` = catalog pins ∩ the snapshot's observed key set, recomputed on EVERY call (never cached per generation — a pin landing in compaction's rename-to-drain window must be visible immediately), so the emitter's hot-branch REPLACE never names an absent column — the same REPLACE list is shared with the hot-ONLY lane (`emit_hot_only`), so a cold start reads the catalog's types rather than `read_json`'s inference (ADR-0011 slice A). DuckDB identifiers are case-INSENSITIVE while client JSON keys are not, so field names are ASCII-folded at every producer's own door BEFORE anything can reach the buffer — HTTP ingest in `envelope::canonicalize`, the syslog listener at SD-key construction, telemetry in its `JsonVisitor` — and the former read-side defences (the snapshot writer's case-variant merge, intersect's VARCHAR degrade) are deleted as unreachable. `FieldCatalog::intersect` is a plain exact-name lookup — one DuckDB identifier has exactly one spelling, in the key set and in the catalog
- **the field catalog: write-time type conformance (ADR-0009 slice 2)**: every custom field's type is pinned in a postgres catalog at first typed sight (all-null batches defer; candidate ladder BIGINT/DOUBLE/TIMESTAMP/BOOLEAN/VARCHAR; nested values stringify at ingest and pin VARCHAR, reachable via `json_extract_string`). pins become durable — and land in the in-process cache the query path reads — strictly BEFORE any parquet carrying them is published; conforming TRY_CASTs each pinned column under a lossless round-trip guard — bare TRY_CAST ROUNDS (`1.5` → `2`), so a cast that would alter the value writes NULL instead and counts as a conflict, and the same rule scores the pin ladder (fractional batches pin DOUBLE, ≥90%-boolean batches pin BOOLEAN) — records conflicts in `field_conflicts` + `trawl_catalog_*` metrics, and the nulled original stays findable in `_raw`. every parquet file trawl writes conforms, so hot/cold and cold/cold unions can never type-conflict; merge and rollup are plain UNION ALL BY NAME whose failure is a `catalog_invariant_violation` (WAL/inputs retained, retried) — never a cast-and-retry. the boot pass proves a marker-less or identity-mismatched corpus conformant (seeding is most-rows-wins with each candidate weighted by the rows that actually CARRY the field, staged atomic rewrites fsynced before the rename, `data/CATALOG` published last, `scheduled/` never scanned) and is fatal on failure for ingest nodes — but never for one bad file: an unreadable or foreign parquet is skipped (`catalog_conform_skip`) and the marker withheld (`catalog_conform_incomplete`), so the pass re-runs every boot until it is repaired or moved out. the rewrite is in place and lossy, so ownership is decided from the PATH before anything is opened — only `{env}/{date}/{HH}/{service}.parquet` (or the daily rollup) with every component passing ingest's own injective predicates is adopted. a query-only node (ingest disabled) runs no pass — it owns nothing under the data root — but gates on the same marker, and the refusal is narrow: only a marker naming ANOTHER catalog is fatal (`ArchiveIdentity::{Proven,Empty,Unproven}`), because an archive with NO marker is exactly what an incomplete pass leaves, so it logs `catalog_identity_unproven` and serves, like the ingest node does for the same corpus
- **every catalog table is bounded, and a pin slot is spent permanently**: field and service names are client-chosen, so `MAX_PINNED_FIELDS` = 10 000 pins install-wide (a field arriving at a full catalog stays unpinned — no column, values remain in `_raw`; `catalog_pin_cap_reached` + `trawl_catalog_pins_rejected_total`), `MAX_CONFLICTS_PER_FIELD` = 100 newest rows in `field_conflicts` (a conflict is recorded only when the conform actually nulled rows), trimmed in the writing transaction; `field_services` is deliberately ever-observed — nothing removes a row, retention never reconciles it, consumers window on `last_seen` (field axis bounded by the pin cap, service axis bounded by nothing — so the read surface KEYSET-PAGES it: `/schema/field?name=` returns at most 1000 observations per page behind an opaque `services_cursor`, never the whole history). observations are written by compaction bookkeeping and — for a corpus that predates the catalog — backfilled by the boot pass from the files it adopts, stamped with each file's PARTITION hour rather than boot time (stamping "now" over two-year-old data would put a dead field inside every window) and gated on its OWN `catalog_state.services_backfilled_at` flag (migration 0005) so a node already conformed under slice 2 re-arms the pass exactly once; the backfill is fatal to the pass, unlike the best-effort conflict evidence. live bookkeeping (conflicts + observations) rides out a postgres blip (3 attempts, 100ms doubling) but under a 2s wall-clock budget per batch — a real outage costs one budget and is abandoned (`catalog_bookkeeping_timeout`), because a stalled compactor stops the WAL draining and the hot buffer evicts, which is invisible events. because filling the cap unstores fields for the WHOLE install, compaction is rationed to half the free slots per batch; the boot pass is exempt (its proposals describe columns already on disk — denying one deletes standing data). alert on `trawl_catalog_pinned_fields` / `trawl_catalog_pin_capacity`, not on the cap-reached log
- **the catalog schema surface (ADR-0009 slice 3)**: type authority is surfaced, never forked. `/api/v1/schema` is a postgres SELECT over `field_types` LEFT JOIN grouped `field_services` (`CatalogStore::list_fields`) — windowed on `last_seen` against `retention.max_age_days` (0 disables; `?all=true` lifts; a never-observed pin, e.g. the envelope seed, always shows), `?service=` filters via EXISTS and scopes the AGGREGATE (not just the row set), columns sorted by `trawl_api::value::sort_by_display_rank` and carrying names+types only (`with_conflicts: false` — the conflict evidence is a SECOND query keyed on the page's field names, so the columns endpoint never aggregates the whole `field_conflicts` table); the UNSCOPED column set is TTL-cached under `schema_cache_ttl_secs` beside the corpus-facts filesystem walk (`CachedSchemaColumns` + `CachedCorpusFacts`, one slot keyed on whether the window applied), so a new pin can take a TTL to appear, while `?service=` is always fresh — a client-chosen key would be an unbounded cache, and migration 0003's `field_services (service, field)` index makes that path an index-only scan. a store outage is a 503 — no silent pin-cache fallback. `/api/v1/schema/services` keeps its footer stats but types come from the in-process pin cache (`schema_refresh` is postgres- and DuckDB-free; a pin-less physical column reports `UNPINNED`, warn-deduped). three SchemaRead-gated read routes — `/schema/fields` (aggregates + `pinned_total`/`pin_capacity`, limit default 500), `/schema/field?name=` (query param, ASCII-folded, 404 unpinned), `/schema/conflicts` (`since_secs` window, limit default 100/max 1000) — back `trawl schema fields|field|conflicts` (`--last 7d`, `--limit`; `fields --data <glob>` is the embedded DESCRIBE path, names+physical types only). `parquet_stats` is stats-only; `Executor::describe_schema` survives solely for embedded mode, and every read-time type reconciler (`describe_schema_columns_coerced` and friends) is deleted
- **search-stage comparisons are pin-aware (ADR-0011 slice A)**: a field filter binds by the field's CATALOG PIN, not by the shape of the query literal — one rule table (`trawl-core/src/compare.rs`), two consumers (the emitter's field-filter arm and the in-memory `CompiledFilter`), every rule pinned by execution probes (`trawl-engine/tests/duckdb_probe.rs`) and mirrored by parity tests (`trawl-core/tests/filter_parity.rs`). VARCHAR pin: `=`/`!=`/IN compare as TEXT, and a NUMERIC literal ALSO matches the value's `TRY_CAST(col AS DOUBLE)` reading (`"200"`, `"0200"`, `"200.0"` all answer `status=200`) — the numeric half is not optional, because a VARCHAR column stores `read_json`'s INFERENCE rendered, not the wire spelling, so exact-text equality would be a batch miss and a live hit; ordered comparison with a numeric literal goes through `TRY_CAST(col AS DOUBLE)` (DOUBLE uniformly, never BIGINT — `TRY_CAST('1.5' AS BIGINT)` ROUNDS to 2, which would make `dur>1` and `dur>1.5` disagree about `"1.5"`), and a value the cast can't read is UNKNOWN, not false; ordered with a non-numeric literal stays lexical. typed pins glob/regex against the STORED value's text — one canonical rendering per pin: BIGINT decimal (a wire `"0404"` globs as `404`, `"accepted"` is NULL/UNKNOWN), BOOLEAN the lowercase word (`"TRUE"` globs as `true`), DOUBLE DuckDB's own rendering (`200.0`, `1e-07` — always a fraction, signed two-digit exponent), TIMESTAMP the RFC 3339 UTC-microsecond WIRE form, not DuckDB's space-separated one. unpinned fields, embedded `--data` (`emitter::emit` is deliberately pin-blind and every caller passes an explicit `FieldTypes::new()`) and the pipeline `| where` stage keep literal-driven coercion (`coerce_filter_value`). "numeric literal" is decided by CONTENT — the AST discards quote provenance, so `status>"400"` IS `status>400`. this ships BEFORE the repin engine (#53) so a repin changes storage, never query meaning; the envelope seed pins `host`/`service`/`env`/`message`/`severity_text`/`_raw` VARCHAR, so it is live on day one for every install
- **live tail matches in SQL's three-valued logic (ADR-0011 slice A)**: `CompiledFilter` answers `Truth` — true / false / UNKNOWN — and UNKNOWN propagates through `NOT`/`AND`/`OR` by SQL's rules, so only a final true is a match. collapsing a missing field to *false* at the leaf survives a top-level filter but INVERTS under `NOT`. two live-tail behaviors flip, both toward the batch answer `/api/v1/query` has always given: `f!=x` now MATCHES events carrying no `f` (its emitted form is `("f" != ? OR "f" IS NULL)`, the one total comparison — on a sparse custom field that is most of the firehose, pair with `f=*`), and `NOT f=x` — plus `NOT level=…` and `NOT <bare term>` — NO LONGER matches them (`NOT (NULL)` is NULL, filtered out), so a live alert written `NOT f=x` to catch events MISSING `f` stops firing and must be rewritten `f!=x`
- **the declared event envelope (ADR-0009)**: ten fields enforced at ingest — `_time`, `_ingested`, `_raw`, `_repairs`, `env`, `service`, `host`, `severity`, `severity_text`, `message` (`_` = metadata about the record's handling). the canonicalizer (`trawl-server/src/ingest/envelope.rs`) captures `_raw` FIRST (client string honoured verbatim, else pre-repair serialization), strips server-owned `_ingested`/`_repairs` (`meta.stripped`), consumes the `_time`/`timestamp`/`@timestamp` wire aliases, stamps `_ingested`, defaults-or-rejects `env` against the `[ingest] envs` allowlist, peer-fills `host` (rejects behind a `trusted_relays` peer), and derives OTel `severity` (integer 1-24 > `severity_text` > `level`, all consumed; syslog numerics 0-7 INVERTED). repairs are a closed enum recorded in `_repairs` + `trawl_ingest_repairs_total{code,service}`. principle: repair when the server has an honest answer, reject (typed reason) when it would guess — `service` missing/non-string/bad-charset and unlisted `env` reject. custom FIELD NAMES are policed here too, per-field and never per-event: every name is ASCII-lowercased FIRST (`field.name_case_folded` when it changed something — DuckDB identifiers are ASCII case-insensitive, so `Dur` IS `dur`; an in-event collision after folding keeps one value, exact spelling wins else the lexicographically-first variant, loser dropped with `field.name_case_collision`), and >255 bytes (`MAX_FIELD_NAME_BYTES`, too long to be a catalog btree key) drops the field with `field.name_too_long` — the rest of the event lands, and both the name and its value stay findable in `_raw`
- **timestamps are canonicalized at ingest, never fatal downstream** (ADR-0008): a valid `_time` input (RFC 3339, ISO 8601 basic offsets like `+0530`/`+02`, offset-less date-times read as UTC, `T` or space separator, `YYYY-MM-DD` or `YYYY/MM/DD`, optional seconds/fraction, or a bare date at midnight UTC) is rewritten to RFC 3339 UTC microseconds; anything else is **substituted, not rejected** — `_time` becomes the arrival time with the `time.from_ingest` repair code (the original is findable in `_raw`). parseable-but-implausible values (>10y past / >1d future) are kept and flagged `time.out_of_range`. the partition key is never hard-CAST: compaction resolves `_time` (and `_ingested`, same ladder) through `COALESCE(TRY_CAST(raw), ingest instant from the row's own WAL filename, compaction instant)` and the hot/cold union `TRY_CAST`s both TIMESTAMP columns on the hot side, so one bad value can never wedge a batch or throw a union
- **storage layout (ADR-0009)**: `data/{env}/{date}/{HH}/{service}.parquet` (daily rollup: `data/{env}/{date}/{service}.parquet`), WAL under `wal/{env}/`. path encoding is injective by validation — env charset `[a-z0-9_-]{1,32}` (`wal`/`scheduled` reserved), service charset `[A-Za-z0-9._-]` (no spaces, no leading dot, ≤128 bytes), names written VERBATIM (no sanitizer; `api.v2` ≠ `api_v2`). `data/EPOCH` (content `2`) marks the layout; boot runs the restartable cutover table in `trawl-server/src/epoch.rs` (legacy root → `data.pre-schema-v2/`, never deleted by trawl)
- **DSL severity**: `level` is a query alias for numeric `severity` — `level=error` → `severity BETWEEN 17 AND 20`, `level>=warn` → `severity >= 13`; unknown/glob/regex `level` values are emit errors; `level` in projections/group-by surfaces column-not-found (use `severity`/`severity_text`). `timestamp`/`@timestamp` alias to the physical `_time` column. bare text search covers `message` OR `_raw`. token table + syslog inversion live in `trawl-core/src/severity.rs`; envelope consts in `trawl-core/src/schema.rs`
- **a cold-data drop is never a silent 200** (ADR-0008): hot-only fallback is permitted only where it cannot hide cold data — a genuine cold start (glob matches no parquet) or a missing-column user error. any other database failure with cold files present returns an error — including a union type conflict, which post-catalog can only mean foreign/nonconformant parquet (DuckDB reports an irreconcilable schema and an unconvertible value identically as `Conversion`-class, so no read-time classifier could tell them apart; the write-time invariant makes one unnecessary)
- **reserved field names**: the envelope's `_ingested`/`_repairs` (client values stripped with `meta.stripped`; non-string `_raw` likewise) and `_trawl_wal_file` (per-row WAL provenance carried through compaction — silently dropped from incoming events, the rest accepted unchanged). case-variants of reserved names cannot exist past ingest: field names are ASCII-lowercased at canonicalization, so a lone `_Time` IS the `_time` wire input, `_Ingested` strips as `_ingested`, and a variant next to the exact spelling loses the collision (`field.name_case_collision`) — the canonical value is unforgeable
- **live streaming**: SSE endpoint uses `CompiledFilter` (in-memory DSL matcher) against the event bus for real-time event delivery, with back-pressure notifications via `StreamEvent::Lagged`. the filter is compiled with the catalog's pin snapshot and evaluates three-valued, so a streamed query and its batch form agree event for event (ADR-0011 slice A)
- **self-telemetry is trawld observing trawld, durable before it is visible**: when `[ingest] internal_telemetry` is on, a tracing layer turns the daemon's own events into ordinary `service=trawld` records through the ingest WAL. each flush stages one batch and writes it on Tokio's BLOCKING pool (both fsync barriers off the async executor); only after the write succeeds does the batch reach the hot buffer and SSE — exactly once, so a query can never see telemetry a restart would erase. a failed write RETAINS its batch on a FIFO retry queue and drains oldest-first, coalescing consecutive queued batches into units of ≤4 MiB (one WAL file, one `batch_id`) so an hours-long outage recovers in writes proportional to queued bytes, not one per flush tick — and nothing merges until a write has actually succeeded. `[ingest] telemetry_buffer_max_bytes` (default 16 MiB) is ONE estimated-charge budget over the active buffer + the queue + the batch in flight, enforced as events ARRIVE (a wedged `spawn_blocking` write would otherwise let the active buffer grow unbounded); over budget the oldest queued batches shed first, then the incoming event, all exactly accounted in `trawl_telemetry_{events,bytes}_dropped_total{reason="preinit_cap"|"buffer_cap"}` beside `trawl_telemetry_wal_write_failures_total` and the `trawl_telemetry_buffer_{events,bytes}` depth gauges — scrapeable precisely while self-ingestion is down. shutdown is budgeted end to end (periodic flush raced against the signal, 5s final drain, 10s `runtime.shutdown_timeout`) so a frozen volume costs a parked blocking thread, never a stalled restart. the boundary is narrow and deliberate: trawld ONLY — `trawl-web` and the CLIs are stdout-only, fleet key mutations are a coalescing 30s poll (not a transactional ledger), and there is no OTLP export
- **the log filter is a packaging contract, and an unmetered rejection is never persisted**: `telemetry::DEFAULT_LOG_FILTER` = `trawl_server=info,trawld=info,fleet_auth=info,auth.backend=info,storage.backend=info,preauth.transport=info` — the same literal in the code fallback, Helm `logLevel`, the Debian env example and the docs, because a target-only filter silently drops every target it fails to name (`trawl_server=info` alone hid `main.rs`, the backend alarm targets and all fleet-auth middleware). `RUST_LOG` is authoritative when valid; invalid installs the default and emits ONE `config_warning` (never the raw env value), and a pre-tracing config failure only reaches stderr, with the resolved path. persistence is strictly narrower than stdout: `UNMETERED_TARGETS` — `fleet_auth`, `auth.backend`, `preauth.transport` (the accept loop's pre-TLS diagnostics), `trawl_server::policy::unmetered` (the grantless 403) — are excluded from the WAL by a second, non-configurable predicate (`is_persisted_target`, ANDed in `wal_filter`; an appended `=off` directive would lose to a more specific operator directive). all four are decided OUTSIDE the per-key rate limiter — the limiter needs a verified key, a bare TCP connect-and-close provokes a handshake warning with no request at all, and `require_trawl_grant` is mounted outside the limiter on purpose (a grantless key never spends a bucket to be told no) — so persisting them would hand a client nothing meters a durable-write amplifier, one ~400-byte record per rejection. the signal moves to `/metrics`, not away: `trawl_auth_failures_total{reason}` (`unauthorized`/`backend_unavailable`/`no_trawl_grant` + defensive `forbidden`/`internal`) counts every rejection from trawl's own policy layer, a closed label set with no key, name or path, so a flood cannot amplify it. query lifecycle telemetry carries metadata, never DSL: `query_start`/`query_complete`/`query_timeout`/`query_failed`, `export_start`/`export_complete` and `stream_start` share a `query_id` and carry `query_len` + actor/roles/outcome/timing, and failures carry `ServerError::error_class` (`parse`/`emit`/`database`/`timeout`/`store`, …) INSTEAD of a message — parse/emit text quotes the user's own tokens and a database error embeds the generated SQL. raw text survives only in authenticated history, the opt-in `server.query_log` debug log (`0600`, `O_NOFOLLOW` so a symlink at the path is refused, `server.query_log_max_bytes` rollover to a single `<path>.1`, one retry per cap on a failed rename, closed on an un-undoable rollover), and the DEBUG-only `query_text`/`query_error_text` events

## tooling

- **edition 2024**, resolver 3, rust-version 1.94 MSRV
- **clippy pedantic** + `unsafe_code = "forbid"` at workspace level
- **lefthook** pre-commit: `cargo fmt --check` + `cargo clippy -- -D warnings`
- **bacon** for continuous clippy-on-save (`bacon` or `bacon test`)
- **cargo-nextest** for testing, **cargo-insta** for snapshot tests
- **cargo-deny** for license/vulnerability auditing

## using trawl

### binaries

- **trawl CLI binary**: `target/debug/trawl` (after `cargo build -p trawl-cli`)
- the binary name is `trawl`, NOT `trawl-cli` — the crate is `trawl-cli` but the binary is `trawl`
- the server binary is `target/debug/trawld` (crate `trawl-server`)
- admin binary is `target/debug/trawl-admin` (crate `trawl-admin`)
- web proxy binary is `target/debug/trawl-web` (crate `trawl-web`) — serves the SPA + translates cookie sessions to bearer tokens
- web UI crate is `trawl-web-ui` — leptos 0.8 CSR SPA, built via `trunk build`

### web UI deploy

the web UI ships as a single binary: `trawl-web` with the SPA baked in
via `rust-embed`. the canonical build is:

```sh
cargo xtask build-web --release
# produces target/release/trawl-web with dist/ embedded
```

the xtask runs `trunk build --release` inside `crates/trawl-web-ui/`
then `cargo build --release -p trawl-web` so rust-embed picks up the
fresh SPA. `cargo xtask` is aliased in `.cargo/config.toml`.

for local iteration there are two faster flows:
- **trunk serve** (SPA hot reload): `cd crates/trawl-web-ui && trunk serve` — proxies `/api/*` to a separately-run `trawl-web` on :8090. full docs in `Trunk.toml`.
- **env override**: `TRAWL_WEB_SPA_DIR=$(pwd)/crates/trawl-web-ui/dist cargo run -p trawl-web` — `trawl-web` serves a pre-built `dist/` from disk instead of its embedded copy. lets you rebuild the SPA without recompiling the binary.

### packaging

`trawl-web` ships by default in both distribution channels:

- **debian**: bundled inside the `trawl-server` .deb alongside `trawld` + `trawl-admin`. postinst generates `/var/lib/trawl/web.cookie` (32 random bytes) and enables+starts `trawl-web.service` — the proxy listens on `127.0.0.1:8090` out of the box. the `[web]` block in `/etc/trawl/trawld.toml` is shared with trawld.
- **helm**: sidecar container in the same StatefulSet pod as trawld, guarded by `web.enabled` (default true). cookie key is stored in a chart-managed Secret and preserved across upgrades via helm's `lookup` function. ingress defaults to the web sidecar (`ingress.backend: web`) — switch to `ingress.backend: trawld` for bearer-token api clients.

both distributions pre-generate/preserve the cookie secret so sessions survive restart.

CLI clients (`trawl query`, `trawl-client`) and vector still hit **trawld directly** on port 5514 — the proxy only speaks cookies and hard-404s `/api/v1/ingest`.

### environments

config uses named profiles (`~/.config/trawl/config.toml`):
- **base** (no `--profile`): live homelab server at `trawl-01.lab.ktle.net:5514`
- **dev** (`--profile dev` or `TRAWL_PROFILE=dev`): localhost:5514 with self-signed cert

env vars and CLI flags override profile settings.

### CLI modes

**TUI** (interactive):
```
trawl                               # launches TUI against base (live) server
trawl -p dev                        # TUI against dev server
```

**query** (execute and print):
```
trawl query "dsl..."                             # auto-detect output: table for TTY, JSON for pipe
trawl query -p dev "dsl..."                      # query dev server
trawl query -f table "dsl..."                    # force table output
trawl query -f json "dsl..."                     # force JSON (one object per line, ndjson)
trawl query -f csv "dsl..."                      # CSV output (with formula injection protection)
```

**embedded mode** (no server, queries parquet files directly):
```
trawl query --data '/path/*.parquet' "dsl..."    # query local parquet files
trawl query --data 'data/**/*.parquet' "* | stats count() by service"
```

**validate** (syntax check, hits server for validation endpoint):
```
trawl validate "dsl..."                          # validates against base server
trawl validate -p dev "dsl..."                   # validates against dev server
```

**schema** (read the field catalog — pins, per-service observations, conflicts):
```
trawl schema fields                              # pinned fields + types + conflict counts (limit 500)
trawl schema fields --service nginx --last 7d    # scoped + windowed (aggregates scope too)
trawl schema field duration                      # one field: pin, services (paged), conflicts
trawl schema field duration --limit 500 --after '<cursor>'   # next page of observations
trawl schema conflicts --last 7d                 # schema-health view (limit 100, max 1000)
trawl schema fields --data 'data/**/*.parquet'   # embedded DESCRIBE: names + physical types only
```
`fields`/`conflicts` honour `-f table|json|csv`; catalog metadata needs a server (routes are `schema_read`-gated).

### global flags

| flag | env var | description |
|------|---------|-------------|
| `-p, --profile <NAME>` | `TRAWL_PROFILE` | named profile from config (overrides `[server]`) |
| `--url <URL>` | `TRAWL_URL` | server URL (default: `https://localhost:5514`) |
| `--token <TOKEN>` | `TRAWL_TOKEN` | API token (direct value) |
| `--insecure` | `TRAWL_INSECURE` | accept self-signed TLS certificates |
| `-c, --config <PATH>` | — | config file path (default: `~/.config/trawl/config.toml`) |

### output formats

- **table**: pretty-printed box-drawing table with row count footer (default for TTY)
- **json**: one JSON object per row, ndjson-style (default for pipes)
- **csv**: RFC 4180 with formula injection protection (string values starting with `=`, `+`, `-`, `@`, `\t`, `|` are prefixed with `'`)
- **parquet**: DuckDB COPY TO with Snappy compression (file output only, requires `--output <path>`)

### common dev examples

```sh
# recent errors by service (live server, base profile)
trawl query "level=error last=1h | stats count() by service | sort -count | head 10"

# same query against dev server
trawl query -p dev "level=error last=1h | stats count() by service | sort -count | head 10"

# browse all data (table output)
trawl query -f table "* | head 5 | fields timestamp, host, service, severity, message"

# pipe JSON to jq for ad-hoc processing
trawl query "last=1h | stats count() by service" | jq '.service'

# export to CSV file
trawl query -f csv "last=24h | stats count() by service, severity_text" > report.csv

# validate a query without executing
trawl validate "level=error | stats count() by host"

# query local parquet files (no server needed)
trawl query --data 'data/**/*.parquet' "* | stats count() by service | sort -count"
```

### HTTP API

the trawl server exposes a REST API. all routes under `/api/v1` except `/health` and `/ingest` require bearer token auth:

| method | path | description |
|--------|------|-------------|
| `GET` | `/api/v1/health` | health check (unauthenticated) |
| `POST` | `/api/v1/query` | execute a DSL query |
| `POST` | `/api/v1/validate` | validate DSL syntax |
| `GET` | `/api/v1/schema` | column names + types from the field catalog (`?service=`, `?all=true`) |
| `GET` | `/api/v1/schema/services` | rich per-service schema (footer stats, catalog types) |
| `GET` | `/api/v1/schema/fields` | pinned-field listing with aggregates + catalog fill |
| `GET` | `/api/v1/schema/field?name=` | one field's pin, observations, conflicts (404 unpinned) |
| `GET` | `/api/v1/schema/conflicts` | recent type conflicts (`?field=`, `?service=`, `?since_secs=`) |
| `GET` | `/api/v1/schema/values/{field}` | get distinct values for a field |
| `GET` | `/api/v1/queries` | list running queries |
| `DELETE` | `/api/v1/queries/{id}` | cancel a running query |
| `GET` | `/api/v1/stats` | server statistics |
| `GET` | `/api/v1/dashboard` | full dashboard snapshot (admin only) |
| `GET` | `/api/v1/whoami` | token identity, role names, and resolved trawl permissions |
| `GET` | `/api/v1/history` | query execution history |
| `GET` | `/api/v1/saved` | list saved queries |
| `POST` | `/api/v1/saved` | create a saved query |
| `PUT` | `/api/v1/saved/{id}` | update a saved query |
| `DELETE` | `/api/v1/saved/{id}` | delete a saved query |
| `PUT` | `/api/v1/saved/{id}/schedule` | upsert schedule on saved query |
| `GET` | `/api/v1/saved/{id}/schedule` | get schedule |
| `DELETE` | `/api/v1/saved/{id}/schedule` | delete schedule |
| `GET` | `/api/v1/saved/{id}/runs` | list report runs (paginated) |
| `GET` | `/api/v1/saved/{id}/runs/{run_id}` | get report run with result data |
| `POST` | `/api/v1/export` | export query results (csv/json/parquet) |
| `GET` | `/api/v1/stream` | SSE stream of query results |
| `POST` | `/api/v1/ingest` | ingest log events (JSON array/ndjson, optional gzip) |
| `GET` | `/metrics` | prometheus metrics (outside /api/v1, unauthenticated) |

## docs

the docs site lives under `docs/` (astro starlight). key pages:

- `docs/src/content/docs/architecture/overview.md` — design principles, components, comparison to splunk/datadog
- `docs/src/content/docs/architecture/data-flow.md` — ingestion pipeline, compaction, storage layout, query execution
- `docs/src/content/docs/about/roadmap.md` — what's shipped, what's next, what's out of scope
- `docs/src/content/docs/reference/{cli,dsl,api,configuration}.md` — user-facing reference
- `docs/src/content/docs/getting-started/{index,first-query,vector-integration}.md` — onboarding

repo-root docs:

- `README.md` — project overview
- `CHANGELOG.md` — release history
- `TUI_ROADMAP.md` — opinionated UX review for the TUI

## DSL quick reference

### query structure

```
[search stage] | [pipe stage] | [pipe stage] ...
```

search stage is optional. pipelines can start with `|` for raw log access.

### search stage (pre-pipeline filtering)

**field filters**
```
service=nginx                    # exact match
status=200,301,404              # IN list (comma-separated)
status>=400                     # comparison (>, >=, <, <=, !=)
path=/api/*                     # glob pattern
message=/error.*/               # regex pattern (slashes required)
host="db host"                  # quoted values (for spaces/special chars)
env=prod                        # environment (prunes the path glob)
```

**operators**: `=`, `!=`, `>`, `>=`, `<`, `<=`

**pinned comparison semantics (server only)** — a search-stage filter binds by the field's catalog pin, not by the literal: against a VARCHAR pin `=`/`!=`/IN compare as text (a numeric literal also matching any spelling of the same number — `"200"`, `"0200"`, `"200.0"`) while `status>=400` compares numerically via `TRY_CAST(col AS DOUBLE)` (non-numeric values quietly don't match), and glob/regex over a numeric/boolean/timestamp pin matches the STORED value's text (BIGINT decimal, lowercase `true`/`false`, DuckDB's double rendering `200.0`/`1e-07`, RFC 3339 UTC micros). embedded `--data` and `| where` stay literal-driven. a field an event doesn't carry is NULL → UNKNOWN everywhere, batch and live: `f!=x` MATCHES those events, `NOT f=x` does NOT. full table in `docs/src/content/docs/reference/dsl.md`.

**severity (`level` is an alias, not a column)** — `level` compiles to band predicates over the numeric `severity`: `level=error` → `severity BETWEEN 17 AND 20`, `level>=warn` → `severity >= 13`, `level=warn,error` → either band. tokens: `trace`/`t`, `debug`/`d`, `info`/`i`, `notice`, `warn`/`warning`/`w`, `error`/`err`/`e`, `fatal`/`critical`/`crit`/`f`, `alert`, `emerg`/`panic`. anything else — an unknown token, a glob or regex on `level`, or naming `level` anywhere outside a comparison (`table`/`fields`, `stats by`, `sort`, `dedup`, `rename`, `let`) — is a query error; project `severity` (number) or `severity_text` (original text) instead. `where level == "error"` works and matches live tail (SSE) exactly.

**time aliases** — `timestamp` and `@timestamp` both resolve to the physical `_time` column.

**text search**
```
error                           # bare word (substring match over message OR _raw)
-debug                          # negated (excluded when either column matches)
"connection refused"            # exact phrase
```
searching `_raw` is whole-event search: for server-filled `_raw` (the canonical pre-repair JSON) a bare term can match another field's value or a field *name*. confine a match to one column with a field filter (`message=/debug/`).

**time filters**
```
last=2h                         # units: s, m, h, d, w
last=7d
last=30m
```

**OR grouping**
```
service=nginx OR service=apache # OR-separated groups
a b OR c d                      # implicit AND within groups: (a AND b) OR (c AND d)
```

### pipe stages

**stats** — aggregation with optional grouping
```
stats count()
stats count() by host
stats avg(duration) by status
stats count(), avg(duration) by service, host
stats avg(duration) as avg_duration
```

**where** — filter on computed values
```
where count > 10
where avg_duration > 100
where status == 200 and count > 5
where host matches /prod-.*/
where x in (1, 2, 3)
```

**sort** — order results (`-` prefix for descending)
```
sort count                      # ascending
sort -count                     # descending
sort status, -count             # multi-field
```

**limit** / **head** — cap result count
```
limit 20
head 20                          # SPL alias for limit
```

**tail** — last N rows (defaults to timestamp DESC if no prior sort)
```
tail 5
```

**table** / **fields** — select output columns
```
table host, status
fields host, status              # SPL alias for table
```

**top** — most frequent values
```
top 10 host                     # top 10 hosts by frequency
top 5 status by service         # top 5 statuses per service
```

**rare** — least frequent values
```
rare 5 status                   # 5 rarest status codes
rare 3 host by service          # 3 rarest hosts per service
```

**drop** — exclude columns
```
drop message
drop host, raw
```

**let** / **eval** — computed/derived fields
```
let duration_ms = duration * 1000
eval status_class = status / 100 # SPL alias for let
let is_error = status >= 400
```

**extract** / **rex** — field extraction

regex (named groups):
```
extract "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message
rex "(?P<code>[A-Z]+)" from raw  # SPL alias for extract
```

key-value pairs:
```
extract kv                      # from 'message' field
extract kv from raw             # from specific field
```

**rename** — rename columns
```
rename service as svc
rename service as svc, host as hostname
```

**dedup** — remove duplicates (keeps most recent)
```
dedup                           # entire row
dedup host                      # by single field
dedup host, service             # by multiple fields
```

**timechart** — time-bucketed aggregation
```
timechart span=5m count()
timechart span=1h count() by service
timechart span=30s count(), avg(duration)
```

**pivot** — pivot table transformation
```
pivot count() on status
pivot avg(duration) on service by host
```

### expressions (in `where`, `let`, aggregations)

**literals**: `42`, `1.5`, `"string"`, `true`, `false`, `null`

**field refs**: `host`, `host.name`, `@timestamp`

**arithmetic**: `+`, `-`, `*`, `/`, `%`

**comparison**: `==`, `!=`, `>`, `>=`, `<`, `<=`

**logical**: `and`, `or`, `not`

**pattern**: `matches` (regex)

**lists**: `x in (1, 2, 3)`

**precedence**: parens supported `(count + 1) * 2`

### aggregation functions

**basic stats**
- `count()` — row count
- `count(field)` — non-null count
- `avg(field)` — mean
- `sum(field)` — total
- `min(field)` — minimum
- `max(field)` — maximum

**cardinality**
- `dc(field)` or `distinct_count(field)` — unique value count

**percentiles**
- `p50(field)` — median
- `p90(field)` — 90th percentile
- `p95(field)` — 95th percentile
- `p99(field)` — 99th percentile

**positional / collection**
- `first(field)` — first value
- `last(field)` — last value
- `values(field)` / `list(field)` — list of distinct values
- `median(field)` — median value
- `stddev(field)` — standard deviation

### scalar functions (in `let`/`eval`, `where`, expressions)

**string**
- `lower(field)` — lowercase
- `upper(field)` — uppercase
- `length(field)` / `len(field)` — string length
- `trim(field)` / `ltrim(field)` / `rtrim(field)` — whitespace trimming
- `replace(field, old, new)` — string replacement
- `substr(field, start[, len])` — substring extraction

**numeric**
- `abs(x)` — absolute value
- `ceil(x)` / `ceiling(x)` — round up
- `floor(x)` — round down
- `round(x[, n])` — round to n decimal places

**conditional / type**
- `if(cond, then, else)` — ternary conditional
- `isnull(x)` / `isnotnull(x)` — null checks
- `coalesce(a, b, ...)` — first non-null value
- `typeof(x)` — value type name
- `now()` — current timestamp

### example queries

```
# errors in the last hour by service
level=error last=1h | stats count() by service | sort -count

# slow requests by endpoint
status=200 last=24h | where duration > 1000 | stats avg(duration) by uri | sort -avg_duration | head 10

# 4xx/5xx rate by host
status>=400 last=2h | stats count() by host, status | where count > 10

# extract IPs and count
"connection from" | rex "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message | stats count() by ip | sort -count

# time series of error rate
level=error OR level=fatal | timechart span=5m count() by service

# dedup flapping alerts
service=monitoring | dedup host, alert_name

# pivot status codes by host
last=1h | pivot count() on status by host

# last 5 events with renamed columns
* | rename service as svc, host as hostname | tail 5

# conditional field + null handling
* | eval msg_len = if(isnotnull(message), length(message), 0) | fields host, msg_len | head 10

# distinct values per group
* | stats values(severity_text), first(message) by service | head 10
```
