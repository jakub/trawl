---
title: Your First Query
description: Start trawld, ingest some logs, and run your first query.
---

:::note
This guide is a work in progress. Check back soon for a complete walkthrough.
:::

## Start the server

```bash
trawld
```

On first run, trawld generates a self-signed TLS certificate and creates a default admin API key. The key is printed to stdout — save it.

## Create an API key

```bash
trawl-admin key create --role analyst --name "my-key"
```

## Configure your client

Create `~/.config/trawl/config.toml`:

```toml
[server]
url = "https://localhost:5514"
token = "flt_your_token_here"
insecure = true  # accept self-signed certs
```

## Run a query

```bash
# List all events from the last hour
trawl query "last=1h | head 10"

# Launch the TUI for interactive exploration
trawl
```
