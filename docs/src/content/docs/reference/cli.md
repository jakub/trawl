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

The notice is table-only: json/csv carry the same fact as a
`degraded_fields` list on the wire, where a prose line would corrupt the
stream. With `--output <file>` it goes to stderr, so the file stays clean.
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
trawl query --data '/path/to/*.parquet' "_severity>=error | head 10"
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
```

`fields` and `conflicts` render through the standard output formats
(`-f table|json|csv`, auto-detected like `query`). `fields` prints the
catalog fill (`N/M pins used`) to stderr; `--limit` raises the listing
caps (fields default 500, conflicts default 100/max 1000). `--last`
accepts DSL-style windows (`s`, `m`, `h`, `d`, `w`).

### Degraded pins

`fields` carries a `degraded` column, and its stderr summary counts them:
a pin is degraded when it has been shelving values for **over a day** and
in volume (100 rows or 3 distinct episodes). `schema field <name>` then
renders the case file — when the damage started, how many senders and
episodes, the lifetime rows shelved, a sample of the values that were
nulled, and the repin command to run:

```
degraded pin:
  since:          2026-08-01T10:00:00.000000Z
  senders:        2
  episodes:       41
  rows shelved:   1290 (lifetime)
  sample values:
    - n/a
    - pending
  suggested:      VARCHAR
  trawl schema repin duration --to varchar --dry-run
