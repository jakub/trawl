---
title: Search in the browser
description: The browser pages, their controls, and what each control does.
---

`trawl-web` serves the browser UI and turns your session into requests against
the Trawl API. Every action uses your API key's permissions, and a browser
session grants none of its own. The proxy listens on `127.0.0.1:8090` by
default, which is an address on the server, not the URL you open. See
[configuration](/reference/configuration/#web) to enable browser access, and the
[query tutorial](/use/query-tutorial/) to learn the query language.

## Navigation

The top bar carries three mode tabs. Each mode has its own left rail.

| Mode | Rail item | Route | What the page does |
|------|-----------|-------|--------------------|
| **Search** | **Search** | `/search` | Run a snapshot query or stream live events |
| **Search** | **History** | `/search/history` | Reopen an earlier query |
| **Search** | **Schema** | `/search/schema` | Browse services and fields, inspect conflicts |
| **Jobs** | **Nets** | `/jobs/nets` | Manage saved queries and their schedules |
| **Jobs** | **Runs** | `/jobs/runs` | Inspect scheduled run outcomes |
| **Settings** | **Health** | `/settings/health` | Read server health and running queries |
| **Settings** | **Schema** | `/search/schema` | The same schema page |

Ctrl+K, or Command+K on macOS, opens the command palette, which lists the mode
tabs and the rail sections and filters on both label and route. The rail's
bottom item, **Help**, opens the documentation site in a new tab.

`/login` takes an API key. A rejected key reports `Invalid API key`, and a
successful sign-in lands on `/search`.

## Search

| Control | Effect |
|---------|--------|
| Editor | Holds the in-progress query. Editing it executes nothing |
| Date range | Selects `5m`, `15m`, `1h`, `4h`, `24h`, `7d`, or an absolute interval |
| **Live Tail** | In the date-range popover. Streams the editor's query over SSE |
| **Stop live** | Beside the tabs, in live mode only. Closes the stream and runs the same query once, with the filters and the range unchanged |
| **Haul** | Executes the editor's query. Ctrl+Enter and Command+Enter do the same |
| **Save** | Names the query and stores it as a net. It does not freeze the rows |
| **Share** | Copies the current URL to the clipboard |
| **Format** | Reformats the editor text. A query with parse errors is left alone |
| **Filters** sidebar | Field values with counts. **Include** and **Exclude** add a filter, **Clear all** removes every filter |
| Filter chips | Above the tabs. Selecting a chip removes that filter |
| **Events** tab | The result table, with the row count in the tab |
| **Visualization** tab | The chart for a `timechart` or aggregate query |
| **Export** | Downloads the results as CSV, JSON, or Parquet |
| **← Prev**, **Next →** | Page through snapshot results, 50 rows per page |

Expanding a row shows its fields and raw text, and carries **Copy \_raw**,
**Show context**, and **Find similar**. Selecting a field tag adds an include
filter for that value.

The URL carries live mode as `mode=live`. **Stop live** and any range
selection leave live mode, including a repeat of the range already selected.
**Haul** and the filter controls keep the current mode. **Stop live** is a
history entry, so the browser Back button returns to the stream.

The status bar names the source it counted. In snapshot it reads **Last** and
the number of rows the page returned. In live it reads **Received** and the
number of events the browser buffer accepted since the stream opened, which is
not the number of messages the server sent, or, for an aggregation, **Updates**
and the number of snapshots. The **Events** tab counts the rows on screen in
both modes.

A live stream has no page count and no incomplete-results notice. See
[live tail](/use/live-tail/) for its limits and
[sharing and export](/use/sharing-export/) for the choice between a link, a
saved query, and a file.

### Sharing search links

The page reads five URL parameters and ignores the rest.

| Parameter | Value |
|-----------|-------|
| `q` | Query text |
| `r` | A quick label such as `1h`, or two UTC instants joined by `..`. The right-hand instant can be `now` |
| `f` | The filter payload. It is opaque, so copy it rather than edit it |
| `page` | Zero-based page index |
| `mode` | `live` for a stream. Any other value is a snapshot |

Copy the whole URL after the search has run. A malformed `f`, `r`, or `page`
blocks execution until you apply the offered repair, and the broken URL stays
intact. While that notice is present, the page also blocks **Haul**,
**Live Tail**, **Save**, **Export**, pagination, range changes, and the filter
controls.

A URL larger than 32 KiB, or one with more than 64 parameters, is refused whole,
with **Start over** as the repair. The same limits apply when the page writes
state back into the URL. A search too large to write leaves the editor and the
existing results in place and reports the problem in a toast.

### The incomplete-results notice

A query that binds a degraded field shows a notice above its results. Some
original values did not fit the pinned type and now read as NULL, so filters,
grouping, and sorting can omit or combine rows differently from the source data.

- Each field name links to its case file when your session holds `schema_read`. Without that permission the names are plain text.
- Dismissing the notice hides it for that query and field set. Paging keeps the dismissal. A new query or a changed field set can show it again.
- The notice describes the execution that produced the rows under it. A later catalog change does not rewrite it.
- Live tail carries no such notice, so its absence is not proof that every field is healthy.

## History

`/search/history` lists completed queries with **When**, **Query**, **Events**,
and **Duration**, and a filter box narrows the list. **Save as net** stores a
row as a saved query. **Export this page** downloads the visible page as CSV or
JSON. **Clear history** asks for confirmation, then deletes every row for your
key.

## Schema

`/search/schema` lists services with their activity, earliest and latest dates,
event count, storage, field count, and average coverage. Column headers sort,
and the filter box matches service names and field names.

Selecting a service opens its drawer: **Overview** for ingest rate and field
types, **Fields** for the per-field table of type, non-null share, cardinality,
storage, and a sample, and **Live Tail** for a bounded stream of that service's
events.

A service's badge counts degraded fields with conflict evidence for that
service. Carrying a column alone does not earn the badge, and the badge snapshot
can lag a completed repin by the schema refresh interval.

### The field case file

Selecting a field opens its case file, which shows the pin, who pinned it and
when, the health verdict, conflict samples, the suggested type, and the services
carrying the field. `/search/schema?field=duration` opens it directly.

- A conflict episode is a write that shelved at least one value, not a count of values.
- The shelved total is a lifetime figure. The windowed count covers a shorter period.
- The services list is paged and can include historical observations.
- A shelved value stays available in the event's `_raw` text.

A field with no pin gets an explicit untyped case file, and a healthy field has
no remedy to apply.

### Repinning from the browser

**Repin this field** needs `schema_write`. Without it, the case file offers the
equivalent `trawl schema repin` command. A repin changes the field across the
whole corpus, including other services, environments, and dates.

The browser presents a dry-run plan first: files affected, rows carrying the
field, values the new pin cannot keep, values that would come back from `_raw`,
and bytes to rewrite. **Get plan** and **Run repin** become **Get forced plan**
and **Run forced repin** when the projection is lossy, and a forced run needs
you to accept its loss bounds.

The plan is a scan of the corpus as it stands, not a reservation, so a job can
refuse for projected loss after a dry run looked safe. One repin runs at a time.
Closing the drawer stops its polling, not the server job. See
[catalog administration](/operate/catalog/) for the procedure.

## Nets

`/jobs/nets` lists saved queries with **Name**, **Query**, **Schedule**,
**Last run**, and **Created**. A row's drawer offers **Rename**, **Edit** for the
query text, **Open query in search**, **Trigger a scheduled run now**, and
**Runs**. Its schedule block sets **Interval** and **Max runs**, switches the
schedule between **Active** and **Paused**, and removes it. **Delete** asks for
confirmation. See [saved queries and reports](/use/saved-reports/).

## Runs

`/jobs/runs` summarizes **Active nets**, **Success rate**, and **Avg duration**,
then lists recent runs with **Net**, **Status**, **When**, **Duration**, and
**Rows**. A filter box narrows the list to one net.

## Health

`/settings/health` reports the server's state and checks. **Refresh** re-reads
every section on the page.

| Section | Permission | Contents |
|---------|------------|----------|
| Health | any signed-in key | Overall state, version, and each named check |
| Capacity | `server_manage` | Uptime, queries since startup, active queries, executors available, and retained work |
| Live operations | `server_manage` | A streamed snapshot: host, ingest rate, query rate, hot buffer events and memory, and executors occupied |
| Queries | `query` | Running and recent queries with user, state, and elapsed time. **Cancel** stops one you may cancel |

Retained work occupies executors and counts toward pool usage. A cancellation
that reports no work cancelled means the query had already finished.
