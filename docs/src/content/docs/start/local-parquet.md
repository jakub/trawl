---
title: Query local Parquet
description: Inspect exported or existing Parquet files with the CLI and no server.
---

Local mode runs a query against files you name. It needs the `trawl` binary
and readable Parquet files, with no server, API key, PostgreSQL, or hot buffer.
It is useful for an exported incident dataset or an archive on your laptop.

## Start with a small result

Quote the path so Trawl receives the glob rather than your shell expanding it:

```bash
trawl query --data '/path/to/logs/*.parquet' '* | head 5'
```

Replace the example path with your files. The first result tells you their
column names. Foreign Parquet files do not automatically have Trawl's envelope
fields, so begin without `_time` or `_severity` assumptions.

If the files carry a `service` column:

```bash
trawl query --data '/path/to/logs/*.parquet' \
  '* | stats count() by service | sort -count'
```

A recursive glob includes nested directories:

```bash
trawl query --data '/path/to/archive/**/*.parquet' \
  '* | head 20' --format json
```

Narrow the file selection before running a large scan. Local mode has no
server row cap; use `head`, an aggregation, or an output file deliberately.

## Work with an exported Trawl dataset

The [first-query tutorial](/getting-started/first-query/#try-embedded-mode-no-server)
creates a Parquet export. Its canonical event time is `_time`. For an old
export, an absolute interval is often more useful than `last=1h`:

```text
earliest="2026-09-01T00:00:00Z" latest="2026-09-02T00:00:00Z" | stats count() by service
```

The lower bound is inclusive; the upper bound is exclusive. An export does
not bring the server's catalog pin snapshot with it. Do not assume every
pin-aware comparison or severity token expression retains the server's
interpretation in embedded mode. For a numeric exported `_severity` column,
`_severity>=17` selects ERROR and higher severity numbers.

## Save the result

```bash
trawl query --data '/path/to/logs/*.parquet' \
  '* | head 100' --format json --output selected.ndjson
trawl query --data '/path/to/logs/*.parquet' \
  '* | stats count() by service' --format csv --output services.csv
```

JSON output is one object per line. The output is data, not an HTTP response
wrapper. See [Sharing and export](/use/sharing-export/) for format choices.

## Know the boundary

This is a file query, not a connection to your server. It does not include
uncompacted events, saved-report storage, permissions, or the live field
catalog. Files with incompatible types can fail to combine; local mode does
not repair the corpus. Keep originals when investigating a damaged archive.

Use [server mode](/start/connect/) when you need current hot events,
catalog semantics, or live tail.
