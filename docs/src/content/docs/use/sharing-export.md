---
title: Share searches and export results
description: Pick between a search link, a saved query, and a downloaded file for the answer you need to pass on.
---

A link describes a search. A saved query stores text. An export holds the rows
one execution returned. Pick by what the other person needs: the question, the
exact interval, or the data you saw.

## Share a browser search

Run the search, then select **Share**. Expect `Search URL copied to clipboard.`
The link carries the query, range, page, mode, and sidebar state, but no API
key, so the recipient needs their own sign-in and permissions on the same
server.

Use an absolute range for incident review, because a relative `1h` means the
hour before the recipient runs it. Even then, retention, late events, or a
catalog change can alter the answer.

In the URL, `q` is the query, `r` is a quick range label or two UTC instants
joined by `..` with `now` allowed on the right, and `f` is the sidebar state.
Copy `f` intact. The [search-link contract](/reference/web-ui/#sharing-search-links)
lists the limits.

A malformed-link notice never runs a wider query instead. Use the repair it
offers, such as **Drop filters** or **Start over**, and keep the original URL
for the sender.

## Export from the browser

1. Run a bounded search.
2. Select **Export** and check the query in the dialog.
3. Choose **CSV**, **JSON**, or **Parquet** and download.

Export runs the effective query again, so the file can differ from the screen
if events arrived in between. Set a fixed range and a `head` limit when the
file must stay bounded. The export row limit can differ from the query limit.
A failed download is not a complete dataset.

## Export from the CLI

Name the server. These commands assume a `lab` profile and a service called
`nginx`:

```bash
trawl --profile lab query 'service=nginx last=1h | head 1000' \
  --format json --output nginx.ndjson
trawl --profile lab query 'service=nginx last=1h | head 1000' \
  --format csv --output nginx.csv
trawl --profile lab query 'service=nginx last=1h | head 1000' \
  --format parquet --output nginx.parquet
```

Expect three files and nothing on stdout. Parquet requires `--output`. JSON is
one object per line, so `jq` can stream it. CSV prefixes a string that starts
with `=`, `+`, `-`, `@`, a tab, or `|` with an apostrophe so a spreadsheet does
not run it as a formula. Choose JSON or Parquet when the exact text matters.

## Keep the context with the file

Record the query, the absolute interval, the server, and the execution time
next to an incident export. The CLI prints the degraded-pin notice only in
table output, and a JSON, CSV, or Parquet file carries no warning. Run the
same query as a table, or read `degraded_fields` in the [API response](/reference/api/),
before you trust the file.

An export can hold raw event text, identifiers, and secrets, and Trawl's
permissions do not apply to it, so share only the rows and fields the
recipient needs. [Query local Parquet](/start/local-parquet/) inspects a
downloaded file without a server. [Saved reports](/use/saved-reports/) keep
scheduled results in Trawl.
