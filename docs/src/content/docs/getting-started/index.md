---
title: Installation
description: Choose the Trawl components you need and install a consistent release.
---

If someone already runs Trawl for you, start with
[Connect to a server](/start/connect/). If you have Parquet files to inspect,
[local mode](/start/local-parquet/) needs only the CLI.

A server installation has several parts:

| Component | Purpose |
| --- | --- |
| `trawl` | CLI queries and interactive terminal UI |
| `trawld` | HTTPS API, ingestion, storage, and query execution |
| `fleet-admin` | Database migrations, roles, and API keys |
| `trawl-admin` | Local TLS certificate generation |
| `trawl-web` | Browser UI and cookie-to-token session proxy |
| PostgreSQL | Separate Fleet identity and Trawl application databases |

The query engine runs inside `trawld`; you do not install a separate DuckDB
service. Logs are stored as Parquet on the server's filesystem. PostgreSQL
holds identity and application state, including the field catalog.

## APT repository (Debian/Ubuntu)

Add the repository, then install the client and server packages:

```bash
curl -fsSL https://trawl.sh/gpg.key \
  | sudo gpg --dearmor -o /usr/share/keyrings/trawl.gpg
echo "deb [signed-by=/usr/share/keyrings/trawl.gpg] https://trawl.sh/apt stable main" \
  | sudo tee /etc/apt/sources.list.d/trawl.list
sudo apt update
sudo apt install trawl-cli trawl-server
```

`trawl-cli` installs the `trawl` client. The `trawl-server` package includes the daemon,
administrative tools, and browser proxy, with systemd units. Installing
binaries does not replace database and identity setup. Read the
[deployment guide](/operate/deployment/) before configuring a permanent host.
The browser proxy defaults to loopback; external browser access needs a
TLS-terminating reverse proxy and the configured public origin.

## GitHub releases (tarball)

Open [GitHub releases](https://github.com/jakub/trawl/releases) and select a
release and its Linux architecture. Use the same release tag in both the URL
and filename. Do not combine a fixed filename with `releases/latest`.

In Bash, enter the selected tag, including its leading `v`:

```bash
read -r -p 'Release tag, including v: ' TRAWL_RELEASE
case "$(uname -m)" in
  x86_64) TRAWL_TARGET=x86_64-unknown-linux-gnu ;;
  aarch64) TRAWL_TARGET=aarch64-unknown-linux-gnu ;;
  *) echo 'Choose an available release asset for this architecture'; exit 1 ;;
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
trawld --version
```

The archive contains all five executables. It does not install systemd units,
create your databases, or choose a storage directory. Keep the executables on
the same release when following its documentation.

## Build from source

For source work, use [Local development](/getting-started/development/).
The workspace's `rust-toolchain.toml` and Cargo manifests specify the Rust
requirements. Bundled DuckDB builds need a C/C++ toolchain and CMake; the
browser build also needs the Wasm target and Trunk.

```bash
git clone https://github.com/jakub/trawl.git
cd trawl
cargo build --release -p trawl-cli -p trawl-admin -p trawl-server -p fleet-admin
env -u NO_COLOR cargo xtask build-web --release
```

The task runner builds the SPA and embeds it in the browser proxy. A Rust-only
build is not a substitute for the release browser asset build. Install the
five executables from `target/release/` for a complete manual installation.

## What's next

Follow [Your first query](/getting-started/first-query/) for an isolated local
installation with two databases, TLS, identities, sample ingest, and expected
results. For an existing server, get its URL and a reader key from its operator
and [connect](/start/connect/).
