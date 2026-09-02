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
  trawl-core/            # DSL parser, AST, SQL emitter (pure, no I/O); also owns the envelope column catalog + `CanonicalType`/`FieldTypes` pin vocabulary (src/schema.rs — incl. `catalog_key`, an ASCII fold and nothing else since ADR-0013 deleted the aliases, `is_reserved_name`, the ONE `_`-prefix predicate both doors share, and `FieldTypes::pin_for`, the one lookup the emitter and the live matcher share; the map is `Arc`-shared, copy-on-write, so a per-pass clone is a refcount bump) and the OTel severity ladder + syslog inversion + the INJECTIVE exact-name table `otel_name`/`number_for_exact` and `token_text`, the one owner of the `_severity` cell display rule every renderer goes through (src/severity.rs), shared by the emitter and the in-memory SSE filter (ADR-0009/0013). src/conform.rs is the ONE conform-expression builder both lanes emit (ADR-0011): `guarded_cast(text, pin)` over the column's TEXT form (`untyped_text` = `json_extract_string(to_json(x), '$')`, the one spelling compaction, the repin rewrite and the emitter all conform through, because a per-lane spelling would be a per-lane stored value), `decimal_reading`/`DECIMAL_COMPARISON_SPACE`, and `SESSION_TIME_ZONE_SQL` (`SET TimeZone='UTC'`, mandatory on every connection that conforms, scores the pin ladder or reads a conformed hot branch — bundled DuckDB links ICU and otherwise defaults to the HOST zone). the hot+cold emitter takes catalog pins and conforms the HOT branch only, through that same builder, so a hot value reads exactly as it will once compacted (envelope `_time`/`_ingested` keep their unconditional TRY_CAST — ingest already made them UTC); probed by execution in trawl-engine/tests/duckdb_probe.rs. src/compare.rs is the ONE pin-aware comparison rule table (ADR-0011 slices A/A′) reached through two doors — `compare_form`/`pattern_form` for a search-stage literal, `compare_form_bound` for a parsed AST literal (quote provenance discarded by CONTENT, so `where status == "400"` IS `where status == 400`) — and rendered/evaluated in exactly two places: src/emitter/compare.rs holds the SQL renderers both lanes share (search-stage field filters and the pipeline expression emitter, differing only in `NullPolicy` — the search stage's `!=` widens with `OR field IS NULL`, the pipeline is `Strict`), src/pin_match.rs holds the one in-memory mirror behind both `filter::CompiledFilter` and `eval` (the `pin_literal` rule dispatch), while compare.rs also owns the live mirrors of DuckDB's cast domain (`decimal_micros` = the exact i128 mirror of `DECIMAL(38,6)`, `conformed_bigint`/`conformed_boolean`/`conformed_severity` = the guarded readings, `try_cast_double`, and `double_total_cmp` = DuckDB's total DOUBLE order for DOUBLE-pinned values; text-vs-number comparison never goes through DOUBLE, it is `DECIMAL(38,6)`) and the one canonical pattern text per pin (`canonical_timestamp_text`, compare.rs, is a hand-written parser mirroring `TRY_CAST(text AS TIMESTAMPTZ)` under a UTC session, matrix-pinned in duckdb_probe.rs); `eval::duckdb_double_to_string` is that DOUBLE text (and now keeps `-0.0`'s sign) and `try_cast_double` is the single owner of the cast domain `tonumber()` also uses. src/filter.rs evaluates three-valued (`Truth` = true/false/UNKNOWN propagating through NOT/AND/OR by SQL's rules), so an absent field can no longer be inverted into a match. src/pin_scope.rs is the compile-time pin scope walk (ADR-0011 slice A′): `PinScope::advance` moves the catalog snapshot across one pipe stage, the match is exhaustive so a new stage variant must state its rule or fail to compile, and the SQL emitter and `stream::compile_stream_plan` consume the SAME walk — which pin applies at each stage is identical in both lanes by construction. `ast::FloatLiteral` carries a float literal's SOURCE TOKEN beside the parsed `f64`, because pin-aware comparison binds the text and `f64` cannot name `9007199254740993`. src/projection.rs is the lane-neutral answer to what a projecting stage NAMES (ADR-0013 ruling 8): `agg_output_name` is the ONE aggregate-output-name derivation (explicit `as` alias, else `func_arg`/`func` over the recursive innermost-field walk) read by the stats/timechart/eventstats emitters, the stream compiler and the pin-scope walk, and `check_projection` is the duplicate-output-name refusal (plus `eventstats`' explicit-`as` requirement) both doors run — `emitter::validate_pipeline` after its per-stage checks, `stream::compile_stream_plan` before its own unsupported-stage ones, so a pivot/eventstats collision gets the semantic sentence rather than "not supported in streaming mode"; its messages render an aggregate back through `format.rs` and `sanitize.rs`, since an argument's string literal admits what a name cannot. `parser::suggest::quote_dsl_field` is the one DSL field-name renderer (bare iff bare-lexable and not one of the three `GRAMMAR_KEYWORDS`, the `SEARCH_KEYWORDS` group separator `OR`/`or`, nor one of the `EXPRESSION_KEYWORDS` an expression position reads first — `true`/`false`/`null` and the operator words — else backticked with doubled escapes), consumed by `format.rs`, the TUI's completions and the web UI's query composition, and drift-guarded against the real grammar
  trawl-engine/          # DuckDB integration, query execution; owns list-source resolution — `ResolvedSource`/`resolve_list_source` narrow a list to the elements that reach a file ONCE, before the read but AFTER the caller's hot snapshot, on every lane (`read_parquet` rejects a whole list when a SINGLE element misses; the planner's parent-dir filter upstream is advisory only — per-element liveness has exactly ONE authority, and resolving before the snapshot would let a file published in between fall out of both union halves), and the evidence that resolution gathers is what `cold_presence` later reads instead of globbing again, so a file that matched and then vanished lands `ColdDataUnread` rather than being re-resolved away (#73, ADR-0008). the injected matcher splits by element shape — a literal element is one `std::fs::metadata` call (a `glob()` round trip is ~1ms and a service-pinned window is ALL literals), a pattern goes through `glob()`, both erring toward RETAINING the element so an unreadable path fails loudly at the read; `fs_matcher_agrees_with_duckdb_glob_on_literals` is the drift guard — the `_raw`-free retry for sources that lack `_raw` (embedded mode over foreign parquet — decided by re-binding, not by error text, ADR-0009) and the no-silent-cold-drop outcome policy — ONE table, `cold_action(outcome, cold, hot)`, reached by all FOUR entry points through two shared classifiers, with `HotLane` making a missing hot buffer an INPUT rather than a reason to skip the gate, so `run_query`/`export_parquet` answer exactly as their hot twins do; the cold-presence check is paid only where the table's verdict depends on it (`consults_cold_presence`, derived from the table), so a successful query never globs (src/executor.rs, ADR-0008). there is NO read-time type reconciliation: the coerced-retry ladder is deleted (ADR-0009 slice 2) — a union type conflict means a nonconformant corpus and errors loudly. `Executor::describe_schema` survives solely for the embedded `--data` path (server schema routes read the catalog, ADR-0009 slice 3); `parquet_stats` is stats-only. every query/export lane now takes pins explicitly (ADR-0011 slice A) — `run_query`/`export_parquet` take the comparison snapshot, `run_query_with_hot`/`export_parquet_with_hot` take BOTH sets (`hot_pins` = pins ∩ snapshot keys, conformance; `pins` = full catalog, comparison typing) and carry the same interpretation through the resolved source AND the hot-only fallback, which goes through `emitter::emit_hot_only` (same REPLACE conformance as the union's hot branch) so an answer can't flip the moment the first parquet lands. the `rust_stages` tail behind `extract kv` is a second EVALUATOR, not a printer: `EmittedQuery::rust_stage_pins` (the pin scope stamped at the kv split) rides into `post_process::apply_rust_stages` so its `where`/`let` bind under the same pins as the SQL prefix (ADR-0011 slice A′), and because that tail reads a TIMESTAMP cell's text as UTC the SQL is rendered UNSHIFTED whenever a tail follows — `execute_emitted_tracked` reports which result columns came back TIMESTAMP and the display offset is applied AFTER the tail
  trawl-api/             # shared wire types (request/response structs); `QueryResponse`'s two non-blocking advisory channels — `degraded_fields` (ADR-0011 C1) and `severity_columns` (which result columns render as severity tokens, ADR-0013), both omitted from the wire when empty; envelope-aware result column ordering (`WELL_KNOWN_LOG_FIELDS` leading, `TRAILING_LOG_FIELDS` demoted — mirrored, not imported, by trawl-core as `schema::LEADING_LOG_FIELDS`/`TRAILING_LOG_FIELDS` with parity tests) and the single home of schema display order (`value::field_display_rank`/`sort_by_display_rank`, called by every /schema* route in trawl-server; the CLI prints rows in the order the server sent); also the catalog wire types (`CatalogFieldsResponse`/`CatalogFieldResponse`/`CatalogConflictsResponse`) and the repin ones (`RepinRequest`/`RepinResponse`/`RepinStatusResponse` over a single `RepinJobResponse` — the same job row is the dry-run report, the progress record and the outcome, so the HTTP status carries the verdict and the body never changes shape)
  trawl-config/          # shared config.toml types (no heavy deps); also `fs::open_with_mode` — the ONE owner-only file open (mode at creation + tighten a pre-existing looser file, `O_NOFOLLOW` so a symlink planted at the path is refused rather than followed and chmodded), used by trawld's query debug log and the CLI's TUI trace log, which each keep their own chmod-failure policy; also the injective path-encoding predicates (`is_valid_env_name`, `is_valid_service_name`, `RESERVED_ENV_NAMES`) every ingest path funnels names through (ADR-0009)
  trawl-server/          # daemon (axum, HTTPS via tokio-rustls); owns the postgres app-state store (history/saved/schedule + report runs) in its own `trawl` database — sole-writer via session advisory lock, auto-migrated at boot (src/store/, ADR-0004 slice 3; trawl-auth crate deleted). authz is permission-only (src/policy.rs — no `Role` enum, ADR-0006 slice 1). ingest canonicalizes every event into the declared ten-field envelope and ALL THREE producers enter there as PROFILES (ADR-0013 slice 2) — reserved-prefix strip, then observe-don't-consume derivation off the per-profile source lists, then the `_producer` stamp (src/ingest/envelope.rs, ADR-0009 as reshaped by ADR-0013); src/ingest/producer.rs owns the closed `ProducerKind` (http|syslog|trawld), the `Asserted` identity a profile proves, and `Derivation::resolve` — the ONE boot-fatal read of `[ingest] severity_from`/`time_from`, resolved in `main` before the tracing subscriber and threaded everywhere, since a second resolution is the shape that lets two doors disagree. src/syslog/convert.rs and src/telemetry.rs hand that door a PAYLOAD MAP and write no envelope field of their own (no direct `_severity`: the listener publishes the raw PRI numeral as `syslog_severity` and the profile's fixed `dialect = "syslog"` source inverts it), so the universal gates they used to skip — `_raw` cap, name-length drop, env allowlist, `_repairs` — now apply; the syslog batcher keys on `pipeline::BatchKey` = (env, service), which is why an event can no longer be misfiled under the listener's default env, compaction repairs `_time`/`_ingested` per-row from WAL-filename provenance (src/ingest/compaction.rs, ADR-0008) and the boot-time storage-epoch cutover gates the data root (src/epoch.rs, ADR-0009; epoch 3 sets an epoch-2 root aside under a SECOND name, ADR-0013). the field catalog lives here too: postgres pin store + read model (src/store/catalog.rs, migrations 0002-0005; 0007 adds repin_jobs; 0008-0009 add conflict samples + `field_conflict_stats`; 0010 admits the SEVERITY spelling and reseeds the envelope for the namespace cutover; 0011 seeds `_producer`; 0012 adds the repin job's `dialect`; 0013 adds `repin_jobs.planned_at`), in-process pin cache + compaction context + the degraded-pin analyzer (src/catalog/, incl. analyzer.rs), the boot conformance pass with the dual-sided data/CATALOG identity marker and its `field_services` backfill (src/catalog/conform.rs, ADR-0009 slices 2-3), and the catalog-backed schema routes (src/handlers.rs; src/schema_refresh.rs takes its SCHEMA types from the pin cache and touches no DuckDB, but its tick also reloads the degraded-field set from postgres — the one postgres read in that job, ADR-0011 slice C1). the pin cache also types comparisons (ADR-0011 slice A): `FieldCatalog::all()` is the FULL unfiltered snapshot (never `intersect`, whose emptiness would make `status>=400` depend on ingest timing), taken once per query by `ExecutorPool` (`with_field_catalog`, wired for query-only nodes too) and once per stream by `stream_query`, where that ONE snapshot roots BOTH the search-stage `CompiledFilter` and the pipeline plan's `PinScope` so a stream cannot disagree with itself (slice A′) — held for the stream's life, so a repin waits for reconnect. self-observation lives here too: src/telemetry.rs owns the `DEFAULT_LOG_FILTER` packaging contract, the `UNMETERED_TARGETS`/`is_persisted_target`/`wal_filter` persistence exclusion, and the WAL-durable-before-visible flush pipeline (blocking-pool writes, FIFO retry queue, one shared memory budget); src/query_log.rs is the opt-in owner-only, size-capped query debug log; src/error.rs owns `error_class` (the content-free classification default telemetry persists in place of a message); src/policy.rs counts every auth rejection into `trawl_auth_failures_total{reason}`. src/repin/ is the ADR-0011 slice-B engine: marker.rs (`data/REPIN` + the SIBLING staging roots `data.repin-next`/`data.repin-aside` — siblings because DuckDB's `**` glob descends into dot-dirs, probed), plan.rs (the scan every job runs, counting with the SAME expressions the rewrite writes), rewrite.rs (hardlink-or-rewrite per file; affected = owned layout path + footer carries the column), gate.rs (RepinCoordinator: corpus-gate RwLock read-wrapped around each compaction batch + job-long RAII rollup pause), cutover.rs (the idempotent per-env two-rename swap shared by live cutover and boot replay), engine.rs (claim → scan → dry-run/force-gate/background build → bounded additive catch-up → cutover under corpus gate + `ExecutorPool::exclusive`, forward-only past the Cutover marker), recover.rs (the marker decision table: filesystem half BEFORE `ensure_current_epoch`, postgres half after AppState — idempotent flip + `clear_conformed` re-arm + orphan reconciliation). the engine pre-flights the FILESYSTEM before it builds anything: marker.rs refuses a data root that IS a mount point or holds a nested one anywhere in its env subtree (the siblings would land on the parent fs — hardlinks EXDEV, and the per-env rename EBUSY once the dir contains a mount), and rewrite.rs refuses a symlink under an env dir rather than dropping it from the shadow (the swapped-aside original would be the only copy left for the cleanup sweep to delete). every data-root marker — EPOCH, CATALOG, REPIN — now publishes through the ONE staged-write idiom, `epoch::publish_marker_staged` (staged temp → fsync → rename → dir fsync). src/pool.rs owns the query-side exclusion the cutover leans on: `ExecutorPool::exclusive` (every permit, bounded) plus `sample_field_values`, so even `/schema/values` autocomplete expands its glob INSIDE a permit and cannot list paths before the swap and read them after; it also owns `severity_columns_for` and stamps `ExecuteOutcome.severity_columns` INSIDE the permit-holding blocking task, under the snapshot the run EXECUTED with (#79 — a repin can now create a SEVERITY pin at runtime, so the handler's second snapshot could disagree; the walk is also client-shaped cost, a regex per `extract` stage, and belongs where `max_concurrent` bounds it). The `| from saved` lane roots that walk in an EMPTY `FieldTypes` — the DSL there is what FOLLOWS the stage whose `PinScope` rule clears the scope, so a saved run's stored columns are not typed by this corpus's live pins (a deliberate token-rendering change; machine formats always carried the number)
  trawl-client/          # typed async HTTP client library
  trawl-cli/             # unified CLI + TUI; the whole command surface (incl. `schema fields|field|conflicts|repin|repin-status`) lives in src/lib.rs and the `trawl` bin is a shim, so integration tests drive it in-process. TUI tracing goes to `~/.config/trawl/tui.log`, opened owner-only through `trawl_config::fs::open_with_mode` — a chmod it cannot perform is fatal here (the file is created fresh per run), unlike trawld's inherited query log
  trawl-admin/           # admin CLI (TLS cert generation only — key mgmt lives in fleet-admin)
  trawl-web/             # browser-facing session proxy (serves SPA, cookie → bearer); fleet_session SSO cookie AEAD comes from fleet-auth's session feature, hand-rolled session.rs retired (ADR-0004 slice 2). owns its OWN `DEFAULT_LOG_FILTER` (`trawl_web=info,fleet_auth=info`) — a separate process under a separate target, so inheriting trawld's target-only filter would silence it entirely; stdout-only (no internal telemetry)
  trawl-web-ui/          # leptos 0.8 CSR SPA (wasm32); every DSL text it composes goes through `quote_dsl_field` (facet filters, schema drill-ins) and every text-level walk over a query skips backtick spans as it does quotes and regex literals (`query_merge`), so a `` `last` `` FIELD is not read as an existing time clause and a `` `a|b` `` name does not split the search stage
  trawl-dashboard/       # shared ratatui dashboard rendering
  trawl-crashdump/       # minidump capture for trawld (linux fatal-signal handler)
  fleet-auth/            # postgres-backed keystore + roles-as-data RBAC (roles/role_permissions/key_roles/app_permissions tables; `verify_key` resolves key→roles→permissions, ADR-0006) + session cookie AEAD + present-only origin validation + axum middleware (ADR-0030); session feature = pure primitives (no pg), consumed by trawl-web
  fleet-ui/              # shared leptos design tokens + components for fleet apps (wasm32; the pure contract modules — `theme::prefs`, `atmosphere::palette`, the geometry/state helpers — compile natively and carry their unit tests). also owns `Atmosphere`, the decorative WebGL mesh-gradient backdrop over a committed, drift-gated `vendor/paper-shaders.js` (@paper-design/shaders 0.0.79, esbuild ESM bundle, Apache-2.0 banner stamped into the artifact) — ADR-0012. wasm-bindgen reads the path-shaped `module = "/vendor/paper-shaders.js"` as a COMPILE-TIME snippet, so merely LINKING the extern block emits its 142 KB into the consumer's dist and modulepreloads it, mounted or not: the extern block and the component over it therefore sit behind the **default-off `atmosphere` cargo feature**, opted into per consumer (`features = ["atmosphere"]` on the dep, or `data-cargo-features` on Trunk's `rel="rust"` link) and needing no other build wiring — unlike `fleet-ui.css`, a real runtime asset Trunk must copy. both consumers now mount it: coastwatch (login + auth-boot, coastwatch#311) and trawl-web-ui (`/login`, dep-level feature). `atmosphere::palette` stays unconditional and is the single knobs site (per-theme hex stops, shader name, speed, texture), its two `--accent` stops machine-pinned against `fleet-ui.css`; degradation is silent (no WebGL2 / lost context → the `var(--bg)` CSS floor, permanently) and `prefers-reduced-motion` freezes it to a static frame. the workbench bin (`src/bin/shell_demo.rs`, `trunk serve`) mounts it on `/login`, is the only wasm-target build of the backdrop and the evidence venue for it; CI wasm-checks both feature sides, rebuilds the vendor bundle for drift, and asserts exactly ONE snippet is emitted in the workbench dist and in trawl-web-ui's (zero = feature fell off, more = stale wasm-bindgen staging leaked into dist)
  fleet-admin/           # fleet keystore ops CLI (migrations, session keys, key lifecycle, `roles` subcommands + `keys assign-role`/`unassign-role`)
  fleet-dev/             # local-only development controller (ADR-0010): `bin/fleet-dev` (and the `bin/dev` compatibility translator) prepares the dev postgres, runs fleet migrations, mints the dev role/key + shared session material, reconciles Tailscale Serve, generates Trunk config and attaches the `mprocs` process tree for the trawl and coastwatch stacks, declared in each repo's `fleet-dev.toml`; supports exactly those two stacks, not a general plugin runner
