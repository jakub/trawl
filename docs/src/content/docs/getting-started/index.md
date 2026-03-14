---
title: Installation
description: How to install trawl and trawld on your system.
---

## APT repository (Debian/Ubuntu)

The easiest way to install trawl on Debian-based systems. The APT repository is updated automatically with each release.

```bash
# Add the signing key
curl -fsSL https://trawl.sh/gpg.key \
  | sudo gpg --dearmor -o /usr/share/keyrings/trawl.gpg

# Add the repository
echo "deb [signed-by=/usr/share/keyrings/trawl.gpg] https://trawl.sh/apt stable main" \
  | sudo tee /etc/apt/sources.list.d/trawl.list

# Install
sudo apt update
sudo apt install trawl trawld
```

This installs two packages:
- **trawl** — the CLI and TUI client
- **trawld** — the server daemon

## GitHub releases (tarball)

Pre-built binaries are available for Linux (x86_64 and aarch64).

```bash
# x86_64
curl -fsSL https://github.com/jakub/trawl/releases/latest/download/trawl-v0.1.8-x86_64-unknown-linux-gnu.tar.gz \
  | tar xz

# aarch64 (Raspberry Pi, ARM servers)
curl -fsSL https://github.com/jakub/trawl/releases/latest/download/trawl-v0.1.8-aarch64-unknown-linux-gnu.tar.gz \
  | tar xz
```

The tarball contains three binaries:
- `trawl` — CLI and TUI
- `trawld` — server daemon
- `trawl-admin` — admin tool for API key management and TLS cert generation

Move them somewhere in your `$PATH`:

```bash
sudo mv trawl-*/trawl trawl-*/trawld trawl-*/trawl-admin /usr/local/bin/
```

## Build from source

Requires Rust 1.88+ (edition 2024).

```bash
git clone https://github.com/jakub/trawl.git
cd trawl
cargo build --release

# Binaries are in target/release/
ls target/release/{trawl,trawld,trawl-admin}
```

## What's next

Once installed, you'll need:

1. **Start the server** — `trawld` listens for log ingestion and queries
2. **Configure Vector** — point your log sources at trawld's ingest endpoint
3. **Run your first query** — use `trawl query` or launch the TUI with `trawl`

See [Your First Query](/getting-started/first-query/) for a walkthrough.
