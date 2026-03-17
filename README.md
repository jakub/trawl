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

## License

[MPL-2.0](LICENSE)
