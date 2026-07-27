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
The bundled DuckDB build needs a C/C++ toolchain (`gcc`/`clang` + `cmake`).

```bash
rustup target add wasm32-unknown-unknown   # web UI only
lefthook install                           # fmt/clippy pre-commit, tests pre-push

cargo build                                # first build compiles DuckDB — go get coffee
cargo xtask build-web --release            # trawl-web with the SPA embedded (optional)
```

### Run a dev server

Auth and app state live in postgres. The dev compose file runs two
throwaway clusters (tmpfs, fsync off — a restart wipes them, repeat the
bootstrap after one):

```bash
docker compose -f docker-compose.dev.yml up -d

# one-time bootstrap: databases, keystore schema, an API key
docker exec fleet-auth-dev createdb -U fleet fleet   # keystore
docker exec fleet-auth-dev createdb -U fleet trawl   # app state (trawld migrates it at boot)
export DATABASE_URL=postgres://fleet:fleet@localhost:5433/fleet
cargo run -p fleet-admin -- migrate
cargo run -p fleet-admin -- roles create --name trawl-admin \
  --perm trawl:query --perm trawl:schema_read --perm trawl:validate \
  --perm trawl:saved_query --perm trawl:export --perm trawl:stream \
  --perm trawl:query_cancel --perm trawl:server_manage
cargo run -p fleet-admin -- keys create --name dev --kind human --role trawl-admin
# token prints to stdout — keep it for the CLI config below
```

Point trawld at the cluster (`~/.trawl/trawld.toml`; TLS is
auto-generated self-signed):

```toml
[server]
http_addr = "127.0.0.1:5514"

[auth]
database_url = "postgres://fleet:fleet@localhost:5433/fleet"

[storage]
database_url = "postgres://fleet:fleet@localhost:5433/trawl"

[ingest]
enabled = true
internal_telemetry = true   # trawld's own events become queryable — instant test data
```

Give the CLI a dev profile (`~/.config/trawl/config.toml`):

```toml
[profiles.dev]
url = "https://localhost:5514"
insecure = true             # self-signed cert
token = "<token from keys create>"
```

Run and query:

```bash
cargo run -p trawl-server &
cargo run -p trawl-cli -- -p dev query -f table \
  "service=trawld last=10m | stats count() by event_type | sort -count"
```

### Tests

The pg-backed suites need `DATABASE_URL` pointing at the dev cluster
(`#[sqlx::test]` creates ephemeral databases per test — never point it at
real data):

```bash
DATABASE_URL=postgres://fleet:fleet@localhost:5433/fleet_test cargo nextest run --workspace
```

`lefthook install` wires the same suites into pre-push, split across both
compose clusters so parallel runs don't collide.

## License

[MPL-2.0](LICENSE)
