---
title: Share searches and export results
description: Choose between a repeatable search link, saved query, and downloaded result file.
---

A link describes a search, a saved query stores text for reuse, and an export
contains rows from an execution. Choose based on what another person needs:
the question, the exact interval, or the data you observed.

## Share a browser search

Run the search, then copy its full address. The recipient needs access to the
same browser server and permissions to run it. The link carries the query,
time range, page, mode, and encoded sidebar state. It does not carry your API
key or grant access to the result.

Use an absolute start and end for incident review. A relative `1h` range means
the hour before the recipient runs it, which can be a different set of events.
Even an absolute interval can produce a different answer after retention,
new late-arriving events, or catalog changes.

The `q` parameter is query text; `r` is a quick range label or two UTC instants
joined by `..`, with `now` allowed on the right. The sidebar state in `f` is
opaque: copy it intact rather than constructing it by hand. See the
[search-link contract](/reference/web-ui/#sharing-search-links) for limits.

If Trawl shows a malformed-link notice, it does not execute a broader fallback
query. Read the notice and use the offered repair before running the search.
Keep the original URL if you need to explain what was wrong to its sender.

## Export from the browser

Run a snapshot search, select **Export**, inspect the query, choose CSV, JSON,
or Parquet, and download. Export executes the effective query; it is not a
copy of just the visible page. Results can change between the displayed
search and the export if new events arrive.

Use a fixed interval and a deliberate query limit when the file must be
bounded. Server export limits can differ from interactive query limits, and
a failed download should not be treated as a complete dataset.

## Export from the CLI

Select your server explicitly. These commands assume a configured `lab`
profile and a service named `nginx`:

```bash
trawl --profile lab query 'service=nginx last=1h | head 1000' \
  --format json --output nginx.ndjson
trawl --profile lab query 'service=nginx last=1h | head 1000' \
  --format csv --output nginx.csv
trawl --profile lab query 'service=nginx last=1h | head 1000' \
  --format parquet --output nginx.parquet
```

Parquet requires an output path. JSON contains one object per line, so it can
be processed incrementally with `jq`. CSV protects spreadsheet consumers by
prefixing formula-like string values with an apostrophe. That protection
changes the exported string representation; choose JSON or Parquet when the
original machine-readable values matter.

## Preserve context with the file

Record the query, absolute interval, server, and execution time beside an
incident export. Also retain any warning about truncated or degraded results.
The absence of a prose warning in a machine-readable output file does not
prove the server had no advisory information.

Exports can contain raw event text, identifiers, and secrets from the source
logs. Share only the selected rows and fields with the intended recipients.
An exported file no longer depends on Trawl's access controls.

Use [local Parquet mode](/start/local-parquet/) to inspect a downloaded file
without server access, or [saved reports](/use/saved-reports/) when you need
Trawl to retain scheduled execution results.
