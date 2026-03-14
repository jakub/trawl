---
title: CLI & TUI
description: Command-line interface and terminal UI reference.
---

The `trawl` binary provides three modes of operation: an interactive TUI, a query command for scripting, and a validate command for syntax checking.

## TUI (interactive mode)

Launch with no subcommand to open the terminal UI.

```bash
trawl                           # connects to default server
trawl -p dev                    # connects to dev profile server
```

The TUI provides:
- Multi-tab query editor with syntax highlighting
- Schema browser with field value profiling
- Query history and saved queries
- Live tail via SSE streaming
- Vim-style result search
- Clipboard integration

## Query mode

Execute a query and print results to stdout.

```bash
trawl query "level=error last=1h | stats count() by service"
```

### Output formats

Output format is auto-detected: table for TTY, JSON for pipes. Override with `-f`:

```bash
trawl query -f table "..."      # pretty-printed box-drawing table
trawl query -f json "..."       # one JSON object per line (ndjson)
trawl query -f csv "..."        # RFC 4180 with formula injection protection
trawl query -f parquet -o out.parquet "..."  # Snappy-compressed parquet file
```

**Table** output includes a row count footer and box-drawing borders.

**JSON** output emits one JSON object per row, ndjson-style. Ideal for piping to `jq`:

```bash
trawl query "last=1h | stats count() by service" | jq '.service'
```

**CSV** output follows RFC 4180 with formula injection protection — string values starting with `=`, `+`, `-`, `@`, `\t`, or `|` are prefixed with `'`.

**Parquet** output requires `--output <path>` and produces a Snappy-compressed parquet file.

### Embedded mode

Query local parquet files directly, without a server:

```bash
trawl query --data 'data/**/*.parquet' "* | stats count() by service"
trawl query --data '/path/to/*.parquet' "level=error | head 10"
```

Embedded mode uses a single ephemeral DuckDB connection with no hot buffer, no auth, and no row limit.

## Validate mode

Check query syntax without executing:

```bash
trawl validate "level=error | stats count() by host"
trawl validate -p dev "..."     # validate against dev server
```

## Global flags

| Flag | Environment variable | Description |
|------|---------------------|-------------|
| `-p, --profile <NAME>` | `TRAWL_PROFILE` | Named profile from config |
| `--url <URL>` | `TRAWL_URL` | Server URL (default: `https://localhost:5514`) |
| `--token <TOKEN>` | `TRAWL_TOKEN` | API token |
| `--insecure` | `TRAWL_INSECURE` | Accept self-signed TLS certificates |
| `-c, --config <PATH>` | — | Config file path (default: `~/.config/trawl/config.toml`) |

## Configuration

Client configuration uses named profiles in `~/.config/trawl/config.toml`:

```toml
# Default server (used when no --profile is specified)
[server]
url = "https://trawl-01.lab.example.com:5514"
token = "flt_your_token_here"

# Dev profile (used with --profile dev or TRAWL_PROFILE=dev)
[profiles.dev]
url = "https://localhost:5514"
token = "flt_dev_token_here"
insecure = true
```

Environment variables and CLI flags override profile settings.
