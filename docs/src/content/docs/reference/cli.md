---
title: CLI & TUI
description: Command syntax, options, defaults, and output formats for the trawl binary.
---

The `trawl` binary runs the terminal UI, executes queries, validates syntax,
inspects the field catalog, and drives a running TUI over a Unix socket.
For catalog procedures, see [catalog administration](/operate/catalog/).

## Global options

Every subcommand accepts these options.

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `-p, --profile` | `<NAME>` | *(none)* | Named profile from the config file. Overrides `[server]` |
| `--url` | `<URL>` | `https://localhost:5514` | Server URL |
| `--token` | `<TOKEN>` | *(none)* | API token |
| `--insecure` | *(flag)* | `false` | Accept self-signed TLS certificates |
| `-c, --config` | `<PATH>` | `~/.config/trawl/config.toml` | Config file path |
| `-V, --version` | *(flag)* | | Print the version and exit |
| `-h, --help` | *(flag)* | | Print help and exit |

A flag beats an environment variable, which beats the config file. Profiles and
the `[ui]` and `[tail]` settings live in
[client configuration](/reference/configuration/#client-configuration).

When `insecure` is on from the flag, `TRAWL_INSECURE`, `[server]`, or a profile,
`trawl` writes one warning line to stderr before any other output. Stdout does
not change. To trust a self-signed or private certificate with verification
left on, set `ca_cert` in the config file. It has no flag or environment
variable.

## Environment variables

| Variable | Equivalent flag | Description |
|----------|-----------------|-------------|
| `TRAWL_PROFILE` | `-p, --profile` | Named profile to select |
| `TRAWL_URL` | `--url` | Server URL |
| `TRAWL_TOKEN` | `--token` | API token |
| `TRAWL_INSECURE` | `--insecure` | Accept self-signed certificates |
| `RUST_LOG` | `warn` | Tracing filter for the TUI log file |

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | The command succeeded |
| `1` | The command failed. `trawl` prints the reason to stderr, except for a broken pipe, which exits quietly |

## TUI mode

```text
trawl [--driver [PATH]]
```

Run `trawl` with no subcommand to open the terminal UI: a multi-tab query
editor, a schema browser, query history, saved queries, live tail over SSE, and
result search. Tracing goes to `~/.config/trawl/tui.log`, not stderr.

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--driver` | `[PATH]` | `~/.config/trawl/driver.sock` | Listen on a Unix socket for programmatic control. The path is optional |

```bash
trawl -p dev
```

## Query mode

```text
trawl query <QUERY> [--data GLOB] [-f FORMAT] [-o PATH]
```

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--data` | `<GLOB>` | *(none)* | Parquet glob for embedded mode. No server is contacted |
| `-f, --format` | `table\|json\|csv\|parquet` | `table` on a TTY, `json` on a pipe | Output format |
| `-o, --output` | `<PATH>` | *(stdout)* | Write output to a file. Required for `parquet` |

```bash
trawl query "_severity>=error last=1h | stats count() by service"
```

### Output formats

| Format | Shape | Notes |
|--------|-------|-------|
| `table` | Box-drawing table with a row-count footer | Adds the incomplete-results footer when one applies |
| `json` | One JSON object per row, newline-delimited | Pipe it to `jq` |
| `csv` | RFC 4180 | A string value starting with `=`, `+`, `-`, `@`, tab, or `\|` is prefixed with `'` |
| `parquet` | Snappy-compressed Parquet file | Requires `-o, --output` |

```bash
trawl query "last=1h | stats count() by service" | jq '.service'
```

### The incomplete-results footer

When a query binds a field whose catalog pin is degraded, `table` output adds
one line after the row count:

```text
note: results may be incomplete — degraded field(s): duration (see: trawl schema field duration)
```

With `-o, --output` the footer goes to stderr, so the file holds only rows.
`json`, `csv`, and `parquet` never print it: the HTTP response carries
`degraded_fields` instead. Embedded mode reads no catalog and never prints it.

### Embedded mode

`--data` queries local Parquet files through one ephemeral DuckDB connection,
with no server, no hot buffer, no authentication, and no row limit.

```bash
trawl query --data 'data/**/*.parquet' "* | stats count() by service"
```

## Validate mode

```text
trawl validate <QUERY>
```

Validate checks syntax without executing. With a resolvable token the server
also checks semantics, function arity, and regex patterns. Without one, `trawl`
parses locally.

```bash
trawl validate "_severity>=error | stats count() by host"
```

## Schema mode

```text
trawl schema <SUBCOMMAND>
```

Schema subcommands read and change the field catalog: pinned types, per-service
observations, and type-conflict evidence. Reads need `schema_read`. Repin,
cancellation, pin reclamation, and acknowledgement need `schema_write`.

Every subcommand takes `-f, --format` with the `table`, `json`, and `csv` values
and the TTY auto-detection `query` uses. A `--last` window accepts `s`, `m`,
`h`, `d`, and `w`. Field names fold to ASCII lowercase.

### Fields

```text
trawl schema fields [--service NAME] [--last WINDOW] [--limit N] [--data GLOB] [-f FORMAT]
```

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--service` | `<NAME>` | *(all)* | Only fields observed for this service |
| `--last` | `<WINDOW>` | *(all)* | Only fields observed inside this window |
| `--limit` | `<N>` | `500` | Maximum fields to list. The server clamps to the pin cap of 10000 |
| `--data` | `<GLOB>` | *(none)* | Embedded listing over local Parquet: names and physical types only |

The listing carries a `degraded` column. Catalog fill (`N/M pins used`) goes to
stderr. Embedded mode runs a plain `DESCRIBE` and reads no catalog metadata.

```bash
trawl schema fields --service nginx --last 7d
```

### Field

```text
trawl schema field <NAME> [--limit N] [--after CURSOR] [-f FORMAT]
```

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--limit` | `<N>` | `100` | Service observations per page. The maximum is 1000 |
| `--after` | `<CURSOR>` | *(first page)* | Resume after a previous run's printed cursor. The cursor is opaque, so pass it unchanged |

The detail view prints the pin, the degraded verdict, bounded conflict samples,
and per-service observations. Conflict rows and lifetime totals use different
windows. See [degraded pins](/operate/catalog/#read-a-degraded-pin).

```bash
trawl schema field duration
```

### Conflicts

```text
trawl schema conflicts [--field NAME] [--service NAME] [--last WINDOW] [--limit N] [-f FORMAT]
```

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--field` | `<NAME>` | *(all)* | Only conflicts for this field |
| `--service` | `<NAME>` | *(all)* | Only conflicts from this service |
| `--last` | `<WINDOW>` | *(all)* | Only conflicts recorded inside this window |
| `--limit` | `<N>` | `100` | Maximum rows. The maximum is 1000 |

```bash
trawl schema conflicts --field duration --service envoy --last 7d
```

### Repin

```text
trawl schema repin <FIELD> --to TYPE [--dialect otel|syslog] [--dry-run] [--force]
    [--yes] [--wait] [--max-nulled-rows N] [--max-ambiguous-rows N] [-f FORMAT]
```

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--to` | `<TYPE>` | *(required)* | `BIGINT`, `DOUBLE`, `TIMESTAMP`, `BOOLEAN`, `VARCHAR`, or `SEVERITY`, case-insensitive |
| `--dialect` | `otel\|syslog` | `otel` | Which ladder the corpus numerals read in. Legal only with `--to severity` |
| `--dry-run` | *(flag)* | `false` | Scan and persist a report job. The corpus is not rewritten |
| `--force` | *(flag)* | `false` | Accept a projected loss, or run a same-type resurrection pass, inside the ceilings |
| `--yes` | *(flag)* | `false` | Skip the interactive confirmation. Required off a TTY |
| `--wait` | *(flag)* | `false` | Poll this job to a terminal result |
| `--max-nulled-rows` | `<N>` | derived | With `--force`, the most rows the rewrite may null |
| `--max-ambiguous-rows` | `<N>` | derived | With `--force` and `--to severity`, the most dialect-ambiguous numerals the rewrite may carry |

Repin runs on an ingest-enabled node. An unspecified force ceiling derives from
the preview scan, with ten percent headroom and a minimum addition of ten rows,
and the CLI prints the ceiling it binds. `requires_force` in the report says
whether the same executing request would be refused.

With `--wait`, the command exits `0` for a completed rewrite or a dry-run
report. Any other terminal state exits nonzero, as does a job identity the
status endpoint stops naming. See [the repin procedure](/operate/catalog/#repin-a-field)
and [severity repinning](/operate/catalog/#repin-to-severity).

```bash
trawl schema repin duration --to BIGINT --dry-run
```

### Repin status

```text
trawl schema repin-status [-f FORMAT]
```

Status needs `schema_read` and returns the running job, or the newest one when
none is running. There is no lookup by job ID.

```bash
trawl schema repin-status
```

### Repin cancel

```text
trawl schema repin-cancel [-f FORMAT]
```

Cancellation exits `0` when the server accepts the request, and nonzero when no
job is running or the cutover has started. Acceptance is not proof of terminal
cancellation. In `json` and `csv` the receipt is one record with verdict,
detail, and job columns. In `table` the sentence and the job table are separate.
See [cancellation and verification](/operate/catalog/#cancel-the-repin).

```bash
trawl schema repin-cancel
```

### Reclaim dead pin slots

```text
trawl schema gc-pins [--dry-run] [--older-than WINDOW] [-f FORMAT]
```

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--dry-run` | *(flag)* | `false` | Scan and report only. There is no `--yes`, so omitting this flag deletes metadata |
| `--older-than` | `<WINDOW>` | `30d` | How long a field must have gone unobserved. The server raises it to the retention floor when that floor is longer |

The output states the requested window, the floor, and the effective window.
Summary text goes to stderr for `json` and `csv`, and to stdout for `table`.
See [pin reclamation](/operate/catalog/#reclaim-unused-pins).

```bash
trawl schema gc-pins --dry-run --older-than 90d
```

### Acknowledge a degraded pin

```text
trawl schema ack <FIELD> [--note TEXT | --clear] [-f FORMAT]
```

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--note` | `<TEXT>` | *(none)* | Why the pin is accepted as it stands. The limit is 1024 bytes. Conflicts with `--clear` |
| `--clear` | *(flag)* | `false` | Withdraw the acknowledgement |

An acknowledgement covers the evidence that exists when you write it. The next
conflict episode raises the badge again.

```bash
trawl schema ack duration --note "fix due Friday"
```

## Driver mode

```text
trawl driver [--socket PATH] <SUBCOMMAND>
```

Driver subcommands control a TUI started with `--driver`. Each invocation opens
one connection to the socket and closes it.

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--socket` | `<PATH>` | `~/.config/trawl/driver.sock` | Path to the driver Unix socket |

| Subcommand | Arguments | Description |
|------------|-----------|-------------|
| `status` | | Print TUI state as JSON |
| `query <QUERY>` | `-f, --format`, `--timeout <MS>` (default `300000`) | Set the editor content, execute it, and print the results |
| `set-query <QUERY>` | | Set the editor content without executing |
| `capture` | `--width <N>` (default `120`), `--height <N>` (default `40`) | Render the TUI to text |
| `key <KEY>` | | Inject one keystroke, such as `ctrl+enter`, `F5`, or `a` |
| `keys <KEYS>...` | | Inject several keystrokes in order |
| `get-results` | `--tab <N>`, `-f, --format` | Print structured result data from a tab. The tab index is 0-based and defaults to the active tab |
| `quit` | | Ask the TUI to exit cleanly |

`parquet` is not a driver output format. Starting a TUI removes an existing file
at the socket path, so give an automated session its own socket.

```bash
trawl driver --socket /tmp/session.sock capture --width 160 --height 50
```
