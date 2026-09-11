---
title: Search in the browser
description: Sign in, search logs, inspect fields, and move from an investigation to a saved report.
---

The browser UI is served by `trawl-web`, which connects your session to the
Trawl API. Use the browser URL supplied by your operator. For a newly installed
server, see [deployment](/operate/deployment/) and
[configuration](/reference/configuration/#web) to enable browser access.
The proxy listens on `127.0.0.1:8090` by default; that address is local to the
server, not necessarily the URL you should open on your laptop.

## Sign in and find your way

Sign in with your API key. Each action uses that key's permissions; opening a
browser session does not give the key additional capabilities.

| Page | Use it to |
| --- | --- |
| Search, `/search` | Run a snapshot query or watch incoming events |
| History, `/search/history` | Reopen an earlier investigation |
| Schema, `/search/schema` | Discover services and fields, inspect conflicts |
| Nets, `/jobs/nets` | Manage named saved queries and schedules |
| Runs, `/jobs/runs` | Inspect scheduled execution outcomes |
| Health, `/settings/health` | Inspect server health within your permissions |

Use the navigation to switch sections. The command palette provides another
way to reach pages and available actions; open it with Ctrl+K or Command+K.
If a page or action is unavailable, check permissions with the operator.

## Run a bounded search

Enter a query such as:

```text
service=nginx last=15m | head 20
```

Replace `nginx` with a service in your installation. Choose the time range,
check the sidebar filters, and select **Haul**. The editor is only part of the
search state: range and filter controls also affect the executed query.

Start with **Events** to read individual rows. Expand a row to inspect its
fields and raw event text. Use Schema when you do not know which field to
filter. The [query tutorial](/use/query-tutorial/) explains filters,
projections, ordering, and aggregations with a known dataset.

Changing editor text does not execute it or replace the existing result.
Run the changed query and check the completion state before interpreting its
rows. Read any truncated-result or degraded-field notice alongside the answer.
A successful zero-row query is different from a request that failed.

## Read a visualization

A timechart query produces time buckets rather than individual events:

```text
service=nginx last=1h | timechart span=5m count()
```

Select **Visualization** to view the chart, or Events to inspect its table.
For a live count chart, switch to Live Tail in the range control. Live event
queries belong in Events; see [Live tail](/use/live-tail/) for connection and
buffer limits.

## Continue an investigation

History reopens previous queries. **Save** gives a query a name for reuse in
Nets; it does not freeze the current rows. **Export** downloads query results
as CSV, JSON, or Parquet. Use an absolute time interval when sharing an incident
search so another reader does not silently investigate a later hour.

- [Saved queries and reports](/use/saved-reports/) explains schedules and runs.
- [Sharing and export](/use/sharing-export/) explains links and downloaded data.
- The sections below explain field conflicts and their browser remedies.

## Sharing search links

Copy the complete URL after running the search. Two parameters have stable
readable forms: `q` is query text, and `r` is either a quick label such as `1h`
or two UTC instants joined by `..`. The right-hand instant can be `now`.
The filter parameter `f` is an opaque payload; copy it rather than editing it.
Its encoding can change between releases.

A malformed filter, range, or page parameter blocks execution until you use
the offered repair. The browser also blocks Save, Export, Live Tail, range
changes, pagination, and filter controls while that notice is present. It
keeps the broken URL intact so you can report it to its sender.

URLs larger than 32 KiB or with more than 64 parameters are refused as a
whole, with **Start over** as the repair. The same limits apply when writing
search state into a URL. If a new search cannot fit, an error toast explains
the problem and the editor and existing results remain in place. Only `q`,
`page`, `mode`, `f`, and `r` are read as search parameters.

Use [Sharing and export](/use/sharing-export/) to choose between a link,
a saved query, and a file containing results.

## Degraded pins on the schema page

A degraded pin is a field whose stored type has been rejecting values over a
sustained period and in sufficient volume. The server supplies this verdict;
the browser does not infer it from the displayed rows.

On Schema, a service's badge counts degraded fields with conflict evidence
for that service. Merely carrying a column does not earn the badge. Open the
service drawer's Fields tab to see individual fields. The badge snapshot can
lag a completed repin by the schema refresh interval.

## The field case file

Select a badged field to open its case file. A direct link such as
`/search/schema?field=duration` works without selecting a service first.
Use the back arrow to return to the service when you arrived from its drawer.

The case file shows the field's pinned type, where it was observed, conflict
samples, and the suggested remedy. Read the counts with their labels:

- A conflict episode is a write that nulled at least one value, not a count of values.
- Rows shelved is a lifetime total; the windowed count describes a shorter period.
- The services list is paged and can include historical observations.
- Values lost from the typed column remain available in the event's `_raw` text.

A field with no pin gets an explicit untyped case file. A healthy field has no
degraded-pin remedy to apply. For the full catalog model and operator steps,
see [Catalog conflicts and repinning](/operate/catalog/).

## Repinning from the browser

Repinning changes a field across the entire corpus, including other services,
environments, and dates. It is not limited to the drawer's selected service.
The operation needs `schema_write`. Readers without that permission can
inspect the evidence and pass its link or suggested CLI command to an operator.

With permission, the browser presents a dry-run plan before execution:

1. Read the affected files and rows, projected nulls, recoverable values, and bytes.
2. Review any loss warning before confirming the real job.
3. Watch the job's status until it succeeds, refuses, blocks, or fails.

The plan is a scan of the current corpus, not a reservation. Ingest continues,
and execution scans again. A job can therefore refuse for projected loss even
when an earlier dry run looked safe. A forced operation requires explicit
acceptance of its loss bounds; review the displayed counts before confirming.

Only one repin runs at a time. When another field holds that slot, follow its
case-file link to inspect it. Closing the drawer stops its polling, not the
server job. If a request or status poll fails, inspect the running or last job
before attempting another operation. A transport error does not prove the
server did nothing.

## The incomplete-results notice

A query that uses a degraded field can show a notice above its results. It
means some original values did not fit the pinned type and now read as NULL.
Filters, grouping, and sorting can therefore omit or combine rows differently
from what the original source data would suggest.

Names link to their case files when your session has `schema_read`. Dismissing
the notice hides it for that query and field set; it does not fix the data.
Paging the same result preserves the dismissal. A new query or changed field
set can show it again.

The notice describes the execution that produced the current rows. A later
catalog change does not rewrite that result's warning. Run a new query to
check the new state. Live tail does not carry this batch-query notice, so its
absence in a live view is not proof that all fields are healthy.
