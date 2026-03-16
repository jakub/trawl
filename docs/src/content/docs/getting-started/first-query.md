---
title: Your First Query
description: Start trawld, ingest some logs, and run your first query.
---

## Start the server

```bash
trawld --config ~/.trawl/trawld.toml
```

On first run with no TLS config, trawld auto-generates a self-signed certificate and writes it to `~/.trawl/tls/`. You'll see something like:

```
INFO trawld starting version=0.1.8 addr=127.0.0.1:5514
WARN no TLS certificate configured, generating self-signed cert
INFO TLS certificate written to ~/.trawl/tls/cert.pem
INFO HTTPS server listening on https://127.0.0.1:5514
```

You'll need a minimal config file. Create `~/.trawl/trawld.toml`:

```toml
[server]
http_addr = "127.0.0.1:5514"

[data]
path = "~/.trawl/data"

[auth]
db_path = "~/.trawl/auth.db"
```

See the [configuration reference](/reference/configuration/) for all available options.

## Create an API key

In a separate terminal, create a key for yourself:

```bash
trawl-admin keys create --role admin --name "my-key"
```

This prints the token once:

```
created API key:
  name:    my-key
  role:    admin
  prefix:  flt_abc1
  token:   flt_abc1234567890abcdefghijklmnopqrstuvwxyz0123456

save this token — it will not be shown again.
```

Copy the full `flt_...` token. You'll need it for the next step.

## Configure your client

Create `~/.config/trawl/config.toml`:

```toml
[server]
url = "https://localhost:5514"
token = "flt_your_token_here"
insecure = true  # accept self-signed certs
```

You can also use named profiles for multiple servers:

```toml
[server]
url = "https://trawl.prod.example.com:5514"
token = "flt_prod_token"

[profiles.dev]
url = "https://localhost:5514"
token = "flt_dev_token"
insecure = true
```

Use `trawl -p dev` or `TRAWL_PROFILE=dev` to select a profile.

## Send some test data

With the server running, ingest a few events:

```bash
curl -k -X POST https://localhost:5514/api/v1/ingest \
  -H "Authorization: Bearer flt_your_token_here" \
  -H "Content-Type: application/json" \
  -d '[
    {"timestamp": "'$(date -u +%Y-%m-%dT%H:%M:%SZ)'", "service": "myapp", "level": "info", "message": "server started"},
    {"timestamp": "'$(date -u +%Y-%m-%dT%H:%M:%SZ)'", "service": "myapp", "level": "error", "message": "connection refused"},
    {"timestamp": "'$(date -u +%Y-%m-%dT%H:%M:%SZ)'", "service": "nginx", "level": "warn", "message": "upstream timeout"}
  ]'
```

In production you'd use [Vector](/getting-started/vector-integration/) to ship logs continuously, but manual ingest is fine for testing.

## Run your first query

```bash
# all events from the last hour
trawl query "last=1h"

# filter by service
trawl query "service=myapp"

# aggregate errors by service
trawl query "level=error last=1h | stats count() by service"

# pipe to jq for ad-hoc processing
trawl query "last=1h | stats count() by service" | jq '.service'
```

## Launch the TUI

For interactive exploration, just run:

```bash
trawl
```

This opens the terminal UI with a query editor, schema browser, and results pane. Type a query in the editor and press `Ctrl+Enter` to execute.

## Try embedded mode (no server)

You can also query parquet files directly without running trawld:

```bash
trawl query --data 'path/to/*.parquet' "* | stats count() by service"
```

This uses a single ephemeral DuckDB connection — no auth, no hot buffer, no row limit. Useful for ad-hoc analysis of exported data or for trying trawl before setting up a server.

## What's next

- [Vector Integration](/getting-started/vector-integration/) — set up continuous log shipping
- [DSL Reference](/reference/dsl/) — full query language documentation
- [Configuration](/reference/configuration/) — all server and client options
