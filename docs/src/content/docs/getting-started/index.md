---
title: Installation
description: Install the Trawl client and server packages from APT, a release tarball, or source.
---

If someone already runs Trawl for you, [connect to it](/start/connect/). To
inspect Parquet files only, [Query local Parquet](/start/local-parquet/) needs
the `trawl` CLI alone. A server installation has these parts:

| Component | Responsibility |
| --- | --- |
| `trawl` | CLI queries, local Parquet queries, and the terminal UI |
| `trawld` | Ingest, storage, queries, and the HTTPS API |
| `trawl-web` | Browser UI and session proxy |
| `fleet-admin` | Fleet migrations, keys, roles, and session keys |
| `trawl-admin` | Self-signed TLS certificate generation |
| PostgreSQL | The Fleet keystore and the Trawl app-state database |

## Install from APT on Debian or Ubuntu

`trawl-cli` installs `trawl`. `trawl-server` installs `trawld`, `trawl-web`,
`fleet-admin`, and `trawl-admin`, with systemd units for the two daemons.
Both packages depend on the exact matching `trawl-runtime` package, which owns
the shared DuckDB library. APT installs it automatically.

```bash
curl -fsSL https://trawl.sh/gpg.key \
  | sudo gpg --dearmor -o /usr/share/keyrings/trawl.gpg
echo "deb [signed-by=/usr/share/keyrings/trawl.gpg] https://trawl.sh/apt stable main" \
  | sudo tee /etc/apt/sources.list.d/trawl.list
sudo apt update
sudo apt install trawl-cli trawl-server
trawl --version
```

Expect `trawl`, the version, and the build details. A client-only machine
needs `trawl-cli` alone. The package writes `/etc/trawl/trawld.toml` and
enables both units. `trawld` refuses to start until `[auth]` and `[storage]`
have database URLs, so the [deployment guide](/operate/deployment/) is next.
`trawl-web` listens on `127.0.0.1:8090` by default, so browser access from
other machines needs a TLS-terminating reverse proxy and a public origin.

## Install from a GitHub release tarball

Linux tarballs contain all five executables in `bin/`. Native macOS arm64
and Intel tarballs contain the CLI. Every tarball includes its DuckDB library
in `lib/trawl/`; keep both directories together. Pick a tag from
[GitHub releases](https://github.com/jakub/trawl/releases) and enter it, with
its leading `v`, when prompted:

```bash
read -r -p 'Release tag, including v: ' TRAWL_RELEASE
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) TRAWL_TARGET=x86_64-unknown-linux-gnu ;;
  Linux-aarch64) TRAWL_TARGET=aarch64-unknown-linux-gnu ;;
  Darwin-arm64) TRAWL_TARGET=aarch64-apple-darwin ;;
  Darwin-x86_64) TRAWL_TARGET=x86_64-apple-darwin ;;
  *) echo 'No release asset for this architecture'; exit 1 ;;
esac
TRAWL_ARCHIVE="trawl-$TRAWL_RELEASE-$TRAWL_TARGET"
curl --fail --location --output "$TRAWL_ARCHIVE.tar.gz" \
  "https://github.com/jakub/trawl/releases/download/$TRAWL_RELEASE/$TRAWL_ARCHIVE.tar.gz"
tar -xzf "$TRAWL_ARCHIVE.tar.gz"
sudo install -d /usr/local/bin /usr/local/lib/trawl
sudo install -m 0755 "$TRAWL_ARCHIVE"/bin/* /usr/local/bin/
sudo install -m 0644 "$TRAWL_ARCHIVE"/lib/trawl/* /usr/local/lib/trawl/
trawl --version
```

Expect the version from your tag, without the `v`. The tarball has no systemd
units, databases, or data directory. Keep all five executables on one release.

## Build from source

Use the pinned Rust toolchain, a native C toolchain, Python 3.11 or newer,
and curl. Linux distribution builds also require Zig and `cargo-zigbuild`.
The Linux server's browser build requires Trunk and the Rust
`wasm32-unknown-unknown` target.

The build helper downloads the checksum-verified official DuckDB library that
matches `Cargo.lock`. It includes ICU, JSON, and Parquet support without a
first-query extension download. The default bundled Cargo build is not the
supported distribution route: its current DuckDB build lacks the required ICU
support. Keep UTC and IANA timezone behavior enabled.

On Linux, build the server, browser, and CLI:

```bash
git clone https://github.com/jakub/trawl.git
cd trawl
TRAWL_TARGET="$(rustc -vV | sed -n 's/^host: //p')"
env -u NO_COLOR cargo xtask build-web --release
bash scripts/release/build-distribution.sh "$PWD" "$TRAWL_TARGET" "$PWD/target/duckdb-runtime"
python3 scripts/release/distribution.py stage \
  --binaries "target/$TRAWL_TARGET/release" --runtime target/duckdb-runtime \
  --output target/package --target "$TRAWL_TARGET" \
  --source-sha "$(git rev-parse HEAD)" --tooling-sha "$(git rev-parse HEAD)"
python3 scripts/release/smoke-cli.py target/package/bin/trawl scripts/release/fixtures/cli.parquet
sudo install -d /usr/local/bin /usr/local/lib/trawl
sudo install -m 0755 target/package/bin/* /usr/local/bin/
sudo install -m 0644 target/package/lib/trawl/* /usr/local/lib/trawl/
trawl --version
```

On macOS, use the same checkout and host-target selection, skip the browser
build, and append `--cli-only` to both the build helper and the `stage` command.
The helper uses native Cargo on macOS. Packaged macOS binaries require macOS 15
or newer. The resulting `bin/trawl` locates its library relative to itself;
no `LD_LIBRARY_PATH` or `DYLD_LIBRARY_PATH` setting is needed.

Expect the version from `Cargo.toml`. A staging destination must not already
exist; select a new `--output` path for another build. Do not copy only the
executable: the `lib/trawl` directory is part of the installed product.

## What's next

[Your first query](/getting-started/first-query/) starts a private server with
two databases, TLS, keys, and three sample events, and checks an exact result.
