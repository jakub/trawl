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

The sidebar groups application pages into three sections and has a bottom
utility link. The first section has no heading.

| Group | Item | Route | Purpose |
|-------|------|-------|---------|
| No heading | **Search** | `/search` | Run a snapshot query or stream live events |
| No heading | **History** | `/search/history` | Reopen an earlier query |
| No heading | **Schema** | `/search/schema` | Browse services and fields, inspect conflicts |
| Scheduled work | **Nets** | `/jobs/nets` | Manage saved queries and their schedules |
| Scheduled work | **Runs** | `/jobs/runs` | Inspect scheduled run outcomes |
| Operations | **Health** | `/settings/health` | Read server health and running queries |
| Bottom utility | **Help** | [Documentation](https://trawl.sh/) | Open the documentation site in a new tab |

The command bar shows the current page, **Go to…**, and the account menu.
**Collapse sidebar** reduces the sidebar to icons, and **Expand sidebar**
restores its labels. In compact navigation, **Open navigation** opens the
sidebar as an overlay.

**Go to…**, Ctrl+K, or Command+K on macOS opens the command palette. The palette
lists the application pages and filters on both label and route.
The sidebar's bottom item, **Help**, opens the documentation site in a new tab.

`/login` takes an API key. A rejected key reports `Invalid API key`. If the
initial session check or Jobs polling detects an expired session, sign-in
carries the requested page in an encoded `return_to` parameter. Successful
sign-in returns to that protected Trawl page with its query and fragment
intact. Failed sign-in and reloading the sign-in page retain the destination.
Both automatic sign-in and the return replace their history entries, so Back
does not stop at sign-in.

Direct sign-in without a valid destination opens Search. Sign out opens bare
`/login`, so the next sign-in also opens Search. The destination uses the new
key's permissions. Only URL state survives: unsaved edits and interrupted
actions do not return, and relative time expressions remain relative.

The sign-in URL can contain query text and an encoded fragment, including in
request logs or a copied link. It does not contain the API key. Return
destinations are limited to 64 KiB; the outer sign-in query is limited to
three times that size plus 64 bytes. Encoding increases the request URI, so a
reverse proxy can reject a long sign-in URL even when it accepted the original
page URL. These client limits do not guarantee acceptance by every deployment.

## Search

| Control | Effect |
|---------|--------|
| Editor | Holds the in-progress query. Editing it executes nothing |
| Date range | Selects `5m`, `15m`, `1h`, `4h`, `24h`, `7d`, or an absolute interval |
| **Live Tail** | In the date-range popover. Streams the editor's query over SSE |
| **Stop live** | Beside the tabs, in live mode only. Closes the stream and runs the same query once, with the filters and the range unchanged |
| **Haul** | Executes the editor's query. Ctrl+Enter and Command+Enter do the same |
| **Save as Net** | In the console. Opens a dialog to name and save the editor text as a net |
| **Copy search URL** | Beside the editor. Copies the current URL to the clipboard |
| **Format** | Reformats the editor text. A query with parse errors is left alone |
| **Filters** sidebar | Field values counted over the snapshot page or live buffer. Aggregate results hide the field-value groups; the active-filter count and **Clear all** remain. **Include** and **Exclude** add a filter, **Clear all** removes every filter |
| Filter chips | Above the tabs. Selecting a chip removes that filter |
| **Events** tab | The result table, with the row count in the tab. An aggregation renders the exact table and, where the shape allows it, the categorical chart beside it |
| **Visualization** tab | The chart for a `timechart` or `stats … by` result, as Line, Column, or Bar. It has no pager: it draws a whole result or states why it cannot |
| **View** | In the results header. Chooses how snapshot events and their details appear |
| **Save** | In the results header. Opens the same dialog as **Save as Net** and captures the editor text |
| **Export** | Downloads the results as CSV, JSON, or Parquet |
| **← Prev**, **Next →** | Under the Events tab's table. Page through results, 50 rows per page |

An aggregation is fetched whole, up to 20,000 rows, and paged in the browser.
**← Prev** and **Next →** slice the result already in hand, so turning one of
its pages sends no request. When the execution produced more rows than one
fetch carries, a line under the pager states both counts. Raw event results
page the other way: each page is its own request.

A control on the Visualization tab picks the chart type: Line, Column, or
Bar. The default follows the query: Line for a `timechart`, Column for a
`stats … by`. A type that does not fit the current result is disabled.

Line draws the x axis as the bucket instant parsed from `_time`, in UTC. It
draws one line per metric column, or one line per group value when the
query groups with `by` fields and has one metric. A bucket with no row for a
series is a gap, not a zero. A result with more than six series draws the
six largest by total, with a caption stating how many series it left out.

Column and Bar each draw one bar per group. A result with more than twenty
groups draws the twenty largest by value, with a caption stating how many
groups it left out.

Every refusal names the fix or offers to open Events instead.

The Visualization tab draws the whole result or nothing. A response that is
not the whole result is refused with the counts the server measured, because
the chart of one page is a different picture from the chart of the result. See
[ADR-0037](https://github.com/jakub/trawl/blob/main/docs/adr/0037-the-chart-draws-the-whole-result-or-nothing.md)
and
[ADR-0038](https://github.com/jakub/trawl/blob/main/docs/adr/0038-the-visualization-tab-draws-time-and-groups.md).

Both Save controls omit the range control and sidebar filters. They save the
editor text, not the effective query or returned rows.

**Edited** appears when the editor differs from the executed query. The strip
below the console shows the accepted snapshot's rows returned, **Execution in**
duration, and **Started** time in UTC. Later editor changes do not change those
execution facts.

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

### Result presentations

The results area has these presentations:

- **Quick start** appears in both results tabs before a snapshot query runs. It has four examples, sender and reserved field references, and severity bands. Each example's **Run** executes it with the selected range and filters, then opens **Events**.
- **View** offers **Inline** or **Inspector** for event details, and **Compact** or **Message first** for rows. Inline expands details in the table. Inspector opens an event panel. Message first leads with time, severity and a wide message column, with service, host and latency below the message when present. Other fields remain in the event details. Without a `message` or `msg` column, the table stays compact. Inline and Compact are the defaults. These choices apply to snapshot events only, not live or aggregate results.
- The histogram above snapshot events groups the current page's rows over their own time span. It does not count all matches in the selected range or measure ingest volume. It is absent for live and aggregate results.
- For a snapshot aggregate, **Events** shows an exact table of group and metric columns, without row expansion. Only grouped fields offer a search action. A `stats` result with one group column and one numeric metric also has a chart with one bar per group beside the table. Other aggregate shapes show the table alone.

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
**Live Tail**, **Save as Net**, **Save**, **Export**, pagination, range changes,
and the filter controls.

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

`/search/history` lists your key's completed queries with **Executed query**,
**When**, **Rows**, and **Action**. The headers do not sort. The filter searches
stored query text across
all retained history for your key before paging. Matching is a literal
substring using the database's lowercase rules; whitespace, `%`, and `_` remain
significant. **Save as Net** stores a row as a saved query.

Typing edits a draft. **Search** or Enter outside IME composition applies it.
A changed filter starts at page one and adds a browser history entry.
Submitting unchanged text refreshes the current page. **Clear filter** applies
an empty filter at page one. Paging keeps the applied filter, discards an unsubmitted
draft, and replaces the URL's page number.

The applied filter is visible in the URL as `hq`; `hpage` is the zero-based
page number. Generated URLs omit an empty filter and page zero. Bookmarks,
reload, Back, and Forward restore that view over the current key's changing
history. They do not preserve a result snapshot. Filter text in the URL is
also present in browser history and any copied link.

History accepts at most 32768 bytes of URL query payload and 64 nonempty
parameter pairs, with strict percent and UTF-8 decoding. Generated filter
URLs reserve room for the largest admitted page number. An oversized draft
stays editable and sends no request. A malformed History link shows an error
and recovery action without fetching history. These browser URL bounds are
separate from the API's decoded filter limit.

A new filter or page hides previous rows while loading. Refreshing the same
view can retain its rows, but paging and export stay disabled until that
refresh succeeds. Empty history, no filter matches, and a page beyond the
matches have separate messages. **Export this page** downloads only the
successfully loaded applied page as CSV or JSON; an unsubmitted draft does
not change the export.

**Clear history** asks for confirmation, then deletes every history row for
your key, including unmatched rows and other pages. Saved queries remain.
While deletion is pending, filter input, Search, Clear filter, paging, export,
and duplicate Clear are disabled. Success resets the filter and page, replaces
the URL with `/search/history`, and refreshes without restoring old rows if
that refresh fails. A failed or unknown deletion outcome preserves the view
and reports failure. Queries completed concurrently can add new history.

## Schema

`/search/schema` lists services with **Service**, **Activity**, **Events**,
**Storage**, and **Fields** columns, followed by icon actions. The actions
column is named **Actions** for screen readers. Its magnifying-glass **Search**
button and lightning-bolt **Live tail** button open that service in Search or
start its live stream. **Service**, **Events**, **Storage**, and
**Fields** have sorting controls. The filter box matches service names and field names.

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

`/jobs/nets` lists saved queries with **Name**, **Schedule**, **Last run**, and
**Actions**. **Name** and **Last run** have sorting controls. A row's drawer offers
**Rename**, **Edit** for the query text, **Open query in search**,
**Trigger a scheduled run now**, and **Runs**. **Delete** asks for confirmation.

The schedule block sets **Run every**, **Each run covers**, and
**Keep schedule running for**. The last field is a run count, with blank meaning
unlimited. **Schedule enabled** switches between **Active** and **Paused**.
**Save schedule** applies the form, and the schedule can also be removed.

**Each run covers** has three modes. **Query text** runs the saved text as written.
**Since last run** covers from the previous run's covered point to the run time.
**Fixed span** covers the **Trailing span** below it, measured from the run time.
Both windowed modes take a **Late-arrival lag**, which moves both window bounds
back by that much so late events can land first; blank is none. A save the server refuses
appears under **Save schedule** in the server's own words, and the form keeps
what you entered.

**Trigger run** is absent for a net whose saved schedule carries a window,
because the scheduler owns every windowed run: a since-last-run schedule would
have its coverage point moved out of band, and a fixed-span run outside the
cadence covers a span the schedule never asked for.

Expanding a run in **Runs** shows the stored result 20 rows to a page, with a
pager under the table. Paging works on the rows the response carried and issues
no further request. When the run stored more rows than the response carried, a
line under the pager gives both counts. See
[saved queries and reports](/use/saved-reports/).

## Runs

`/jobs/runs` summarizes **Recorded runs**, **Success rate**, and **Average duration**,
then lists recent runs with **Net**, **Status**, **When**, **Duration**, and
**Rows**. All five table columns have sorting controls. A filter box narrows
the list to one net.

## Health

`/settings/health` reports server checks, capacity, ingestion, storage, and
queries. **Refresh** re-reads server health and capacity. **Refresh queries**
re-reads Queries. The diagnostic cards receive the shell's shared dashboard
stream; those buttons do not trigger a storage scan or open another stream.

| Section | Permission | Contents |
|---------|------------|----------|
| Server health | any signed-in key | Overall state, version, and each named check |
| Capacity | `server_manage` | Uptime, queries since startup, active queries, executors available, and retained work |
| Live operations | `server_manage` | Host, HTTP ingest rate, query rate, hot buffer events and memory, and executors occupied |
| Ingestion | `server_manage` | HTTP rejected events, syslog configuration, transport receive counts, syslog rate, parse errors, backpressure drops, and active TCP connections |
| Storage | `server_manage` | WAL and ingested Parquet totals with measurement status and age, successful compaction cycles, error tally, and last successful cycle age |
| Queries | `query` | Running and recent queries with user, state, and elapsed time, plus authorized cancellation controls |

Server health, Capacity, and Live operations precede Ingestion and Storage.
Ingestion and Storage use two columns on wide screens and stack on narrow screens.
Queries follows at full width.

### Ingestion and compaction facts

HTTP counts and rates exclude syslog. Cumulative ingestion and compaction
counters are since process startup. **Syslog configuration** reports configured
enablement, not listener health. **Disabled** hides the syslog readings;
enabled but idle syslog shows zero readings. Transport receive counts do not
prove that the messages reached persistent storage.

WAL totals include active files and do not count only compaction-eligible work.
Ingested Parquet totals exclude saved report files. **Successful compaction
cycles** can include cycles with no eligible work. **Compaction error tally**
includes failed cycles and loss/error tallies, can accompany successful cycles,
and can exceed their count. These historical totals do not establish an active
incident or a failure percentage.

**Last successful cycle** displays `0s ago` when the reported age is zero.
A missing age displays **No successful cycle reported since startup**.

### Measurement status and stream freshness

Storage displays **Not configured** for an absent optional source and
**Awaiting measurement** before a configured source completes its first attempt.
A complete scan displays its totals, including a measured zero. A failed scan
shows the last complete totals and their sample age if a sample exists.
Otherwise, it shows **Measurement unavailable; collection failed**.
The precise wire contract is in the
[dashboard API reference](/reference/api/#dashboard-snapshot).

Sample age is relative to the displayed dashboard snapshot. The page does not
advance that age with a client timer. Non-live stream labels appear beside both
sections. **Waiting for first snapshot** has no fabricated readings.
**Snapshot; waiting for live updates** identifies bootstrap data.
**Stale; reconnecting** and **Live updates failed** retain available readings.
**Forbidden** has no readings. An identity change clears the prior dashboard
state and invalidates its pending callbacks.

A live stream does not prove a recent disk measurement. Collection failure and
stream failure are separate facts. The cards link to
[ingestion guidance](/operate/ingestion/),
[storage recovery](/architecture/recovery/), and
[retention guidance](/operate/retention/).

Retained work occupies executors and counts toward pool usage. A cancellation
that reports no work cancelled means the query had already finished.