```

`rows shelved` is the lifetime total; the `rows_nulled` column in the
conflict table below it sums only the evidence still inside the per-field
recency window, so the two differ on purpose. Every shelved value remains
in `_raw`. The verdict is advisory — nothing repins without `--yes` and
`schema_write`.

**Retiring a badge after fixing the sender.** The gate has no recency term:
evidence is never aged out, so a field stays badged after its shipper is
corrected. That is on purpose — the shelved rows are still missing from the
corpus. Clearing both is one command, the same-type pass:

```bash
trawl schema repin duration --to bigint --force --dry-run   # `bigint` = the current pin
trawl schema repin duration --to bigint --force --yes
```

A repin whose target equals the current pin is the **resurrection-only**
pass: it re-extracts the shelved values from `_raw` under the pin they
already have, and — because a successful repin clears the field's conflict
evidence in the same transaction as the flip — the badge goes out with the
damage it was reporting. Repinning to a *different* type does both as well.

`field` pages its service observations — the service axis is client-chosen
and never pruned, so the server caps a page at 1000 rows (default 100).
When more remain, a cursor is printed to stderr; pass it to `--after` for
the next page:

```bash
trawl schema field duration --limit 500
trawl schema field duration --limit 500 --after '2026-08-02T10:00:00.000000Z|nginx'
```

### Repin

`schema repin` changes a wrongly-pinned field's type by rewriting the
corpus (ADR-0011): affected files are rebuilt to the new type with
conflict-shelved values resurrected from `_raw`, unaffected files are
hardlinked, and the switch is atomic and crash-recoverable. It needs the
`schema_write` permission.

```bash
trawl schema repin status --to varchar --dry-run   # mandatory first look
trawl schema repin status --to varchar --yes       # execute (background job)
trawl schema repin status --to varchar --yes --wait  # poll to completion
trawl schema repin dur --to bigint --yes --force   # accept a lossy projection
trawl schema repin-status                          # the running/last job
trawl schema repin-cancel                          # ask the running job to stop
```

An executing repin confirms interactively; off a TTY it refuses without
`--yes`. A repin whose dry run projects nulled values refuses without
`--force` and prints the plan (the values it would null stay findable in
`_raw`). `--to <current type> --force` runs a resurrection-only pass.
All three commands honour `-f table|json|csv`.

Every report — dry run, running job, terminal job — carries
`requires_force`: whether the *identical executing* request would be
refused. A dry run succeeds by design, so without that column a plan
carrying loss or dialect ambiguity would read as a clean pass and the
refusal would arrive with the request that was meant to do the work.

#### Stopping a running repin

`schema repin-cancel` asks the running job to stop. It needs
`schema_write` and takes no confirmation prompt, because cancelling only
ever leaves the corpus as it already is. There are three answers, and the
exit code carries the verdict:

- accepted (exit 0): the job stops at the next file boundary of its scan or
  build loop, sweeps its staging, and ends `cancelled` with the live corpus
  and the pin unchanged. The snapshot walk and the filesystem preflight are
  not checkpointed, so a job inside one of those stops when it leaves it.
- past the point of no return (exit non-zero): the job is already swapping
  the corpus. The request is refused rather than queued, and the job
  completes.
- no job running (exit non-zero): nothing to stop on this node.

Acceptance is not a promise of a terminal `cancelled` status. A job that
finishes first finishes, and a trawld that dies between the request and any
boundary acting on it leaves the job `failed` with `cancel_requested_at`
and `cancelled_by` set. A restart is the stronger cancel: killing trawld
before the cutover leaves the live corpus untouched, and boot recovery
sweeps the shadow generation.

In `-f json` and `-f csv` the receipt is one record: the verdict, the
server's sentence, and the job's own columns, nulled when no job is
attached. `-f table` keeps the sentence and the job table as two blocks.

`repin --wait` exits non-zero when another actor cancels the job mid-wait.
The row prints as usual and the message names who asked; a script that read
that exit as success would go on to trust a rewrite that never happened. It
also exits non-zero when the status surface stops naming the job it is
following: there is no way to ask that route for a job by id, so a second
job claiming the freed slot leaves the first job's outcome unknown, and the
message says so rather than reporting the last row it saw.

#### Putting a sender's own field on the severity ladder

`--to severity` is the one target that changes what values *mean* rather
than only how they are stored: the field joins `_severity`'s vocabulary, so
`level=error` becomes a band match, `level>=warn` compares ladder
positions, and results render tokens.

```bash
trawl schema repin level --to severity --dry-run                    # plan first
trawl schema repin level --to severity --dialect syslog --dry-run   # sender speaks syslog PRI
trawl schema repin level --to severity --yes --force                # accept the plan
```

`--dialect` reads **numerals only** (tokens are dialect-free): `otel`
counts up 1-24, `syslog` counts down 0-7 and is inverted. The two ladders
overlap over 1-7 with opposite meanings — `3` is `trace3` to OTel and `err`
to syslog — and no value-shape rule can tell them apart, so trawl refuses
rather than guesses: a corpus carrying those numerals needs either
`--dialect syslog` or `--force`. The count of such rows is reported
whatever you assert (`ambiguous_numerals`); only the refusal depends on it.
`--dialect` with any other target is an error, not an ignored flag.

The report also names the values the new pin cannot read at all (up to five
distinct samples) and warns when something is **still writing** the field.
That warning matters: a repin translates **history**. After the cutover,
live events keep taking the ingest-time reading, so under `--dialect
syslog` a historical `3` becomes 17 (`err`) while the next live `3`
conforms as OTel 3 (`trace3`) — one column, two meanings, split at the
cutover instant. If the sender really speaks syslog PRI, declare it in
`[ingest] severity_from` (`dialect = "syslog"`) so live events read the
same way, then repin the history.

**What it costs.** The severity rung is the most expensive conform in the
vocabulary: it is a token table, an ASCII gate and a guarded numeric read
per value, measured at roughly **36 µs per affected row** — about 10× any
other target — so a 100-million-row field is on the order of one CPU-hour
of rewriting. Retention stands down for the job's whole life and the
affected bytes are held twice until it sweeps, so size the window before
starting: a repin that runs for hours is a repin that suppresses deletion
for hours. Unaffected files are hardlinked and cost nothing, so the number
that matters is `rows_carrying` in the dry run, not the corpus total.

`repin --to severity` needs `schema_write` and a human, like every other
repin. `_severity` itself — and every other declared envelope field — is
refused: its type is part of the event contract.

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
