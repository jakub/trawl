<p align="center">
  <a href="https://trawl.sh">
    <img src="docs/public/trawl.png" alt="trawl" height="150px">
  </a>
</p>
<h1 align="center">trawl</h1>
<p align="center">
Self-hosted log collection, storage, and search for homelabs and small-to-medium infrastructure. Splunk-like pipeline DSL, Parquet + DuckDB under the hood, zero licensing cost, single binary.
</p>
<p align="center">
  <a href="https://github.com/jakub/trawl/actions/workflows/ci.yml">
    <img src="https://github.com/jakub/trawl/actions/workflows/ci.yml/badge.svg" alt="trawl">
  </a>
  <a href="LICENSE">
    <img src="https://img.shields.io/badge/license-MPL--2.0-blue.svg" alt="trawl">
  </a>
</p>

## Install

```bash
# Debian/Ubuntu
curl -fsSL https://trawl.sh/gpg.key | sudo gpg --dearmor -o /usr/share/keyrings/trawl.gpg
echo "deb [signed-by=/usr/share/keyrings/trawl.gpg] https://trawl.sh/apt stable main" \
  | sudo tee /etc/apt/sources.list.d/trawl.list
sudo apt update && sudo apt install trawl trawld

# From GitHub releases
curl -fsSL https://github.com/jakub/trawl/releases/latest/download/trawl-v0.1.8-x86_64-unknown-linux-gnu.tar.gz \
  | tar xz
```

The `trawld` package also installs and enables `trawl-web.service`, the
browser-facing session proxy that serves the web UI on `127.0.0.1:8090`.
Put a TLS-terminating reverse proxy (caddy / nginx / traefik) in front if
you want external access.

## Quick example

```bash
# errors in the last hour by service
trawl query "level=error last=1h | stats count() by service | sort -count"

# slow requests with field extraction
trawl query "status=200 last=24h | where duration > 1000 | stats avg(duration) by uri | head 10"

# query local parquet files directly (no server needed)
trawl query --data 'data/**/*.parquet' "* | stats count() by service"

# launch the interactive TUI
trawl
```

## Documentation

Full documentation at **[trawl.sh](https://trawl.sh)**

- [Getting Started](https://trawl.sh/getting-started/)
- [Query Language (DSL)](https://trawl.sh/reference/dsl/)
- [CLI & TUI](https://trawl.sh/reference/cli/)
- [HTTP API](https://trawl.sh/reference/api/)
- [Configuration](https://trawl.sh/reference/configuration/)
- [Architecture](https://trawl.sh/architecture/overview/)

## Developing

Everything below was validated end-to-end on a fresh Linux checkout;
macOS works the same (the hooks and scripts are portable to both).

**Prerequisites:** stable Rust via [rustup](https://rustup.rs)
(`rust-toolchain.toml` pins the channel and components), Docker (dev
postgres), [cargo-nextest](https://nexte.st) (tests),
[trunk](https://trunkrs.dev) (web UI — it fetches the lockfile-matching
`wasm-bindgen` itself), and [lefthook](https://lefthook.dev) (git hooks).
The attached development stack also uses [mprocs](https://github.com/pvolok/mprocs).
Production-style builds bundle DuckDB and need a C/C++ toolchain
(`gcc`/`clang` + `cmake`). The canonical `bin/dev` flow uses the matching
prebuilt DuckDB shared library instead.

```bash
rustup target add wasm32-unknown-unknown   # web UI only
lefthook install                           # fmt/clippy pre-commit, tests pre-push

cargo build                                # production-style build with bundled DuckDB
cargo xtask build-web --release            # optimized + precompressed SPA embedded in trawl-web
```

Release web builds emit deterministic `.br`/`.gz` sidecars and fail if the
complete SPA exceeds its 2.5 MiB Brotli or 4 MiB gzip wire-size budget.
For remote `bin/dev` sessions, add `--release-spa` to avoid serving the much
larger debug Wasm artifact through Tailscale or across the LAN.

### Run a dev server

`fleet-dev` owns local database preparation, Fleet migrations, the persistent
development role/key, session configuration, Trunk proxying, and the attached
`mprocs` stack. With no machine profile, it uses the dedicated persistent
Docker Postgres provider on `127.0.0.1:5435`:

```bash
bin/fleet-dev doctor trawl
bin/dev
# browse http://localhost:8081/login and paste the key shown in mprocs
```

`bin/dev` is the canonical fast development path. Its first server build may
download the version-matched DuckDB shared library; Cargo caches the archive
and extracted libraries below `target/duckdb-download`. The launcher supplies
the loader path only to `trawld`, so the browser flow and the `mprocs` pane
layout are unchanged. An ordinary `cargo build -p trawl-server` still compiles
and statically bundles DuckDB.

If old bundled DuckDB fingerprints consume excessive disk, inspect and then
remove only that package's generated artifacts:

```bash
cargo clean -p libduckdb-sys --dry-run
cargo clean -p libduckdb-sys
```

The downloaded cache is preserved, but the next bundled build recompiles
DuckDB. Cleanup is deliberately not part of `bin/dev`.

`bin/dev --release-spa` keeps the same stack with an optimized SPA.
`bin/dev --tailscale` verifies the persistent Tailscale Serve mapping before
launch; configure it explicitly with `bin/fleet-dev setup trawl --exposure
tailscale`. See [Local development](docs/src/content/docs/getting-started/development.md)
for CNPG profiles, 1Password service accounts, state paths, and recovery.

### Tests

The separate `docker-compose.dev.yml` remains disposable test infrastructure;
`fleet-dev.compose.yml` is only for interactive development. The pg-backed
suites need `DATABASE_URL` pointing at a disposable cluster
(`#[sqlx::test]` creates ephemeral databases per test—never point it at real
data):

```bash
DATABASE_URL=postgres://fleet:fleet@localhost:5433/fleet_test cargo nextest run --workspace
```

`lefthook install` wires the same suites into pre-push, split across both
compose clusters so parallel runs don't collide.

## License

[MPL-2.0](LICENSE)
