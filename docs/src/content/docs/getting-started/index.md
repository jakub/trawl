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

Run the command blocks on this page in Bash. On macOS, run `bash` to start it
before pasting the commands.

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
its leading `v`, when prompted. Linux artifacts are verified on Debian 12
(Bookworm), with the system `libstdc++6` and `libgcc-s1` packages installed.
macOS artifacts require macOS 15 or newer and are built and tested
natively for arm64 and Intel. They use ad-hoc signatures; they are not Apple
Developer ID signed or notarized, so Gatekeeper can require explicit approval
before first use:

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
curl --fail --location --output "$TRAWL_ARCHIVE.tar.gz.sha256" \
  "https://github.com/jakub/trawl/releases/download/$TRAWL_RELEASE/$TRAWL_ARCHIVE.tar.gz.sha256"
case "$(uname -s)" in
  Linux) sha256sum --check "$TRAWL_ARCHIVE.tar.gz.sha256" || exit 1 ;;
  Darwin) shasum -a 256 --check "$TRAWL_ARCHIVE.tar.gz.sha256" || exit 1 ;;
esac
tar -xzf "$TRAWL_ARCHIVE.tar.gz"
sudo install -d /usr/local/bin /usr/local/lib/trawl
sudo install -m 0755 "$TRAWL_ARCHIVE"/bin/* /usr/local/bin/
sudo install -m 0644 "$TRAWL_ARCHIVE"/lib/trawl/* /usr/local/lib/trawl/
trawl --version
```

Expect the version from your tag, without the `v`. The tarball has no systemd
units, databases, or data directory. On Linux, keep all five executables on one
release. On both platforms, keep the executable and shared runtime versions
together.

## Build from source

Use the pinned Rust toolchain, a native C toolchain, Python 3.11 or newer,
and curl. Linux amd64 builds also require [Zig](https://ziglang.org/download/)
and [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild).
Linux arm64 builds use the native GNU compiler and linker.
The Linux server's browser build requires Trunk and the Rust
`wasm32-unknown-unknown` target.

The build helper downloads the checksum-verified official DuckDB library that
matches `Cargo.lock`. It includes ICU, JSON, and Parquet support without a
first-query extension download. Ordinary Cargo builds and tests also use the
same checksum verification and shared runtime, with a verified archive cache
under the Cargo target directory.
Use the build helper below to stage relocatable distributions with their library.
UTC and IANA timezone behavior remain enabled.

On Linux, build the server, browser, and CLI for the current host. An arm64
build uses that host's system libraries. To build arm64 artifacts for the
supported Debian 12 baseline, use the container procedure in the
[distribution helpers](https://github.com/jakub/trawl/blob/main/scripts/release/README.md).

```bash
git clone https://github.com/jakub/trawl.git
cd trawl
TRAWL_TARGET="$(rustc -vV | sed -n 's/^host: //p')"
(cd crates/trawl-web-ui && env -u NO_COLOR trunk build --release)
cargo xtask compress-web
bash scripts/release/build-distribution.sh "$PWD" "$TRAWL_TARGET" "$PWD/target/duckdb-runtime"
python3 scripts/release/distribution.py stage --source . \
  --binaries "target/$TRAWL_TARGET/release" --runtime target/duckdb-runtime \
  --output target/package --target "$TRAWL_TARGET" \
  --source-sha "$(git rev-parse HEAD)" --tooling-sha "$(git rev-parse HEAD)"
python3 scripts/release/smoke-cli.py target/package/bin/trawl scripts/release/fixtures/cli.parquet
sudo install -d /usr/local/bin /usr/local/lib/trawl
sudo install -m 0755 target/package/bin/* /usr/local/bin/
sudo install -m 0644 target/package/lib/trawl/* /usr/local/lib/trawl/
trawl --version
```

On macOS 15 or newer, build the CLI with native Cargo:

```bash
git clone https://github.com/jakub/trawl.git
cd trawl
TRAWL_TARGET="$(rustc -vV | sed -n 's/^host: //p')"
bash scripts/release/build-distribution.sh "$PWD" "$TRAWL_TARGET" "$PWD/target/duckdb-runtime" --cli-only
python3 scripts/release/distribution.py stage --source . \
  --binaries "target/$TRAWL_TARGET/release" --runtime target/duckdb-runtime \
  --output target/package --target "$TRAWL_TARGET" \
  --source-sha "$(git rev-parse HEAD)" --tooling-sha "$(git rev-parse HEAD)" --cli-only
python3 scripts/release/smoke-cli.py target/package/bin/trawl scripts/release/fixtures/cli.parquet
sudo install -d /usr/local/bin /usr/local/lib/trawl
sudo install -m 0755 target/package/bin/trawl /usr/local/bin/
sudo install -m 0644 target/package/lib/trawl/libduckdb.dylib /usr/local/lib/trawl/
trawl --version
```

The resulting `bin/trawl` locates its library relative to itself. Neither
platform needs a `LD_LIBRARY_PATH` or `DYLD_LIBRARY_PATH` setting.

Expect the version from `Cargo.toml`. A staging destination must not already
exist; select a new `--output` path for another build. Do not copy only the
executable: the `lib/trawl` directory is part of the installed product.

## What's next

[Your first query](/getting-started/first-query/) starts a private server with
two databases, TLS, keys, and three sample events, and checks an exact result.
