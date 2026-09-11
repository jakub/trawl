---
title: Query local Parquet
description: Run queries against Parquet files on disk with the CLI and no server.
---

`trawl query --data` runs a query against files you name, with no server,
key, PostgreSQL, or hot buffer. Use it for an exported incident dataset or an
archive on your laptop.

## Look at the first rows

Quote the glob so `trawl` expands it, not your shell.

```bash
trawl query --data '/path/to/logs/*.parquet' '* | head 5'
```

Expect five rows and the footer `5 row(s)`. The column names tell you what to
filter on. Files from other tools have `_time` or `_severity` only if their
writer added them.

## Count by a column

If the files have a `service` column:

```bash
trawl query --data '/path/to/logs/*.parquet' \
  '* | stats count() by service | sort -count'
```

Expect one row per service, largest first. A `**` glob includes nested
directories:

```bash
trawl query --data '/path/to/archive/**/*.parquet' \
  '* | head 20' --format json
```

There is no server row cap here. Narrow the glob, add `head`, or aggregate
before a large scan.

## Query an exported Trawl dataset

[Your first query](/getting-started/first-query/#query-the-export-without-a-server)
creates a Parquet export. Its event time is `_time`. For an old export, use an
absolute interval instead of `last=1h`:

```text
earliest="2026-09-01T00:00:00Z" latest="2026-09-02T00:00:00Z" | stats count() by service
```

The start is inclusive and the end is exclusive. An export has no catalog, so
`_severity>=error` may not read as it does on the server. `_severity>=17`
compares the number and selects ERROR and above.

## Save the result to a file

```bash
trawl query --data '/path/to/logs/*.parquet' \
  '* | head 100' --format json --output selected.ndjson
trawl query --data '/path/to/logs/*.parquet' \
  '* | stats count() by service' --format csv --output services.csv
```

Expect no output on success. JSON is one object per line with no HTTP
envelope. [Share searches and export results](/use/sharing-export/) compares
the formats.

## Know what a local query leaves out

A local query reads files, not your server: no hot buffer, saved reports,
permissions, or field catalog, and no `from saved`. Files with incompatible
column types can fail to combine, and nothing rewrites them, so keep the
originals of a damaged archive. Use [a server connection](/start/connect/) for
events still in the hot buffer, catalog types, or live tail. The
[CLI reference](/reference/cli/#embedded-mode) calls this embedded mode.
