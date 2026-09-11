---
title: CLI & TUI
description: Command-line interface and terminal UI reference.
---

The `trawl` binary provides interactive TUI, query, validation, schema, and
TUI-driver commands. This page defines command syntax and output. For catalog
procedures, use [catalog administration](/operate/catalog/).

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
trawl query "_severity>=error last=1h | stats count() by service"
```

### Output formats

Output format is auto-detected: table for TTY, JSON for pipes. Override with `-f`:

```bash
trawl query -f table "..."      # pretty-printed box-drawing table
trawl query -f json "..."       # one JSON object per line (ndjson)
trawl query -f csv "..."        # RFC 4180 with formula injection protection
trawl query -f parquet -o out.parquet "..."  # Snappy-compressed parquet file
```

**Table** output includes a row count footer and box-drawing borders. When
the query bound a field whose catalog pin is degraded (see below), one more
footer line follows:

```
note: results may be incomplete — degraded field(s): duration (see: trawl schema field duration)
```

The notice is table-only. The HTTP query response carries `degraded_fields`,
but CLI JSON/CSV output remains data rows without an added prose record. With `--output <file>` it goes to stderr, so the file stays clean.
Embedded `--data` mode has no catalog and never prints it.

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
trawl query --data '/path/to/*.parquet' "_severity>=17 | head 10"
```

Embedded mode uses a single ephemeral DuckDB connection with no hot buffer, no auth, and no row limit.

## Validate mode

Check query syntax without executing:

```bash
trawl validate "_severity>=error | stats count() by host"
trawl validate -p dev "..."     # validate against dev server
```

## Schema mode

Inspect the field catalog — pinned types, per-service observations, and
type-conflict evidence:

```bash
trawl schema fields                    # pinned fields, types, conflict counts
trawl schema fields --service nginx    # only fields that service has carried
trawl schema fields --last 7d          # only fields observed in the window
trawl schema field duration            # detail: type, when/where pinned, which services
trawl schema conflicts --last 7d       # schema-health dashboard
trawl schema conflicts --field duration --service envoy
trawl schema ack duration --note "fix due Friday"   # acknowledge a degraded badge
```

`fields` and `conflicts` render through the standard output formats
(`-f table|json|csv`, auto-detected like `query`). `fields` prints the
catalog fill (`N/M pins used`) to stderr; `--limit` raises the listing
caps (fields default 500, conflicts default 100/max 1000). `--last`
accepts DSL-style windows (`s`, `m`, `h`, `d`, `w`).

### Degraded pins

`fields` carries a `degraded` column. `field` prints a verdict and bounded
samples; conflict rows and verdict lifetime totals have different windows.
See [diagnosis and acknowledgement](/operate/catalog/#degraded-pins).
`ack FIELD [--note TEXT | --clear]` needs `schema_write`; note limit is 1024 bytes.

`field FIELD [--limit N] [--after CURSOR]` pages service observations.
The default page is 100 and the maximum is 1000. Pass the returned cursor
unchanged; it is opaque.

### Repin

```text
trawl schema repin FIELD --to TYPE [--dialect otel|syslog] [--dry-run]
    [--force] [--yes] [--wait] [--max-nulled-rows N] [--max-ambiguous-rows N]
trawl schema repin-status
trawl schema repin-cancel
```

Types are `BIGINT`, `DOUBLE`, `TIMESTAMP`, `BOOLEAN`, `VARCHAR`, and
`SEVERITY`, case-insensitive. `--dialect` is legal only for SEVERITY.
Repin and cancellation need `schema_write` on an ingest-enabled node.
Status needs `schema_read` and returns the running or newest job, with no ID lookup.
There is no human-key-kind gate.

| Option | Contract |
| --- | --- |
| `--dry-run` | Scan and persist a report job; do not rewrite the corpus |
| `--force` | Accept projected loss or run a same-type resurrection pass, subject to ceilings |
| `--yes` | Skip interactive confirmation; required off a TTY |
| `--wait` | Poll for this job's terminal result; fail if the status endpoint stops naming it |
| `--max-nulled-rows N` | With force, bound rows the rewrite may null |
| `--max-ambiguous-rows N` | With force, bound dialect-ambiguous numerals |

Unspecified force ceilings derive from the preview scan with ten percent
headroom and a minimum addition of ten rows. The CLI prints and binds them on
execution; explicit applicable ceilings avoid that preview. `requires_force`
on the report says whether the identical executing request would be refused.

`--wait` exits zero for a completed rewrite or dry-run report. Other terminal
states and a lost job identity are nonzero. Formats are `table`, `json`, and `csv`.
See [the repin procedure](/operate/catalog/#repin) before executing.

#### Stopping a running repin

Cancellation exits zero when accepted, nonzero when no job is running or cutover
already started. Acceptance does not prove terminal cancellation. In JSON/CSV
the receipt is one record with verdict, detail, and job columns; table output
separates the sentence and job table. See [cancellation and verification](/operate/catalog/#stopping-a-running-repin).

#### Putting a sender's own field on the severity ladder

See [severity repinning](/operate/catalog/#putting-a-senders-own-field-on-the-severity-ladder)
for dialect selection and the distinction between historical rewriting and future ingestion.

### Reclaiming dead pin slots

```text
trawl schema gc-pins [--dry-run] [--older-than WINDOW] [-f table|json|csv]
```

There is no `--yes`: omitting `--dry-run` executes metadata deletion.
`WINDOW` accepts `s`, `m`, `h`, `d`, or `w`, defaults to 30 days, and is raised
to the server's retention floor. Output states requested, floor, and effective
windows. Summary text goes to stderr for JSON/CSV and stdout for tables.
See [pin reclamation](/operate/catalog/#reclaiming-dead-pin-slots) for proof and refusal conditions.

Embedded mode works for the field listing only — a plain `DESCRIBE` over
local parquet, no server or postgres needed:

```bash
trawl schema fields --data 'data/**/*.parquet'   # names + physical types only
```

Catalog metadata (pins, observations, conflicts) requires a server. Note
that over foreign parquet with irreconcilably drifted columns, embedded
queries error loudly instead of silently coercing to `VARCHAR` — trawl's
own files can never conflict (write-time catalog conformance).

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
