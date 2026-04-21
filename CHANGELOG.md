# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

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

### Changed
- Value types (`Value`, `QueryResult`, `SchemaColumn`) moved from trawl-engine to trawl-api
- DuckDB error message strings extracted into named constants

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

[Unreleased]: https://github.com/jakub/trawl/compare/v0.1.8...HEAD
[0.1.8]: https://github.com/jakub/trawl/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/jakub/trawl/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/jakub/trawl/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/jakub/trawl/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/jakub/trawl/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/jakub/trawl/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/jakub/trawl/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/jakub/trawl/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/jakub/trawl/releases/tag/v0.1.0
