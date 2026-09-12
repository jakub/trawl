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

Each release has one tarball per Linux architecture,
`trawl-<tag>-<target>.tar.gz`, with all five executables. Pick a tag from
[GitHub releases](https://github.com/jakub/trawl/releases) and enter it, with
its leading `v`, when prompted:

```bash
read -r -p 'Release tag, including v: ' TRAWL_RELEASE
case "$(uname -m)" in
  x86_64) TRAWL_TARGET=x86_64-unknown-linux-gnu ;;
  aarch64) TRAWL_TARGET=aarch64-unknown-linux-gnu ;;
  *) echo 'No release asset for this architecture'; exit 1 ;;
esac
TRAWL_ARCHIVE="trawl-$TRAWL_RELEASE-$TRAWL_TARGET"
curl --fail --location --output "$TRAWL_ARCHIVE.tar.gz" \
  "https://github.com/jakub/trawl/releases/download/$TRAWL_RELEASE/$TRAWL_ARCHIVE.tar.gz"
tar -xzf "$TRAWL_ARCHIVE.tar.gz"
sudo install -m 0755 \
  "$TRAWL_ARCHIVE/trawl" "$TRAWL_ARCHIVE/trawld" \
  "$TRAWL_ARCHIVE/trawl-admin" "$TRAWL_ARCHIVE/fleet-admin" \
  "$TRAWL_ARCHIVE/trawl-web" /usr/local/bin/
trawl --version
```

Expect the version from your tag, without the `v`. The tarball has no systemd
units, databases, or data directory. Keep all five executables on one release.

## Build from source

[Local development](/getting-started/development/) lists the toolchain:
`rust-toolchain.toml` pins Rust, the bundled DuckDB needs a C and C++
toolchain and CMake, and the browser build needs the `wasm32-unknown-unknown`
target and Trunk.

```bash
git clone https://github.com/jakub/trawl.git
cd trawl
cargo build --release -p trawl-cli -p trawl-admin -p trawl-server -p fleet-admin
env -u NO_COLOR cargo xtask build-web --release
sudo install -m 0755 target/release/{trawl,trawld,trawl-admin,fleet-admin,trawl-web} /usr/local/bin/
trawl --version
```

Expect the version from `Cargo.toml`. `cargo xtask build-web` builds the
browser UI into `trawl-web`, which a plain `cargo build` leaves out.

## What's next

[Your first query](/getting-started/first-query/) starts a private server with
two databases, TLS, keys, and three sample events, and checks an exact result.