```

## key design decisions

- single-node only. no clustering, sharding, or multi-tenancy.
- pipeline-oriented DSL: `service=nginx _severity>=error last=2h | stats count() by host | where count > 10`
- SQL injection prevention via parameterized queries + field allowlists
- agent tasks are cryptographically signed offline — compromised server can't create novel execution authority
- **roles are data, permissions are code** (ADR-0006): a role is a named, cross-app bundle of `app:permission` strings living in the fleet keystore; a key holds any number of roles and its effective permissions are the union. handlers gate on compile-time `Permission` variants only — new role = `fleet-admin roles create`, new permission = deploy. unrecognized permission strings are ignored (fail closed), and a key resolving zero recognized trawl permissions 403s. there is no `Role` enum and no `(app, role)` grant anywhere: `/whoami` carries `roles` (names, display/audit only) + `permissions` (gating), and role names never reach `/metrics` labels since it sits outside the auth stack
- **per-key rate limiting**: one token bucket per key id per route class (`default_rpm` for interactive routes, `ingest_rpm` for `/api/v1/ingest`, the latter earned by the `ingest` permission). a role's optional `rate_rpm` overrides — never combines with — the class default; effective ceiling = max `rate_rpm` across the key's roles when any role sets it
- **real-time event bus**: ingested events are published to a `broadcast::channel`-backed bus and stored in a hot buffer, making them queryable within milliseconds of ingest (before WAL compaction to parquet)
- **hot buffer**: batch-keyed in-memory store that makes fresh events visible to ALL queries via `UNION ALL BY NAME` with the parquet source; drained automatically after compaction. the snapshot writer hoists one event per novel key to the front of the ndjson so the full key set lands inside DuckDB's default schema-detection prefix — the reader stays on the cheap default sample instead of paying `sample_size=-1` on every query and SSE poll (ADR-0008). `snapshot()` returns an atomic `HotSnapshot { file, field_types }` where `field_types` = catalog pins ∩ the snapshot's observed key set, recomputed on EVERY call (never cached per generation — a pin landing in compaction's rename-to-drain window must be visible immediately), so the emitter's hot-branch REPLACE never names an absent column — the same REPLACE list is shared with the hot-ONLY lane (`emit_hot_only`), so a cold start reads the catalog's types rather than `read_json`'s inference (ADR-0011 slice A). DuckDB identifiers are case-INSENSITIVE while client JSON keys are not, so field names are ASCII-folded at the ONE door BEFORE anything can reach the buffer — `envelope::canonicalize`, which every producer profile enters through (the syslog listener's SD-key fold and telemetry's `JsonVisitor` fold are DELETED, not duplicated) — and the former read-side defences (the snapshot writer's case-variant merge, intersect's VARCHAR degrade) are deleted as unreachable. `FieldCatalog::intersect` is a plain exact-name lookup — one DuckDB identifier has exactly one spelling, in the key set and in the catalog
- **the field catalog: write-time type conformance (ADR-0009 slice 2)**: every custom field's type is pinned in a postgres catalog at first typed sight (all-null batches defer; candidate ladder BIGINT/DOUBLE/TIMESTAMP/BOOLEAN/VARCHAR; nested values stringify at ingest and pin VARCHAR, reachable via `json_extract_string`). pins become durable — and land in the in-process cache the query path reads — strictly BEFORE any parquet carrying them is published; conforming casts each pinned column's TEXT form under a lossless round-trip guard (`trawl_core::conform::guarded_cast`, the SAME builder the emitter's hot branch emits, ADR-0011) — bare TRY_CAST ROUNDS (`1.5` → `2`) and reads a vocabulary it won't write back (`TRUE` → `true`), so a cast that would alter the value writes NULL instead and counts as a conflict, and the same rule scores the pin ladder (fractional batches pin DOUBLE, ≥90%-boolean batches pin BOOLEAN); text-first because the cast domain must not vary with `read_json`'s per-batch inference, and the TIMESTAMP rung parses through TIMESTAMPTZ (offset APPLIED, zoneless read as UTC) under the mandatory `SET TimeZone='UTC'` — records conflicts in `field_conflicts` + `trawl_catalog_*` metrics, and the nulled original stays findable in `_raw`. every parquet file trawl writes conforms, so hot/cold and cold/cold unions can never type-conflict; merge and rollup are plain UNION ALL BY NAME whose failure is a `catalog_invariant_violation` (WAL/inputs retained, retried) — never a cast-and-retry. the boot pass proves a marker-less or identity-mismatched corpus conformant (seeding is most-rows-wins with each candidate weighted by the rows that actually CARRY the field, staged atomic rewrites fsynced before the rename, `data/CATALOG` published last, `scheduled/` never scanned) and is fatal on failure for ingest nodes — but never for one bad file: an unreadable or foreign parquet is skipped (`catalog_conform_skip`) and the marker withheld (`catalog_conform_incomplete`), so the pass re-runs every boot until it is repaired or moved out. the rewrite is in place and lossy, so ownership is decided from the PATH before anything is opened — only `{env}/{date}/{HH}/{service}.parquet` (or the daily rollup) with every component passing ingest's own injective predicates is adopted. a query-only node (ingest disabled) runs no pass — it owns nothing under the data root — but gates on the same marker, and the refusal is narrow: only a marker naming ANOTHER catalog is fatal (`ArchiveIdentity::{Proven,Empty,Unproven}`), because an archive with NO marker is exactly what an incomplete pass leaves, so it logs `catalog_identity_unproven` and serves, like the ingest node does for the same corpus
- **every catalog table is bounded, and a pin slot is spent permanently on the ingest path** (an operator repin retypes it; nothing reclaims it): field and service names are client-chosen, so `MAX_PINNED_FIELDS` = 10 000 pins install-wide (a field arriving at a full catalog stays unpinned — no column, values remain in `_raw`; `catalog_pin_cap_reached` + `trawl_catalog_pins_rejected_total`), `MAX_CONFLICTS_PER_FIELD` = 100 newest rows in `field_conflicts` (a conflict is recorded only when the conform actually nulled rows), trimmed in the writing transaction; `field_services` is deliberately ever-observed — nothing removes a row, retention never reconciles it, consumers window on `last_seen` (field axis bounded by the pin cap, service axis bounded by nothing — so the read surface KEYSET-PAGES it: `/schema/field?name=` returns at most 1000 observations per page behind an opaque `services_cursor`, never the whole history). observations are written by compaction bookkeeping and — for a corpus that predates the catalog — backfilled by the boot pass from the files it adopts, stamped with each file's PARTITION hour rather than boot time (stamping "now" over two-year-old data would put a dead field inside every window) and gated on its OWN `catalog_state.services_backfilled_at` flag (migration 0005) so a node already conformed under slice 2 re-arms the pass exactly once; the backfill is fatal to the pass, unlike the best-effort conflict evidence. live bookkeeping (conflicts + observations) rides out a postgres blip (3 attempts, 100ms doubling) but under a 2s wall-clock budget per batch — a real outage costs one budget and is abandoned (`catalog_bookkeeping_timeout`), because a stalled compactor stops the WAL draining and the hot buffer evicts, which is invisible events. because filling the cap unstores fields for the WHOLE install, compaction is rationed to half the free slots per batch; the boot pass is exempt (its proposals describe columns already on disk — denying one deletes standing data). alert on `trawl_catalog_pinned_fields` / `trawl_catalog_pin_capacity`, not on the cap-reached log
- **the catalog schema surface (ADR-0009 slice 3)**: type authority is surfaced, never forked. `/api/v1/schema` is a postgres SELECT over `field_types` LEFT JOIN grouped `field_services` (`CatalogStore::list_fields`) — windowed on `last_seen` against `retention.max_age_days` (0 disables; `?all=true` lifts; a never-observed pin, e.g. the envelope seed, always shows), `?service=` filters via EXISTS and scopes the AGGREGATE (not just the row set), columns sorted by `trawl_api::value::sort_by_display_rank` and carrying names+types only (`with_conflicts: false` — the conflict evidence is a SECOND query keyed on the page's field names, so the columns endpoint never aggregates the whole `field_conflicts` table); the UNSCOPED column set is TTL-cached under `schema_cache_ttl_secs` beside the corpus-facts filesystem walk (`CachedSchemaColumns` + `CachedCorpusFacts`, one slot keyed on whether the window applied), so a new pin can take a TTL to appear, while `?service=` is always fresh — a client-chosen key would be an unbounded cache, and migration 0003's `field_services (service, field)` index makes that path an index-only scan. a store outage is a 503 — no silent pin-cache fallback. `/api/v1/schema/services` keeps its footer stats but types come from the in-process pin cache (`schema_refresh`'s SCHEMA half is postgres- and DuckDB-free — its tick's ONE postgres read is the degraded-set reload, ADR-0011 slice C1; a pin-less physical column reports `UNPINNED`, warn-deduped). three SchemaRead-gated read routes — `/schema/fields` (aggregates + `pinned_total`/`pin_capacity`, limit default 500), `/schema/field?name=` (query param, ASCII-folded, 404 unpinned), `/schema/conflicts` (`since_secs` window, limit default 100/max 1000) — back `trawl schema fields|field|conflicts` (`--last 7d`, `--limit`; `fields --data <glob>` is the embedded DESCRIBE path, names+physical types only). `parquet_stats` is stats-only; `Executor::describe_schema` survives solely for embedded mode, and every read-time type reconciler (`describe_schema_columns_coerced` and friends) is deleted
- **search-stage comparisons are pin-aware (ADR-0011 slice A)**: a field filter binds by the field's CATALOG PIN, not by the shape of the query literal — one rule table (`trawl-core/src/compare.rs`), two consumers (the emitter's field-filter arm and the in-memory `CompiledFilter`), every rule pinned by execution probes (`trawl-engine/tests/duckdb_probe.rs`) and mirrored by parity tests (`trawl-core/tests/filter_parity.rs`). VARCHAR pin: `=`/`!=`/IN compare as TEXT, and a NUMERIC literal ALSO matches the value's `DECIMAL(38,6)` reading (`"200"`, `"0200"`, `"200.0"` all answer `status=200`) — the numeric half is not optional, because a VARCHAR column stores `read_json`'s INFERENCE rendered, not the wire spelling, so exact-text equality would be a batch miss and a live hit; ordered comparison with a numeric literal reads BOTH SIDES through the same `conform::decimal_reading` (ADR-0011 ruling #6 — the literal binds as its own TEXT so it never round-trips through f64; DOUBLE was blind above 2^53 in BOTH engines alike, matching three distinct snowflake ids, and BIGINT is impossible because `TRY_CAST('1.5' AS BIGINT)` ROUNDS to 2, which would make `dur>1` and `dur>1.5` disagree about `"1.5"`), and a value with no reading — `nan`, `inf`, `0x10`, ≥10^32 — is UNKNOWN, not false; ordered with a non-numeric literal stays lexical. typed pins glob/regex against the STORED value's text — one canonical rendering per pin: BIGINT decimal (a wire `"0404"` globs as `404`; `"1.5"`/`"0x10"` fail the guard and `"accepted"` has no reading at all — every one NULL/UNKNOWN), BOOLEAN the lowercase word and ONLY a genuine one (a wire `"TRUE"`/`"t"`/`"yes"`/`"1"` fails the round trip and is NULL, so `flag=TRUE*` matches nothing), DOUBLE DuckDB's own rendering (`200.0`, `1e-07` — always a fraction, signed two-digit exponent; the one unguarded rung, since pinning DOUBLE MEANS accepting its precision), TIMESTAMP the RFC 3339 UTC-microsecond form of the stored INSTANT — the conform is zone-aware, so an offset-bearing custom value globs at its UTC hour (`09:00:00+05:30` → `/T03:30/`), not DuckDB's space-separated rendering. unpinned fields and embedded `--data` (`emitter::emit` is deliberately pin-blind and every caller passes an explicit `FieldTypes::new()` / `PinScope::unpinned()`) keep literal-driven coercion (`coerce_filter_value`). "numeric literal" is decided by CONTENT — the AST discards quote provenance, so `status>"400"` IS `status>400`. this ships BEFORE the repin engine (#53) so a repin changes storage, never query meaning; the envelope seed pins `host`/`service`/`env`/`message`/`_raw` VARCHAR and `_severity` SEVERITY, so it is live on day one for every install
- **the pipeline stages are pin-aware too (ADR-0011 slice A′)**: a BARE field-vs-literal comparison inside `| where` or `| let` consults the same catalog pin and adopts the same rule table as the search stage, in all three lanes — the SQL emitter, the SSE stream, and the `rust_stages` batch tail behind `extract kv`. concretely, over a VARCHAR pin `| where status > 400` stops raising a Conversion error and filters in `DECIMAL(38,6)`, `== 200` gains the numeric arm (a stored `"200.0"` matches), `in (…)` routes each element through the equality rule, and `matches`/`like`/`ilike` against a typed pin target the pin's canonical text. **excluded shapes stay literal-driven, structurally**: field-vs-field, function-wrapped fields, arithmetic on the field, `== null`, and a pattern with the field on the RIGHT (both operand orders bind for comparisons — `400 < status` IS `status > 400` — but only the left operand of a pattern op is a subject). **which pin applies follows the PIPELINE**, not a flat name lookup (`pin_scope::PinScope::advance`): `rename` remaps it (an unpinned source scrubs any pin the target name held), a computed `let` unpins while a bare alias copies the pin, aggregations (`stats`/`timechart`/`pivot`) keep group-by keys and nothing else, `extract <regex>` unpins its capture-group names, `extract kv` passes the scope THROUGH (accepted, documented residual: a kv key shadowing a pinned name is read under that pin), `from saved` clears. **one NULL-policy difference from the search stage, kept on purpose**: the pipeline `!=` does NOT carry the `OR field IS NULL` widening (`NullPolicy::Strict`) — plain SQL null propagation is what the pin-blind `| where` always answered, so a repin never changes missing-field semantics. batch/live agreement is proven generatively in `trawl-core/tests/pin_stage_parity.rs` beside slice A's `filter_parity.rs`. this completes the invisibility promise and UNBLOCKS the repin engine (#53)
- **operator-triggered repin (ADR-0011 slice B, #53)**: `trawl schema repin <field> --to <type>` / `POST /api/v1/schema/repin` (new `Permission::SchemaWrite`, registered in app_permissions but granted to NO role by the migration; status route is SchemaRead and works on query-only nodes) retypes one field's corpus via a shadow-generation rewrite with resurrection: the new generation is staged as a SIBLING of the data root (`data.repin-next/` — in-root dot-staging is impossible, DuckDB's `**` glob descends into dot-dirs), unaffected/foreign files hardlink, affected files rewrite through `ConformPolicy::Repin`, whose target column reads `trawl_core::conform::resurrection_expr` = COALESCE(guarded stored reading, guarded `_raw` re-extraction — exact-key RFC 6901 pointer + best-effort case-variant fallback), so shelved conflict values return under the same lossless guard and the dry run counts with the very expression the rewrite writes. every request (dry or real) claims the one-running `repin_jobs` row (partial unique index → 409) and runs the same scan IN the request — a full-corpus pass, so the POST can outlive a client timeout, but the claimed job then runs DETACHED (202) and reaches a terminal status on its own: a disconnect never strands the slot, and a timed-out repin is polled from the status route, never retried blind. a lossy plan without force parks `refused_needs_force` with the plan as the 409 body; `to == current` + force is the resurrection-only pass. the force gate is asked AGAIN of the FINISHED shadow — ingest runs throughout, so a file written after the scan can carry unreadable values — and a 202 job therefore still ends `refused_needs_force` with the corpus untouched. the job also pre-flights the filesystem before it builds (mount point at or under the data root, symlink under an env dir, or a shadow root that survived a previous sweep → refused, dry run included). an additive bounded catch-up loop folds in mid-build compaction (the file-relocating rollup is paused job-long, RAII), and the cutover — final increment, `data/REPIN` marker, two renames per env dir, transactional pin flip (postgres + `FieldCatalog::repin`, the cache's first non-add-only path) — runs under BOTH exclusion primitives (compaction corpus gate + `ExecutorPool::exclusive` over every permit, bounded → `blocked` outcome), because probes proved mixed scalar unions SILENTLY PROMOTE rather than error: exclusion is the whole atomicity budget, and past the Cutover marker the engine is forward-only (a swap/flip failure exits crash-consistent; the marker replay finishes it). boot recovery is a decision table: filesystem half BEFORE the epoch gate (building → abandon shadow; cutover → complete renames; cleanup → sweep; a query-only node serves a building marker and REFUSES a cutover one), postgres half after AppState (idempotent `finish_cutover`, `clear_conformed` re-arms the conformance pass that same boot, orphaned running rows fail). retention stands down entirely (age AND pressure) while marker/staging exists, re-read immediately before every deletion so a job admitted mid-tick still stops the sweep — the job pre-flights its double-held bytes against `min_free_disk_bytes`, and a staging root whose sweep FAILS deliberately keeps the marker (the marker is what licenses trawl to delete that root; the next boot retries). residuals: SSE streams keep their pin snapshot until reconnect; a query-only node keeps stale pins until restart. metrics: `trawl_catalog_repin_jobs_total{outcome}`, `_running`, `_files_{total,done}`, `_rows_{nulled,resurrected}_total`, `_duration_seconds` — plus `trawl_retention_suppressed`, the one to ALERT on: it is 1 for every tick a sweep stands down, so unlike `_running` (0 in exactly that case) it catches staging no job owns, which suppresses retention indefinitely while the archive grows
- **a repin can put a SENDER's field on the severity ladder (ADR-0013 ruling 10, #79)**: `repin --to severity` is admitted through `CanonicalType::from_catalog` (the INJECTIVE door — `from_duckdb` still cannot name it, so inference cannot mint the pin), and the envelope refusal is now the predicate `schema::is_contract_typed` = `is_reserved_name` ∪ the four sender-asserted names (`env`/`service`/`host`/`message`), so a contract slot added later is refused the day it exists. The one dialect-changing decision an operator can make is `--dialect otel|syslog`, persisted on the job row (migration 0012: nullable, CHECKed present iff `to_type = 'SEVERITY'` — a legacy or non-severity job reports NULL, never a backfilled `otel`), NEVER on the pin, the catalog or the `data/REPIN` marker (dialect-free by design: no replay path re-runs a cast). It is carried PER ARM — `conform::RepinTarget { pin, stored, raw }`, wrapped by `compaction::RepinReading::new(from, to, dialect)` and built ONLY by the engine because only the engine knows the OLD pin: a stored column that was already SEVERITY holds canonical ladder positions and keeps its OTel reading (re-reading a stored `3` as syslog would corrupt it to 17) while the `_raw` re-extraction, the sender's own wire text, takes the assertion. `guarded_cast` delegates to `guarded_cast_in(text, pin, Otel)`, so "live conform is OTel" is a fact about CALL SITES rather than a second expression. AMBIGUITY is derived, whole-expression: a row is ambiguous iff the full target expression reads non-NULL under BOTH wire dialects and they differ (`severity::dialect_ambiguous` / `conform::severity_dialect_ambiguous_sql`, probe-paired) — exactly the integers 1-7, which covers the resurrection arm for free and keeps one-dialect values out (those are visible loss `projected_nulls` already reports). The count is taken ALWAYS; the REFUSAL fires iff the target is SEVERITY, the asserted dialect is not syslog, and force is absent — `repin::force_refusal(pin, dialect, nulled, ambiguous, force)` is ONE decision with THREE askers (the scan gate, the finished-shadow gate, and `requires_force` on every job row the wire carries, because a dry run terminates `succeeded` and would otherwise read as a green light). The report also carries ≤5 `unmapped_samples` (`compaction::bounded_misfit_samples_sql`, an `approx_top_k` sketch — deliberately NOT the conform's own `distinct_misfit_samples_sql`, which is unbounded and private to compaction) and LIVENESS — the newest `field_services` observation inside `repin::LIVENESS_WINDOW` (24h, not the retention window), which the CLI renders as the cutover discontinuity: a repin translates HISTORY, so under `--dialect syslog` a historical `3` becomes 17 while the next live `3` conforms as OTel 3, and the live half belongs to `[ingest] severity_from`
- **the degraded-pin analyzer and the incomplete-results notice (ADR-0011 slice C1, #69)**: the catalog now says when a pin is doing sustained damage. evidence gained two durable halves, both written in `record_conflicts`' ONE transaction: ≤5 distinct misfit SAMPLES per conflict row (`field_conflicts.samples`, migration 0008 — captured by compaction at null-time in the same phase as the tally, the only moment the value is in hand; ≤256 BYTES cut on a char boundary in Rust because DuckDB's `left()` counts CHARACTERS (probed), control chars → U+FFFD at capture, deduplicated AFTER both transforms, sampled only for columns that actually conflicted and never logged), and per-`(field, service)` aggregates (`field_conflict_stats`, migration 0009 — first_at/last_at/episodes/rows_nulled_total, upserted with a GROUP BY INSIDE the statement because the boot pass sends many rows per pair and postgres refuses to touch an upsert row twice). the aggregates exist because the 100-row recency trim evicts exactly the history a span-based verdict needs. the ANALYZER (`trawl-server/src/catalog/analyzer.rs`) is a pure read-time function — no daemon, no verdict table: degraded = span ≥ 24h AND (≥100 rows shelved OR ≥3 episodes), with NO multi-sender gate (ruling 3: one nginx sending `status="accepted"` for a week IS the case), and `suggested_target` reads the misfit `observed_type`s through `normalize_duckdb_type` (never `from_duckdb`, which would drop INTEGER/HUGEINT/JSON) — one uniform rung that isn't the current pin, else VARCHAR. the verdict on the wire is FACTS ONLY (`DegradedVerdict`: since/services/episodes/rows_shelved/samples/suggested_to) and rides `/schema/fields` + `/schema/field` under the existing SchemaRead gate, composed in the handler by TWO page-keyed queries (never a join into the listing SQL — the same generic-plan trap `list_fields` documents). `rows_shelved` is the LIFETIME total and deliberately disagrees with the windowed `rows_nulled` beside it. `/api/v1/query` stamps `QueryResponse.degraded_fields` = the fields the query BOUND (`trawl_core::field_refs`, a pin-scope-BLIND AST walk — filters, `where`/`let` RHS, group-by, sort, projections, `rename` sources; bare-word search and time bounds bind nothing) ∩ an in-process set the schema-refresh tick reloads (a store error KEEPS the previous set — an empty one would silently un-badge the install). residuals: staleness bounded by one tick; SSE carries no notice; embedded `--data` has no catalog; a `let` target SHADOWING a degraded name over-collects. a successful repin CLEARS the field's evidence inside `finish_cutover`'s transaction — gated on that call being the one that completed the job, so a boot replay can't delete the evidence a forced lossy repin recorded after the flip. alert on `trawl_catalog_degraded_fields` RISING; the remedy is `trawl schema repin`, behind `schema_write` and a human, because the badge is sender-influenceable advisory signal
- **`| let` and `| rename` evaluate in PARALLEL, not sequentially (ADR-0011 slice A′)**: the in-memory lanes (SSE and the kv batch tail) now mirror the single projection the SQL lane emits (`COLUMNS(c -> c NOT IN (targets)), (expr) AS tgt, …`) instead of applying assignments one at a time. a `let` target naming a column the row ALREADY carries is invisible to its siblings — `let a = 1, b = a` and `let a = a + 1, b = a` give `b` the ORIGINAL `a` — while a target the row does NOT carry is DuckDB's lateral column alias, so `let ms = 1000, total = ms * 2` answers 2000 in every lane. which column a name binds is DuckDB's own case-INSENSITIVE rule (`bind_event_key`), applied to every pipeline field read whether or not the field is pinned, so a reference doesn't change meaning with the pin. pins never follow an alias (`let a = status, b = a` leaves `b` unpinned). residual: a column the CORPUS carries but THIS row leaves absent is a NULL column read in batch, while the live lane, seeing no key, binds the alias
- **live tail matches in SQL's three-valued logic (ADR-0011 slice A)**: `CompiledFilter` answers `Truth` — true / false / UNKNOWN — and UNKNOWN propagates through `NOT`/`AND`/`OR` by SQL's rules, so only a final true is a match. collapsing a missing field to *false* at the leaf survives a top-level filter but INVERTS under `NOT`. two live-tail behaviors flip, both toward the batch answer `/api/v1/query` has always given: `f!=x` now MATCHES events carrying no `f` (its emitted form is `("f" != ? OR "f" IS NULL)`, the one total comparison — on a sparse custom field that is most of the firehose, pair with `f=*`), and `NOT f=x` — plus `NOT <bare term>` — NO LONGER matches them (`NOT (NULL)` is NULL, filtered out), so a live alert written `NOT f=x` to catch events MISSING `f` stops firing and must be rewritten `f!=x`
- **`now()` is ONE instant per unit of output (ADR-0017 §3, #106)**: no evaluation path reads the clock — `context::EvalContext` (trawl-core/src/context.rs) is the ONE `Utc::now()` in the crate, truncated to DuckDB's microsecond TIMESTAMP domain at construction, and `eval`/`filter`/`stream` take it as a mandatory parameter. BATCH captures once per logical query at the engine's lane head: the emitter binds it as `CAST(? AS TIMESTAMP)` (so `typeof(now())` is `TIMESTAMP`, not the driver's inference or SQL's own TIMESTAMPTZ `now()`) and it rides out on `EmittedQuery.anchor`, which the `_raw`-free retry, the hot-only cold-start fallback and the `rust_stages` tail behind `extract kv` INHERIT rather than re-sample. LIVE samples per EVENT in one door — `stream::accept_event`, which takes the filter's `last=` window and every pipeline stage under one `ctx`, closing the old per-bus-batch/per-event split — while an aggregate snapshot's post-stage rows share one instant carried by the distinct type `stream::SnapshotContext` (a type, not a second `EvalContext` parameter, so the two boundaries cannot be passed for each other). the batch search stage's `last=` window deliberately stays on DuckDB's own statement clock (a second clock domain, microseconds away). guarded by `trawl-core/tests/now_anchor_contract.rs` (source walk: `Utc::now(` outside context.rs is a failure, comment lines filtered so the prose that explains the rule doesn't trip it) and by the scalar parity harness's `now` family, where ONE anchor reaches both lanes
- **the two namespaces, and the declared event envelope (ADR-0009 as reshaped by ADR-0013)**: one sentence — **bare names are sender vocabulary trawl NEVER assigns meaning to; underscore names are trawl's contract slots**. the envelope is TEN fields: `_time`, `_ingested`, `_raw`, `_repairs`, `_severity`, `_producer` (trawl-owned) + `env`, `service`, `host`, `message` (sender-asserted: the sender is the authority on those values, so bare is principled). `_producer` (ADR-0013 slice 2 ruling 6) names the DOOR — `http`|`syslog`|`trawld`, server-stamped from the profile and unforgeable, since an incoming `_producer` strips to a bare `producer` first. `severity` LEFT the envelope and is ordinary sender data; `severity_text` is not an envelope field either, though it stays in the packaged `DEFAULT_SEVERITY_FROM` list (`severity`, `severity_text`, `level`) as a derivation source. the whole `_` prefix is sealed by a PREDICATE (`schema::is_reserved_name`), not a list, so the envelope can grow without colliding with standing data — and the SAME predicate guards both doors. the canonicalizer (`trawl-server/src/ingest/envelope.rs`) captures `_raw` FIRST (client string honoured verbatim, else pre-repair serialization), then runs the ONE reserved-prefix strip: a non-proposable `_x` (proposable = `_time` always, `_raw` when a string) has its leading underscore RUN stripped and lands under the bare remainder — `_HOSTNAME`→`hostname`, `__name__`→`name__`, a forged `_severity`→`severity`, `_ingested`/`_repairs`/`_trawl_wal_file`/a non-string `_raw` all through it — with `field.reserved_prefix`; a bare name the SAME event carries wins (`field.reserved_prefix_collision`, first-in-map-order among prefixed claimants) and an empty remainder (`_`, `___`) is dropped, value still in `_raw`. that one rule replaced `RESERVED_CLIENT_FIELDS`, `meta.stripped`, the non-string-`_raw` case and the silent `_trawl_wal_file` removal. then it stamps `_ingested`, defaults-or-rejects `env`, peer-fills `host` (rejects behind a `trusted_relays` peer), and DERIVES the two `_` slots READ-ONLY off CONFIGURED source lists (`[ingest] time_from`/`severity_from`, ADR-0013 slice 2 ruling 5 — boot-fatal validation, forward-only on change, a profile's fixed sources prepended and not configurable; the packaged defaults are the old consts): `_time` ← first PRESENT of `_time`/`timestamp`/`@timestamp` (only `_time`, the proposal slot, is consumed and canonicalized — an unparseable one falls to arrival rather than reaching past itself), `_severity` ← first MAPPABLE of `severity`/`severity_text`/`level`. every source is stored VERBATIM under its own name, so `{"service":"game","level":"gold"}` keeps a queryable `level` column, gets no `_severity`, and is repaired in no way — `severity.unmapped` is DELETED (a derivation into the `_` namespace touches nothing sender-visible, so there is nothing to confess) and the ops signal is `trawl_severity_unmapped_total{service}`. a numeric source maps STRICTLY as OTel 1-24 (`3` is trace, `0`/`25` nothing) unless its source entry DECLARES `dialect = "syslog"`: the ranges overlap, so no value-shape rule can tell the dialects apart, and provenance — asserted in config, not by a privileged writer — is what licenses the inversion, which is why the syslog profile's FIXED source reads `syslog_severity` that way and a syslog-over-HTTP forwarder reaches the same answer by configuring the same entry. repairs are a closed enum recorded in `_repairs` + `trawl_ingest_repairs_total{code,service}`. principle: repair when the server has an honest answer, reject (typed reason) when it would guess — `service` missing/non-string/bad-charset and unlisted `env` reject. custom FIELD NAMES are policed here too, per-field and never per-event: every name is ASCII-lowercased FIRST (`field.name_case_folded` when it changed something — DuckDB identifiers are ASCII case-insensitive, so `Dur` IS `dur`; an in-event collision after folding keeps one value, exact spelling wins else the lexicographically-first variant, loser dropped with `field.name_case_collision`), and >255 bytes (`MAX_FIELD_NAME_BYTES`, too long to be a catalog btree key) drops the field with `field.name_too_long` — the rest of the event lands, and both the name and its value stay findable in `_raw`
- **timestamps are canonicalized at ingest, never fatal downstream** (ADR-0008): a valid `_time` input (RFC 3339, ISO 8601 basic offsets like `+0530`/`+02`, offset-less date-times read as UTC, `T` or space separator, `YYYY-MM-DD` or `YYYY/MM/DD`, optional seconds/fraction, or a bare date at midnight UTC) is rewritten to RFC 3339 UTC microseconds; anything else is **substituted, not rejected** — `_time` becomes the arrival time with the `time.from_ingest` repair code (the original is findable in `_raw`). parseable-but-implausible values (>10y past / >1d future) are kept and flagged `time.out_of_range`. the partition key is never hard-CAST: compaction resolves `_time` (and `_ingested`, same ladder) through `COALESCE(TRY_CAST(raw), ingest instant from the row's own WAL filename, compaction instant)` and the hot/cold union `TRY_CAST`s both TIMESTAMP columns on the hot side, so one bad value can never wedge a batch or throw a union
- **storage layout (ADR-0009)**: `data/{env}/{date}/{HH}/{service}.parquet` (daily rollup: `data/{env}/{date}/{service}.parquet`), WAL under `wal/{env}/`. path encoding is injective by validation — env charset `[a-z0-9_-]{1,32}` (`wal`/`scheduled` reserved), service charset `[A-Za-z0-9._-]` (no spaces, no leading dot, ≤128 bytes), names written VERBATIM (no sanitizer; `api.v2` ≠ `api_v2`). `data/EPOCH` (content `3` since ADR-0013) marks the layout; boot runs the restartable cutover table in `trawl-server/src/epoch.rs` — a marker-less epoch-1 root → `data.pre-schema-v2/`, an epoch-2 root → `data.pre-epoch-3/` (a SECOND name, because trawl never deletes a set-aside and an existing epoch-1 one must not be clobbered), and a query-only node warns and serves an epoch-2 root rather than moving data it does not own. since ADR-0011 slice B the data root also has a PLACEMENT requirement: it must sit inside its volume, never BE the mount point, and the whole corpus must live on that one filesystem (no tiered mount under an env/date/hour subtree) — a repin stages `data.repin-next/`/`data.repin-aside/` as SIBLINGS, and neither the hardlinks nor the per-env swap renames can cross a filesystem boundary. both packaged layouts already do this (volume `/var/lib/trawl`, data `/var/lib/trawl/data`)
- **DSL severity rides the SEVERITY pin, and the DSL has ZERO aliases (ADR-0013)**: `level`, `timestamp` and `@timestamp` are ordinary field references in EVERY position — including the ones that used to be errors (`stats count() by level`, `table level`, `where level in (…)`) — `catalog_key` is an ASCII fold and nothing else, and `quote_field` is verbatim: the name you type is the column in DESCRIBE is the identifier in the SQL, in all four lanes. severity moved onto `_severity`, which nothing can shadow, via a new `CanonicalType::Severity` (physically BIGINT — `as_duckdb` stays PHYSICAL and is no longer injective, while `as_catalog`/`from_catalog` carry the persisted/wire spelling `SEVERITY`; `normalize_duckdb_type` can never yield it, so INFERENCE can never install it — while an OPERATOR can, through the catalog door: `repin --to severity` is admitted by `from_catalog` since #79, and the envelope is refused by the `is_contract_typed` PREDICATE instead of an `ENVELOPE_TYPES` scan). its conform rung is the guarded BIGINT cast inside a 1-24 ladder guard, so a stored value always has a token rendering. the pin types the comparison through the ADR-0011 rule table, so all four lanes agree by construction: `_severity=error` → `BETWEEN 17 AND 20` (equality/IN take the BAND), `_severity>=warn` → `>= 13` (ordered takes the token's exact number), `_severity=error2` → exactly 18 (OTel exact short names, any operator), `_severity=17` → exactly 17 (integers bind unclamped, so `_severity>0` is "carries a severity at all"), and glob/regex match the canonical token text (`conform::severity_token_text_sql` / `severity::otel_name`, INJECTIVE over 1-24 — a band rendering would make `_severity=error2` filter rows it displays as `error`). an unknown value is a `CompareError::UnknownSeverityToken` naming the vocabulary, in every lane with the same sentence, never a filter that quietly matches nothing; the search stage's `!=` widens with OR-IS-NULL while the pipeline stays Strict, exactly as slice A′ requires. results DISPLAY the token (`error`, not `17`) in the CLI table, the TUI and the web UI, while `-f json`/`-f csv`/SSE keep the NUMBER — rendering is presentation only. embedded `--data` is pin-blind, so compare the ladder number there. a retired spelling earns NO advisory of any kind (ADR-0013 §7, ruled out at #60 review): `level` is sender vocabulary like any other bare name, a query naming it answers with rows or with an empty success exactly as any unwritten field does, and a notice keyed on its shape would be trawl assigning meaning to a bare name again. bare text search covers `message` OR `_raw`. token table, OTel exact-name tables and syslog inversion live in `trawl-core/src/severity.rs`; envelope consts and `is_reserved_name` in `trawl-core/src/schema.rs`
- **a cold-data drop is never a silent 200** (ADR-0008): the gate is LANE-INDEPENDENT — `run_query`, `run_query_with_hot`, `export_parquet` and `export_parquet_with_hot` all classify their outcome and route it through the one `cold_action` table, so a lane without a hot buffer answers exactly as its hot twin does (#73: `run_query`/`export_parquet` had no gate at all, and an empty hot buffer takes the no-hot path — `hb.snapshot()` returns `None` — so an idle minute was enough to make the everyday `service=X last=Nh` list-source miss a silent empty 200). hot-only fallback is permitted only where it cannot hide cold data — a genuine cold start (glob matches no parquet) or a missing-column user error. any other database failure with cold files present returns an error — including a union type conflict, which post-catalog can only mean foreign/nonconformant parquet (DuckDB reports an irreconcilable schema and an unconvertible value identically as `Conversion`-class, so no read-time classifier could tell them apart; the write-time invariant makes one unnecessary)
- **reserved field names are a PREDICATE, not a list**: `schema::is_reserved_name(name) = name.starts_with('_')`, enforced at both doors (ADR-0013 §5). ingest strips the prefix and keeps the data (above); the pipeline may not MINT one — `let _foo = 1` and `rename x as _foo` are PARSE errors (`parser/pipe.rs::assignment_target`) and an `extract` capture group named `_foo` — the one write position whose name comes from a regex, not the grammar — is refused where each lane compiles that regex: `emitter/validate.rs` for the SQL lane, `stream.rs::compile_extract` for the SSE lane, which never runs `validate_pipeline` (the kv arm's precedent — both lanes, one predicate, `rejects_minting_reserved_names` asserts the pair). case-variants cannot exist past ingest: names are ASCII-lowercased at canonicalization, so a lone `_Time` IS the `_time` proposal, `_Ingested` strips as `_ingested` → bare `ingested`, and a variant next to the exact spelling loses the collision — the canonical value is unforgeable
- **live streaming**: SSE endpoint uses `CompiledFilter` (in-memory DSL matcher) against the event bus for real-time event delivery, with back-pressure notifications via `StreamEvent::Lagged`. the filter is compiled with the catalog's pin snapshot and evaluates three-valued, so a streamed query and its batch form agree event for event (ADR-0011 slice A); the same snapshot roots the pipeline plan's `PinScope`, so `| where`/`| let` agree too (slice A′)
- **self-telemetry is trawld observing trawld, durable before it is visible**: when `[ingest] internal_telemetry` is on, a tracing layer turns the daemon's own events into ordinary `service=trawld` records — canonicalized through the ONE door under the `trawld` profile (ordinary sender vocabulary, no `trawld_` prefix; a tracing field named `service`/`env`/`host` loses to the profile's assertion with `field.producer_asserted`, value still in `_raw`) — through the ingest WAL. each flush stages one batch and writes it on Tokio's BLOCKING pool (both fsync barriers off the async executor); only after the write succeeds does the batch reach the hot buffer and SSE — exactly once, so a query can never see telemetry a restart would erase. an ordinary failed write RETAINS its batch on a FIFO retry queue and drains oldest-first, coalescing consecutive queued batches into units of ≤4 MiB (one WAL file, one `batch_id`) so an hours-long outage recovers in writes proportional to queued bytes, not one per flush tick — and nothing merges until a write has actually succeeded; a panicked or cancelled blocking write has consumed its batch, so that batch is counted exactly once as dropped under `reason="write_crashed"` while the write-failure counter still increments once. `[ingest] telemetry_buffer_max_bytes` (default 16 MiB) is ONE estimated-charge budget over the active buffer + the queue + the batch in flight, enforced as events ARRIVE (a wedged `spawn_blocking` write would otherwise let the active buffer grow unbounded); over budget the oldest queued batches shed first, then the incoming event, all exactly accounted in `trawl_telemetry_{events,bytes}_dropped_total{reason="preinit_cap"|"buffer_cap"|"write_crashed"}` beside `trawl_telemetry_wal_write_failures_total` and the `trawl_telemetry_buffer_{events,bytes}` depth gauges — scrapeable precisely while self-ingestion is down. shutdown is budgeted end to end (periodic flush raced against the signal, 5s final drain, 10s `runtime.shutdown_timeout`) so a frozen volume costs a parked blocking thread, never a stalled restart. the boundary is narrow and deliberate: trawld ONLY — `trawl-web` and the CLIs are stdout-only, fleet key mutations are a coalescing 30s poll (not a transactional ledger), and there is no OTLP export
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
trawl schema fields                              # pinned fields + types + conflict counts + `degraded` (limit 500)
trawl schema fields --service nginx --last 7d    # scoped + windowed (aggregates scope too)
trawl schema field duration                      # one field: pin, services (paged), conflicts, degraded case file
trawl schema field duration --limit 500 --after '<cursor>'   # next page of observations
trawl schema conflicts --last 7d                 # schema-health view (limit 100, max 1000)
trawl schema fields --data 'data/**/*.parquet'   # embedded DESCRIBE: names + physical types only
trawl schema repin status --to varchar --dry-run # repin plan (schema_write; --yes to execute, --force for lossy, --wait to poll)
trawl schema repin-status                        # running/last repin job
```
`fields`/`conflicts` honour `-f table|json|csv`; catalog metadata needs a server (routes are `schema_read`-gated). a degraded pin renders its case file under `schema field` (facts, sample shelved values, the `repin --dry-run` line), and `trawl query -f table` appends one `note: results may be incomplete` footer when the query bound one — json/csv carry `degraded_fields` on the wire instead. with `--output` the file is the deliverable and the footer goes to stderr.

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
trawl query "_severity>=error last=1h | stats count() by service | sort -count | head 10"

# same query against dev server
trawl query -p dev "_severity>=error last=1h | stats count() by service | sort -count | head 10"

# browse all data (table output)
trawl query -f table "* | head 5 | fields _time, host, service, _severity, message"

# pipe JSON to jq for ad-hoc processing
trawl query "last=1h | stats count() by service" | jq '.service'

# export to CSV file
trawl query -f csv "last=24h | stats count() by service, _severity" > report.csv

# validate a query without executing
trawl validate "_severity>=error | stats count() by host"

# query local parquet files (no server needed)
trawl query --data 'data/**/*.parquet' "* | stats count() by service | sort -count"
```

### HTTP API

the trawl server exposes a REST API. all routes under `/api/v1` except `/health` and `/ingest` require bearer token auth:

| method | path | description |
|--------|------|-------------|
| `GET` | `/api/v1/health` | health check (unauthenticated) |
| `POST` | `/api/v1/query` | execute a DSL query (carries `degraded_fields` when non-empty) |
| `POST` | `/api/v1/validate` | validate DSL syntax |
| `GET` | `/api/v1/schema` | column names + types from the field catalog (`?service=`, `?all=true`) |
| `GET` | `/api/v1/schema/services` | rich per-service schema (footer stats, catalog types) |
| `GET` | `/api/v1/schema/fields` | pinned-field listing with aggregates + catalog fill |
| `GET` | `/api/v1/schema/field?name=` | one field's pin, observations, conflicts (404 unpinned) |
| `GET` | `/api/v1/schema/conflicts` | recent type conflicts (`?field=`, `?service=`, `?since_secs=`) |
| `POST` | `/api/v1/schema/repin` | repin a field (dry-run/execute; `schema_write`) |
| `GET` | `/api/v1/schema/repin/status` | running/last repin job (`schema_read`) |
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

**pinned comparison semantics (server only)** — a filter binds by the field's catalog pin, not by the literal: against a VARCHAR pin `=`/`!=`/IN compare as text (a numeric literal also matching any spelling of the same number — `"200"`, `"0200"`, `"200.0"`) while `status>=400` compares numerically in `DECIMAL(38,6)` on both sides — exact for every i64, no reading for `nan`/`inf`/≥10^32 (values without one quietly don't match) — and glob/regex over a numeric/boolean/timestamp pin matches the STORED value's text (BIGINT decimal, lowercase `true`/`false` and only genuine ones, DuckDB's double rendering `200.0`/`1e-07`, RFC 3339 UTC micros of the zone-aware instant). the same rules cover BARE field-vs-literal comparisons in `| where`/`| let` (ADR-0011 slice A′) — with the pin followed through the pipeline (`rename` remaps, a computed `let` unpins, `stats` keeps group-by keys) and `!=` keeping plain SQL null propagation instead of the search stage's null widening. embedded `--data` stays literal-driven. a field an event doesn't carry is NULL → UNKNOWN everywhere, batch and live: in the search stage `f!=x` MATCHES those events, `NOT f=x` does NOT. full table in `docs/src/content/docs/reference/dsl.md`.

**severity (`_severity`, the derived slot — `level` is ordinary sender data; `sev(x)` reads any field on the same ladder)** — the SEVERITY pin gives `_severity` its vocabulary: `_severity=error` → `BETWEEN 17 AND 20`, `_severity>=warn` → `>= 13`, `_severity=warn,error` → either band, `_severity=error2` → exactly 18, `_severity=17` → exactly 17, `_severity=warn*` globs the canonical token text (13-16). band tokens: `trace`/`t`, `debug`/`d`, `info`/`i`, `notice`, `warn`/`warning`/`w`, `error`/`err`/`e`, `fatal`/`critical`/`crit`/`f`, `alert`, `emerg`/`panic`; plus OTel's exact short names (`trace2`…`fatal4`). anything else is a query error naming the vocabulary. `where _severity == "error"` works and matches live tail (SSE) exactly. `level=error` is a VERBATIM comparison on the sender's own field — no alias, and no advisory pointing here either (ADR-0013 §7); to read that field ON the ladder, `| where sev(level) >= "error"`.

**no aliases** — `timestamp` and `@timestamp` are ordinary sender fields, READ as `_time` derivation sources at ingest and stored verbatim under their own names. Only `_time` is `_time`.

**backticks** — any field name can be written between backticks and a backticked name is ALWAYS a field reference, in every field position (`` `http-status`=500 ``, ``| table `request id` ``, ``| stats count() as `total count` by `where` ``). Content is any character but a backtick, a doubled backtick escapes one, empty/control-char names are parse errors; the ASCII fold and the sealed `_` namespace still apply (`` `Dur` `` IS `dur`; ``let `_foo` = 1`` is the same error as bare). Function, stage and saved-query names take NO backticks. The closed keyword set is THREE — `last=`, `earliest=`, `latest=` — and backticks reach fields of those names. A backtick ENDS every unquoted position — a bare value and a bare word alike — so a tick reaches text only double-quoted (`` host="a`b" ``): backticks are not value quotes (`` service=`nginx` `` is a parse error, not a filter on the literal ticks), `-` cannot negate a quoted name (``NOT `http-status`=500`` is the spelling), and `` host=a+`b#c` `` errors rather than absorbing the tick. Nothing outside the grammar reads a backtick any more: the pre-parse comment scanner is DELETED (ADR-0014), so `` | sort -`a#b` `` and `` | let x = 1+`a#b` `` reach their names because a quoted name is a production, not a scanner state.

**output names must be unique** — an aggregating stage's projected set (group keys, timechart's `_time` bucket, top/rare's minted `count`, one name per aggregate) may not carry two columns of one name after folding: `stats count() by count`, `stats count() as n, sum(x) as N`, `top 5 count` are errors naming both producers, in batch and live alike. `eventstats` REQUIRES an explicit `as`. an un-aliased aggregate names its argument's INNERMOST field in every lane (`avg(tonumber(rssi) * -1)` → `avg_rssi`), and the in-memory lanes (SSE, and the kv batch tail that compiles through the same plan) — whose accumulators read a bare field, not an expression — REFUSE a computed argument rather than answering NULL under that name.

**comments** — `#` to end of line, and `#` ONLY (ADR-0014). A comment opens where the grammar is between tokens: start of input or after whitespace. Inside an unquoted token a `#` is a parse error spanning the byte with a hint to quote (`foo#bar`, `color=#ff0000`, `a=1# note`); quoted values, backticked names and regex bodies carry it verbatim, since none has a whitespace-skip site inside. `//` is not a comment, so a value carries it freely (`url=https://example.com/x`, `path=/api//v1`) — the loud half is a bare TERM starting with `//`, a parse error naming `#`. The 84 padding call sites (19 + 60 + 5 across expr.rs, pipe.rs, search.rs) go through ONE owner, `parser::comment::Spaced::spaced`, with `chumsky::Parser::padded` in clippy's `disallowed_methods` so a missed site fails the build; `parser::scan::scan_outside_quotes` is the one text-level walk that may change an answer (the web UI's date-range span), and the two TUI walkers are documented display-only.

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
a bare field-vs-literal comparison here is pin-aware on the server (see above); `where status == "400"` is `where status == 400`.

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

**tail** — last N rows (defaults to `_time` DESC if no prior sort)
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
eval status_class = floor(status / 100) # SPL alias for let
let is_error = status >= 400
let ms = 1000, total = ms * 2    # a target the row lacks IS a lateral alias (total = 2000)
let a = a + 1, b = a             # a target the row HAS is invisible to siblings (b = original a)
```
assignments in one `let` evaluate in parallel, as the SQL does; a computed target loses the source field's catalog pin (a bare alias `let s2 = status` keeps it).

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

**field refs**: `host`, `host.name`, `_time`, `@timestamp` (a plain sender name — the `@` is spelling, not an alias)

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
- `sev(x[, dialect])` — the query-time severity reading: `sev(level)>="error"` reads ANY field on the OTel ladder through the SAME kernel ingest derives `_severity` with (band tokens, exact short names, strict integer; no reading → NULL, never an error). It DECLARES its result `SEVERITY`, so the comparison takes the pin rules (band for `==`/`in`, exact number when ordered, canonical token text for patterns), a `let` target adopts the pin, and the column renders tokens. Literals must be QUOTED (a bare `error` is a field ref). `dialect` is a literal `"otel"` (default) or `"syslog"` and governs NUMERICS only — syslog inverts 0-7; anything else is a query error naming the vocabulary, in all lanes. Works under embedded `--data`: the pin is the function's, not the catalog's

### example queries

```
# errors in the last hour by service
_severity>=error last=1h | stats count() by service | sort -count

# slow requests by endpoint
status=200 last=24h | where duration > 1000 | stats avg(duration) by uri | sort -avg_duration | head 10

# 4xx/5xx rate by host
status>=400 last=2h | stats count() by host, status | where count > 10

# extract IPs and count
"connection from" | rex "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message | stats count() by ip | sort -count

# time series of error rate
_severity>=error | timechart span=5m count() by service

# dedup flapping alerts
service=monitoring | dedup host, alert_name

# pivot status codes by host
last=1h | pivot count() on status by host

# last 5 events with renamed columns
* | rename service as svc, host as hostname | tail 5

# conditional field + null handling
* | eval msg_len = if(isnotnull(message), length(message), 0) | fields host, msg_len | head 10

# distinct values per group
* | stats values(_severity), first(message) by service | head 10
```
