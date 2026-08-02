# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- **Fleet-owned local development controller (`fleet-dev`, ADR-0010, #54).** A new Rust controller replaces Trawl's hand-written launcher with one convention-heavy path for localhost/Docker and CNPG/Tailscale development: strict versioned manifests and machine profiles, redacted pure plans, non-mutating doctor checks, demand-driven 1Password service-account resolution, private per-process `mprocs` configuration, a dedicated persistent loopback-only PG18/pgvector provider, application-owned migrations, provider-scoped Fleet developer credentials, shared development SSO settings, explicit conflict-safe Tailscale Serve setup, signal/exit propagation, and single-stack locking. `bin/dev` remains as a thin legacy flag translator; Coastwatch adoption follows in its companion issue.

### Changed
- **BREAKING — roles-as-data RBAC cutover (ADR-0006 slice 1, #44).** fleet-auth's static `(key, app, role)` grant model is replaced by data-defined roles: `roles` / `role_permissions` / `key_roles` tables plus a warn-only `app_permissions` vocabulary registry. A role is a named, cross-app bundle of permission strings with an optional `rate_rpm` ceiling; keys hold any number of roles and effective permissions are the union — tiers can now be reshaped with `fleet-admin roles` without a deploy. The migration converts every legacy grant in place to a role named `<app>-<role>` (`trawl-admin`, `coastwatch-analyst`, …) carrying the permission set the owning app hardcoded (trawl's minus the dead `key_manage`), links keys via `key_roles`, seeds trawl's vocabulary, and **drops `api_key_role_assignment` irreversibly** — existing tokens keep working with unchanged capability, but the migration is the runtime cut point for every app on the shared fleet database (coastwatch's companion arc must ship in the same window; see the cutover runbook). Wire changes: `/whoami` replaces `assignments` with `roles: [names]` (permissions unchanged: recognized trawl permissions in canonical order); trawl-web's `/login` and `/me` replace `role: String` with `roles` + `permissions` and gate on "≥1 resolved trawl permission"; active-query/log `role` labels become the comma-joined role-name list. `fleet-admin` grows `roles create/list/show/add-perm/remove-perm/set-rate/delete` (`set-rate <NAME> --rate-rpm N | --default` re-tiers or clears a role's ceiling in place, keeping its bundle and key assignments; delete refuses while keys hold the role unless `--force`; unknown permissions warn on stderr but persist) and `keys create --role` / `assign-role` / `unassign-role` replace `--grant` / `grant` / `revoke-grant`. Rate limiting: a role `rate_rpm` (max across the key's roles) **overrides** the route-class defaults, applied independently per class. The SPA's admin gating re-keys from the role name onto the `server_manage` permission.
- **BREAKING — rate limiting is per API key, not per role (ADR-0006 slice 0, #43).** Every key now gets an independent token bucket keyed by its keystore id, so one noisy key can no longer starve others; the `[server.rate_limit]` per-role knobs (`admin`, `analyst`, `reader`, `ingest`) are replaced by two route-class knobs: `default_rpm` for the interactive API routes (default 100 — the loosest legacy *interactive* ceiling, so an upgrade never widens a reader key to the shipper-sized budget) and `ingest_rpm` for `/api/v1/ingest` (default 1000 — the legacy ingest ceiling, unchanged); `0` disables a class. `ingest_rpm` is earned by the `ingest` permission, not by the route: a key without it stays on its `default_rpm` bucket when it posts to `/api/v1/ingest`, so the shipper-sized ceiling never widens an unprivileged key on the heaviest endpoint. A config still carrying a per-role key fails validation at boot with a message naming the migration, and the helm chart fails at render if a values file still sets `rateLimit.admin`/`analyst`/`reader`/`ingest` (`rateLimit.defaultRpm` + `rateLimit.ingestRpm` replace them) — never a silent ignore of tuned quotas. 429 semantics and response shape are unchanged. Role-differentiated class-of-service is deliberately deferred to slice 1, where it returns as a `rate_rpm` role attribute; the `rate_limit_exceeded` log event now carries `key_id` instead of `role`.
- **Web UI reskin: Mira Blue design language (ADR-0007, #47).** fleet-ui and the trawl web UI move onto Basecoat Mira's geometry and neutral zero-chroma OKLCH surfaces with fleet's blue accent retained — a pure CSS/HTML pass (no component markup changes). Buttons drop to weight 500 with transparent borders, `color-mix` tint hovers, and a 1px translate press; destructive buttons become tinted (red text on a red wash) instead of solid; inputs and the DSL editor get translucent line-tint fills; focus is a 2px solid accent-tinted ring; radii are tokenized (`--radius-panel/ctl/sm/...`); button text reads the new `--on-accent` token, and dark mode keeps Mira's inversion (near-black text on the light-blue accent). Fonts swap from Open Sans / Fira Code to **Geist / Geist Mono** (still Google Fonts, self-hosting deferred), and `color-scheme` + `scrollbar-color` are declared so Firefox native widgets follow the theme. The `css_chrome_parity` golden fixture is re-captured at this baseline. coastwatch is unaffected until its next `TRAWL_REV` bump, where it runs its own adoption pass.
- **Web UI assets are precompressed and content-negotiated.** Release builds emit deterministic Brotli/gzip sidecars, enforce whole-SPA wire budgets (2.5 MiB Brotli, 4 MiB gzip), and bake every representation into `trawl-web`. Embedded and `TRAWL_WEB_SPA_DIR` delivery now negotiate `Accept-Encoding`, preserve the original MIME type, emit representation-specific ETags with conditional 304 support, and retain immutable caching for Trunk-hashed assets. `bin/dev --release-spa` makes remote/Tailscale iteration use optimized Wasm instead of the roughly 145 MiB debug module.

### Fixed
- **A malformed ingest timestamp can no longer wedge compaction or silently drop a service's parquet history (ADR-0008, #49).** Previously an event whose `timestamp` was present but not castable to `TIMESTAMP` (`"not-a-date"`, an out-of-range date, a nested object, a bare number) was accepted with HTTP 200 and then poisoned everything downstream: the batch hard-CAST failed the whole compaction chunk forever (WAL never drained, never quarantined, never reclaimed), and every hot+cold query on that service was misclassified as a schema conflict and silently degraded to hot-only — dropping the entire parquet history from results while still returning 200. Now: (1) ingest canonicalizes valid timestamps (RFC 3339; ISO 8601 basic offsets like `+0530`/`+02` as Java and Go encoders emit; offset-less date-times read as UTC; `T` or space separator, `YYYY-MM-DD` or `YYYY/MM/DD` dates, optional seconds and fraction; or a bare date at midnight UTC — surrounding whitespace trimmed) to RFC 3339 UTC microseconds, and substitutes malformed ones with the arrival time, preserving the original verbatim in a `timestamp_invalid` field (new `trawl_ingest_events_repaired_total` counter + `ingest_repairs` warn); (2) compaction never hard-CASTs the partition key — `TRY_CAST` falls back per-row to the ingest instant encoded in the row's own WAL filename (this drains WAL already wedged on disk from before the fix, no operator step), then to the compaction instant, so no parquet row ever carries a NULL timestamp; (3) the hot/cold union `TRY_CAST`s the hot side's timestamp; (4) error classification keys off DuckDB's error-class token instead of substring-sniffing, and the query outcome policy now returns an **error** whenever a database failure could hide existing cold data — including a read that matched no files while cold parquet exists on disk (a transient race, surfaced as a retryable 503 rather than an empty result) — a cold-data drop is never a silent HTTP 200 (hot-only fallback remains for genuine cold starts and missing-column user errors; parquet export routes through the same gate *and* through the same repairs the gate assumes ran first, so an export never hard-errors on a corpus a query answers). A columnless result is not accepted as proof of a cold start either: DuckDB rejects a whole list source when a *single* element matches no file, which happens routinely for a sparse-traffic service whose hour directories exist without its parquet in them, so the query is retried over just the elements that do match — the cold rows that exist are returned instead of being dropped in favour of hot-only. Note: rows drained from pre-fix WAL carry ingest time, not event time, and their original bad values are not preserved — only post-fix ingest preserves originals. The per-row WAL-filename provenance in (2) travels as a reserved `_trawl_wal_file` column, so that field name is now trawl's: ingest silently drops it from incoming events (the rest of the event is accepted unchanged), and WAL already carrying it still compacts, losslessly, under a renamed provenance column.
- **Web UI: the DSL editor's line-number gutter follows the theme.** CodeMirror ships an unconditional light base theme for the gutter rail, the active-line band, and the caret, and nothing installs a CM dark theme — so in dark mode the rail rendered as a near-white slab down the left of an otherwise near-black editor. `.cm-gutters` / `.cm-activeLine` / `.cm-content` are now tokenized (`--panel`, `--line`, `--ink-4`, `--fill`, `--ink`) like the rest of the editor frame — the rail repaints the frame's own resting surface, opaquely, since it is `position: sticky` over content that scrolls horizontally beneath it. Pre-existing since the editor landed, not a Mira Blue regression (#47).

## [0.4.0] - 2026-07-24

### Added
- **Live admin stats in the web UI footer**, fed by a new SSE endpoint. `GET /api/v1/dashboard/stream` (server) emits a cached dashboard snapshot as named `stats` events every 2s, `ServerManage`-gated and bounded by its own semaphore so footer connections never consume user stream slots; `trawl-web` proxies it as a first-class SSE pass-through. The web UI replaces the three stubbed footer groups with live hot-buffer, WAL-backlog, active-query, and uptime figures, opened only after `/me` resolves as admin (non-admins never issue the request) and cleared if the stream closes so the numbers never freeze stale.
- **Web UI: sortable table headers** across the schema and nets tables (click to sort, click again to flip), and a **compact/comfortable density toggle** in the statusbar that rescales facets, chips, histogram axes, tables, and the editor together via the fleet-ui dense type sub-scale.
- **Fleet SSO opt-in for the web UI**: a `[web] shared_domain` config knob (env-mirrored, documented for reverse proxies) sets the `fleet_session` cookie `Domain=` so a single sign-in is shared across fleet apps under a common parent domain. Surfaced in both channels — a commented `shared_domain` in the deb `trawld.toml` and `web.sharedDomain` in the helm chart — with an SSO provisioning runbook (ADR-0004 slice 2).
- `fleet-admin keys revoke-grant <prefix> <app>` and `fleet-admin keys retype <prefix> <kind>`, completing key-lifecycle parity with trawl-admin ahead of the trawld keystore cutover (ADR-0004 slice 0). `revoke-grant` follows the `keys revoke` confirmation convention (`--yes`/`-y`, `[y/N]` prompt, non-TTY refusal without `--yes`) and accepts `app` or `app:role` (role half ignored); a missing grant errors with `GrantNotFound` rather than silently succeeding. `retype` flips a key between `human` and `service` and refuses revoked keys (#35).

### Changed
- **BREAKING — app stores move to a dedicated `trawl` postgres database; `trawl-auth` crate deleted (ADR-0004 slice 3, #41, closes #12).** Query history, saved queries, schedules, and report runs leave the transitional sqlite file for a postgres database that trawld owns outright: it auto-migrates the schema at boot (sole writer) and holds a session advisory lock for its lifetime, so a second trawld against the same database fails startup instead of racing. New `[storage] database_url` config (`TRAWL_DATABASE_URL` env takes precedence, NO fallback to the auth URL); `[auth]`'s env override is renamed to `FLEET_DATABASE_URL` — the bare `DATABASE_URL` is no longer read by trawld (it belongs to fleet-admin and the sqlx test harness) — and `db_path` is rejected at config validation with a message naming this migration. No sqlite importer (hard-cutover doctrine): recreate saved queries and schedules per the cutover runbook. Wire shapes are unchanged, but duplicate-name and schedule-exists conflicts now return **409** (previously 400) per the store error table, the saved-query list is served by one bulk-join statement instead of `1 + 3n` lookups, and `/health` gains a timeout-bounded `storage_db` check (degraded ⇒ HTTP 200). Concurrency guards are real cross-connection guarantees now (partial unique index for the one-running rule, transactional `max_runs` claims, orphaned result files cleaned up when a run is deleted mid-flight), and a background task now detects loss of the sole-writer advisory lock (pg restart, idle-cull, `pg_terminate_backend`) and hard-exits rather than letting a second writer start split-brain. Helm requires `storage.database.existingSecret` (injected as `TRAWL_DATABASE_URL`) and injects the auth Secret as `FLEET_DATABASE_URL` (the Secret KEY name is unchanged); the deb's `trawld.toml` gains `[storage]`. Internally the workspace returns to the umbrella `sqlx` crate with `sqlx::migrate!()` and `#[sqlx::test]` — the rusqlite `links` collision, the hand-built migrator, and the PgFixture/`pg_test!`/`FLEET_TESTS_REQUIRED` test machinery are all gone (#12).
- **BREAKING — fleet-auth postgres keystore cutover (ADR-0004 slice 1, #36).** trawld now verifies API keys against fleet-auth's external postgres keystore and **will not start until that database is reachable and migrated** (`fleet-admin migrate`): `apt install` alone no longer yields a working server. `[auth]` gains `database_url` (the `DATABASE_URL` env var takes precedence); `auth_cache_ttl_secs` is gone (revocation is now checked per-request in postgres — no TTL window). This is a hard cutover with no data migration: every existing `flt_` token stops working and must be re-minted with `fleet-admin` (admin, CLI, and vector ingest keys), and schedules must be recreated — follow the cutover runbook in the docs. The legacy sqlite `auth.db` is quarantined in place (postgres and sqlite key ids are unrelated sequences; reusing the file would leak one principal's history/saved queries/auto-executing schedules to another) — trawld refuses to start while `db_path` still points at a file named `auth.db`; repoint it at a fresh `store.db`. Keys from other fleet apps now receive an opaque 403 on every authenticated route (previously `/whoami` leaked cross-app assignments to them). `trawl-admin keys` subcommands are removed (`tls` remains); key management lives in `fleet-admin`, which now ships in the docker image and the .deb. The helm chart requires `auth.database.existingSecret` (the DSN is injected as `DATABASE_URL` from a Secret, never rendered into the ConfigMap) and its init container now only runs `fleet-admin migrate` — per-install key auto-minting is gone.
- **BREAKING — `trawl-web` sessions move onto the shared fleet-auth `fleet_session` SSO cookie (ADR-0004 slice 2, #40).** The 444-line hand-rolled `session.rs` is deleted; the cookie's crypto (XChaCha20-Poly1305), builders, and origin validation now come from fleet-auth's `session` feature. The cookie is renamed to `fleet_session` (`SameSite=Lax`), carries an app-agnostic `{token, name, exp}` payload — the role leaves the cookie and `/me` re-derives it live from upstream `/whoami` each request, so grant changes take effect immediately — and its `Domain=` is driven by the new `[web] shared_domain` knob. Cookie-cleared-on-401 behaviour is now decided only by `/me` against the permission-free `/whoami` (a proxied 401 for a mere permission denial no longer signs the user out fleet-wide). Existing `trawl_session` cookies are invalidated by the rename and users must re-authenticate.
- **Web UI reskin: slate/blue design-workbench pass and shared fleet-ui chrome (ADR-0005, ADR-0002/ADR-0030).** The interface moves off the amber theme onto a cool slate/blue palette (accent `#2a5c8a` light / `#5a9fd4` dark) with a golden-ratio type rhythm, sentence-case labels, and Open Sans / Fira Code fonts. The whole chrome — shell, rail, topbar, modals, drawers, tabs, badges, status dots, sparklines, toasts, pagers, loading states — is rebuilt on the shared `fleet-ui` design system, bringing consistent overlay focus management: modals trap focus and restore it to the opener, drawers capture without trapping, `Escape` closes only the topmost overlay, and the overflow (`⋯`) menu is fully keyboard-navigable. The schema page is now a sortable services table (the card grid is gone). **BREAKING for fleet-ui consumers:** the `--amber*` design tokens and `.amber` class are renamed to `--accent*` / `.accent`.
- **MSRV raised 1.88 → 1.94** (required by sqlx 0.9). Only affects from-source builds — shipped binaries are built with a newer toolchain.

### Fixed
- **The date/time scalars now evaluate on the live-streaming path.** `tonumber`, `tostring`, `date_part`, `date_trunc`, `date_diff`, `strftime`, and `strptime` — previously handled only in the batch (DuckDB) path — are now implemented in the in-memory streaming evaluator too, so a `let`/`eval` using them computes correctly in live tail (SSE) instead of silently returning null/unknown. `EvalValue` gains a `Timestamp` variant (naive datetime, mirroring DuckDB's offset-discarding `AS TIMESTAMP` cast), enabling comparisons like `where timestamp > now()` in live tail without pre-parsing the field. A generative test pins both paths to identical results against real DuckDB (ADR-0001, #22, #24).
- **Streaming-vs-DuckDB scalar parity gaps closed** across the scalar functions, each a silent divergence between `/query` (batch) and live tail (SSE): `length()`/`len()` now counts characters, not UTF-8 bytes; `concat()` skips NULL args and casts the rest to text, matching DuckDB `CONCAT` (not `||`); `substr()` follows DuckDB's 1-based, character-counted, negative-start/length window semantics; float-to-string rendering is bit-for-bit `CAST(DOUBLE AS VARCHAR)`; `abs()`/unary-neg return NULL on `i64::MIN` overflow; `tonumber()` strips digit separators like `TRY_CAST`; invalid `strftime`/`strptime` format literals are now rejected up front in both paths (previously batch errored, streaming nulled); and partial `strptime` formats (year-only `%Y`, year-month `%Y-%m`, bare month-day `%m-%d`, date + incomplete time `%Y-%m-%d %H`) fill from the same `1900-01-01 00:00:00` base in both paths (#22, #24, #25).
- Web UI: results rows now re-render when the sort changes (the tbody was evaluated once at mount); the DSL editor renders the Fira Code stack instead of CodeMirror's injected generic `monospace`; histogram bars render in the accent blue with error segments in red.
- Git hooks are POSIX/Linux-portable (they previously assumed a macOS dev box: bash-only `source`, an unconditional `~/.cargo/env`, and `sysctl hw.ncpu` for the CPU count).

### Security
- **Cross-site logout / CSRF hardening in the session path (fleet-auth + trawl-web).** Login and logout now enforce present-only, strictly same-host `Origin` validation (an absent `Origin` still passes for curl/scripts), and the same guard is applied to every cookie-authed proxy mutation (`POST`/`PUT`/`DELETE` saved, schedule, queries, export) so a compromised or attacker-controlled sibling origin can no longer ride the `SameSite=Lax` shared cookie to forge a request or a fleet-wide sign-out. A shared cookie `Domain=` is explicitly not treated as an origin allowlist. Host derivation falls back to the HTTP/2 `:authority` pseudo-header so h2-terminated deployments are covered.
- **Remote-triggerable panics on the SSE streaming eval path closed.** An ordinary query over attacker-influenced event data could abort the client stream (and storm the log with backtraces): a non-char-boundary byte slice in `strip_offset` (reachable via `* | where timestamp > now()` against a ≥6-byte multi-byte field value), a chrono `Item::Error` panic from an invalid `strftime` format (`%Q`, a dangling `%`), and a multi-byte trailing char in the schedule interval parser are all fixed with checked slicing and up-front validation.
- Docs site: astro 6 → 7.1 + starlight 0.41 clears three XSS advisories and pulls svgo 4.0.2 (removeScripts bypass); the transitional `esbuild` override is retired (astro 7 resolves the patched 0.28.1 itself). `crossbeam-epoch` 0.9.18 → 0.9.20 (RUSTSEC-2026-0204).
- Packaging: the fleet-auth Postgres DSN conffiles (`/etc/trawl/trawld.toml`, `/etc/default/trawld`) install `0640 root:trawl` instead of world-readable `0644`, so an unprivileged local user can no longer read the keystore credential.

## [0.3.3] - 2026-06-22

### Added
- Release builds now publish Breakpad symbol files with GNU build-ids, so `trawld` minidumps (from the 0.3.2 crash-dump capture) symbolicate against the Rust frames and the statically linked libduckdb module. Symbols are harvested before the shipped binaries are stripped — keeping the binaries lean — and published to a `symbols/` store on gh-pages for `minidump-stackwalk --symbols-url`. The build asserts the GNU build-id survives stripping on both architectures, failing the release loudly rather than shipping binaries that could never be symbolicated.

### Fixed
- Eliminate a `trawld` crash-loop: the background schema-refresh job called DuckDB's `parquet_metadata()`, which can SIGSEGV on a worker thread (uncatchable) and take the whole daemon down. Per-column schema-browser statistics (null/min/max/compressed size and row counts) are now read directly from parquet footers in safe Rust, so a corrupt or mid-write file is logged and skipped — and retried on the next refresh — instead of crashing the daemon.

### Security
- Override the documentation site's transitive `esbuild` dependency to 0.28.1, clearing GHSA-g7r4-m6w7-qqqr (development-server arbitrary file read; low severity, does not affect the published static site).

## [0.3.2] - 2026-06-21

### Added
- Out-of-process crash-dump (minidump) capture for `trawld`. On a fatal signal (SIGSEGV/SIGABRT/SIGBUS) a re-exec'd monitor process writes a minidump before the process dies, turning an opaque exit-139 into a `.dmp` that `minidump-stackwalk` can symbolicate against the Rust frames and the libduckdb module. New `trawl-crashdump` crate (the one place `unsafe` is allowed in the workspace); opt-in via the Helm `crashDump` block (off by default, requires `CAP_SYS_PTRACE` on the trawld container only). Dumps are written owner-only (`0600`) since they contain raw process memory. Debian/systemd parity is tracked separately.

### Fixed
- Bump DuckDB 1.5.1 → 1.5.4 to pick up JSON/Parquet segfault and out-of-bounds hardening (upstream #21594, #21972, #21635, #23100) on the `read_json`/`read_parquet` paths trawld drives hardest during compaction and hot-buffer queries — the leading suspect for the recurring exit-139 crashes. Pinned a serde recursion-limit regression test so pathologically deep JSON keeps being rejected at ingest rather than reaching the recursive-descent parser.

## [0.3.1] - 2026-06-19

### Fixed
- Quarantine corrupt WAL `.ndjson` files (NUL-filled or truncated torn-write debris from a hard kill) instead of letting one bad file head-of-line-block a service's compaction forever; good files in the same batch still compact, and an all-corrupt batch is counted as data loss rather than retried indefinitely
- Per-file read isolation on WAL compaction: a malformed-but-textual file that slips past the byte sniff is isolated and quarantined rather than wedging the whole batch, so no corruption shape can stall compaction
- fsync WAL writes — data fsync before the rename, parent-directory fsync after — so a hard SIGKILL can no longer leave a full-length but NUL-filled `.ndjson` poison pill; the parent-dir fsync is best-effort so it can't falsely reject an already-durable write

## [0.3.0] - 2026-06-18

### Added
- Documentation site at [trawl.sh](https://trawl.sh) (Starlight/Astro)
- Fuzz testing for parser and emitter (cargo-fuzz + libfuzzer)
- MSRV check (Rust 1.88) in CI
- Configurable syslog channel capacity

### Fixed
- SQL injection in parquet export path (single-quote escaping)
- Gzip decompression bomb — capped at 10x wire size
- Parser panic on multi-byte unicode in error enrichment
- `unreachable!()` in export handler replaced with error return
- Timechart auto-bucket for >30 day ranges (was 1h, now 1d)
- Source list validation handles unspaced comma-separated paths
- Heal hot/cold and cross-file parquet **schema drift**: complex columns (STRUCT/JSON/array) are coerced to VARCHAR at compaction write time and symmetrically in the hot buffer, so a field that is an object in one batch and a plain string in another no longer drops cold/parquet rows at query time or wedges the daily rollup
- Quarantine corrupt/truncated parquet inputs (renamed `.corrupt`) instead of letting one bad file wedge the daily rollup forever; surface the resulting data loss on the compaction error counter
- Daily-rollup accounting hardening: count quarantined inputs even when the merge then hard-errors, count a wedged rollup recovery, and make hourly-file cleanup idempotent so a partially-completed recovery can't loop

### Changed
- Value types (`Value`, `QueryResult`, `SchemaColumn`) moved from trawl-engine to trawl-api
- DuckDB error message strings extracted into named constants
- **BREAKING**: trawl-auth is now a multi-app identity substrate (ADR-0021). API keys carry 0..N `(app, role)` grants in a new `api_key_role_assignment` table instead of a single flat `role` column. Existing v2 databases are auto-migrated; every pre-existing key gets backfilled with a single `("trawl", <old_role>)` grant and `kind = "human"`.
- **BREAKING**: `/api/v1/whoami` response shape now returns `{prefix, name, kind, assignments, permissions}` — the flat `role` field is gone (clients should read `assignments` and find the `"trawl"` entry).
- **BREAKING**: `trawl-admin keys create` replaces `--role <role>` with `--kind <human|service>` plus a repeatable `--grant <app:role>` flag. New subcommands: `keys grant`, `keys revoke-grant`, `keys retype`.

### Security
- Bump dependencies to clear advisories: `tar` 0.4.46 (RUSTSEC PAX desync, build-time), `rand` 0.8.6/0.9.3 (RUSTSEC unsoundness); docs toolchain `astro` 6.4.6 (SSRF/XSS), `vite` 7.3.5 (`fs.deny` bypass), `js-yaml` 4.2.0 (merge-key DoS)

## [0.2.0] - 2026-04-20

### Added
- `trawl-web` (browser session proxy + embedded leptos SPA) now ships by default in both distribution channels
- Debian: `trawl-server` .deb installs the `trawl-web` binary, a sandboxed `trawl-web.service` systemd unit, and `/etc/default/trawl-web`; `postinst` generates a persistent 32-byte session cookie key at `/var/lib/trawl/web.cookie`
- Helm chart: `trawl-web` runs as a sidecar container in the trawld StatefulSet pod (`web.enabled: true` by default) with a chart-managed cookie Secret that survives upgrades via `lookup`
- New `[web]` block in `trawld.toml` (`bind_addr`, `cookie_secret_path`, `session_ttl_secs`, `allow_insecure_cookies`) — shared config for trawld and trawl-web
- Release workflow now builds the SPA with `trunk` + `wasm-bindgen-cli` before `cargo zigbuild` and includes `trawl-web` in release tarballs, .deb packages, and the container image

### Changed
- Helm ingress now targets the trawl-web sidecar by default (`ingress.backend: web`, plain HTTP/8090) instead of trawld's raw HTTPS API. Set `ingress.backend: trawld` to restore the previous behavior for bearer-token API clients
- Container image exposes port 8090 (web UI) alongside 5514 and 1514
- API clients (CLI, `trawl-client`, vector log shippers) continue to talk to trawld on 5514 directly — the proxy only accepts cookie-authed traffic and blocks `/api/v1/ingest`

## [0.1.8] - 2026-03-08

### Added
- Native syslog TCP hardening: idle timeout, per-connection event limits, CIDR allowlist
- Admin dashboard tab in TUI
- `/api/v1/whoami` endpoint for token identity

### Fixed
- Cap structured data element extraction limits in syslog parser
- Accept bare IPs in syslog CIDR allowlist
- Run syslog WAL writes in `spawn_blocking`
- Handle oversized TCP syslog messages without silent splitting
- Prevent panic on multi-byte UTF-8 in `truncate_query`

### Changed
- Consolidate ingest pipeline into shared `PipelineWriter`
- Extract shared service validation to pipeline module

## [0.1.7] - 2026-03-05

### Added
- TUI redesign: horizontal tab bar, vim-style splash screen, status bar relocation
- Enhanced query error messages with "did you mean?" suggestions and real-time validation
- Native syslog listener for network appliances (UDP + TCP)
- `/api/v1/whoami` endpoint

### Fixed
- Prioritize error status over stale results in render dispatch
- Focus editor when loading query from history or saved tabs
- Show Running indicator and accept execute keys from results pane
- Use actual viewport height for results pane scrolling

## [0.1.6] - 2026-02-28

### Fixed
- Hot buffer visibility race — insert events synchronously during ingest, eliminating the window where freshly ingested events were invisible to queries
- Remove `mark_draining` to eliminate a second hot buffer event invisibility race

### Changed
- Field filter syntax changed from `:` to `=` (e.g. `service=nginx` instead of `service:nginx`)

### Added
- Named profiles and inline token support in CLI config

## [0.1.5] - 2026-02-25

### Fixed
- Add postinst script to create `trawl` system user before service start

## [0.1.4] - 2026-02-24

### Fixed
- Correct APT `Filename:` path prefix doubling in release workflow

## [0.1.3] - 2026-02-23

### Changed
- Sync deb package version from git tag in release workflow
- Rename server deb package from `trawld` to `trawl-server`

## [0.1.2] - 2026-02-21

### Added
- Modular Vector configs with debian drop-in architecture
- `workflow_dispatch` trigger for manual releases

### Fixed
- GPG armor header warning tolerance on secret import
- Set git identity before gh-pages init commit
- Pin apt sources to amd64 in cross-compile, add arm64 from ports

## [0.1.1] - 2026-02-18

### Changed
- Project renamed from "fleet" to "trawl" — all crate names, env vars, paths, configs updated

### Fixed
- Fast-hash feature gate to reduce argon2 cost in tests
- Force libduckdb-sys rebuild to survive cache cleaning
- Relocate auto-generated TLS certs and fix debian packaging
- Free disk space on CI runners before build

## [0.1.0] - 2026-02-15

### Added
- Initial release
- Pipeline-oriented DSL with 14 pipe stages and 17 aggregation functions
- Parquet storage with DuckDB query execution
- WAL → hourly parquet → daily rollup compaction pipeline
- Hot buffer for sub-millisecond event visibility
- SSE streaming with in-memory compiled filters
- argon2id API key authentication with four roles
- TLS with auto-generated self-signed certificates
- Prometheus metrics at `/metrics`
- Internal telemetry (server monitors itself via `service=trawld`)
- Interactive TUI with syntax highlighting and schema browser
- CLI with table, JSON, CSV, and parquet output formats
- Embedded mode for querying local parquet files without a server
- Scheduled queries with cron-style execution
- CI/CD with cross-compiled binaries, .deb packages, and APT repository

[Unreleased]: https://github.com/jakub/trawl/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/jakub/trawl/compare/v0.3.3...v0.4.0
[0.3.3]: https://github.com/jakub/trawl/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/jakub/trawl/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/jakub/trawl/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/jakub/trawl/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/jakub/trawl/compare/v0.1.8...v0.2.0
[0.1.8]: https://github.com/jakub/trawl/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/jakub/trawl/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/jakub/trawl/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/jakub/trawl/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/jakub/trawl/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/jakub/trawl/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/jakub/trawl/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/jakub/trawl/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/jakub/trawl/releases/tag/v0.1.0
