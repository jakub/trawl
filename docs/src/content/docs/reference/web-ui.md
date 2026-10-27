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
| No heading | **Investigations** | `/investigations` | Follow and work the case files of agents and people |
| Scheduled work | **Nets** | `/jobs/nets` | Manage saved queries and their schedules |
| Scheduled work | **Runs** | `/jobs/runs` | Inspect scheduled run outcomes |
| Operations | **Health** | `/settings/health` | Read server health and running queries |
| Bottom utility | **Help** | [Documentation](https://trawl.sh/) | Open the documentation site in a new tab |

**Investigations** appears only for a key that holds `query` and
`investigation_read`.

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
| **Filters** rail | Field values counted over the snapshot page or live buffer. **Include** and **Exclude** add a filter, and **Clear all** removes every filter. At 900px and wider, the rail opens by itself for a page with field values to count. Otherwise it collapses to a 32px strip at the left of the results, which shows the active-filter count. A press on the strip or the rail's header opens or closes the rail, and that choice holds until you reload the page or sign out. An aggregate result has no field values to count. An open rail with nothing to count says so, and keeps the active-filter count and **Clear all**. Below 900px, the rail is a disclosure above the results that opens only by hand |
| Filter chips | Above the tabs. Selecting a chip removes that filter |
| **Events** tab | The result table, with the row count in the tab. An aggregation renders the exact table and, where the shape allows it, the categorical chart beside it |
| **Visualization** tab | The chart for a `timechart` or `stats … by` result, as Line, Column, or Bar. It has no pager: it draws a whole result or states why it cannot |
| **View** | In the results header. Chooses how snapshot events and their details appear |
| **Export** | Downloads the results as CSV, JSON, or Parquet |
| **Send to investigation** | Beside **Export**, in snapshot mode only, for a key that holds `investigation_write`. Captures the executed search into an open investigation you choose. See [Send a search to an investigation](#send-a-search-to-an-investigation) |
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

**Save as Net** omits the range control and the active filters. It saves the
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

### Query errors

Two sources report a wrong query, and neither speaks for the other.

The **draft diagnostic** is the browser's own parser reading the editor text
as you type. The first error appears in full on a line under the editor, as
`Line 1:35 — found 'h', expected …`. When the text has more errors, a
**+N more** disclosure under that line lists them. It starts closed and closes
again when the text changes. A red marker in the editor gutter marks each line
with an error. Screen readers name the marker "Query syntax error". F8 moves the
cursor to the next error. The draft diagnostic never blocks **Haul**.

The **query error notice** is the server's answer to a query it refused as a
parse error or a validation error. It replaces the table on **Events** and the
chart on **Visualization**. Its first line reads `Couldn't run the query:`
followed by the server's message, with any hint after an em dash. Under a
**Query sent to server** label, it quotes the query text the server received,
with a caret under the part the error names. That text includes the range and
filters folded into the query, so it can differ from the editor text. A
multi-line query shows only the line with the error, numbered. When the server
reports more than one error, the first line counts them and each error gets
its own message and excerpt, in the server's order. A validation error, such
as an unknown function, names no position and shows no excerpt. The notice
has no **Retry**: sending the same text again gets the same answer.

When the server refuses a snapshot as `corpus_recovering`, both results tabs
show a recovering notice instead of the table or chart. Its title reads
**Search is recovering**, the server's sentence follows, and **Retry** sends
the same request again. The status bar reads **Recovering**, not **Error**, and
**Hauling** while the Retry runs. The server is still loading data from before a
restart, or finishing an interrupted storage rollup, so an empty table would be
wrong. Live mode reads only the event stream, which this refusal does not
touch. See [reads while the corpus is unsettled](/architecture/data-flow/#reads-while-the-corpus-is-unsettled).

When the server answers 503 `query_resource_exhausted`, the page shows the
server's sentence as a notice with no syntax underline, because the text is
fine. It does not retry on its own and offers no **Retry**, because the same
request spends the same budget again, and it does not claim the result is
empty. Narrow the time range or simplify the query. The export dialog shows
the same notice when an export is refused this way, and stays open. See
[query resource exhausted](/reference/api/#query-resource-exhausted).

For every other failure, **Events** shows `Couldn't load results:` with the
server's message or status, and a **Retry** button. **Visualization** shows
**Snapshot query failed. Open Events for the error.** with **Retry
snapshot**.

In live mode the browser cannot read why the server refused a stream. When a
stream closes before it opens and the browser's parser also rejects the text
it sent, the notice reads `Couldn't start the live stream. The query has a
syntax error:` followed by the parser's message and excerpt, with no
**Retry**. Any other live failure, including a validation error and a stream
that closes after it opened, shows **Live stream unavailable. Retry or switch to
Snapshot.** with **Retry live stream**. See
[ADR-0039](https://github.com/jakub/trawl/blob/main/docs/adr/0039-query-errors-server-verdict-on-the-sent-text-local-marks-on-the-draft.md).

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
**Live Tail**, **Save as Net**, **Export**, pagination, range changes,
and the filter controls.

A URL larger than 32 KiB, or one with more than 64 parameters, is refused whole,
with **Start over** as the repair. The same limits apply when the page writes
state back into the URL. A search too large to write leaves the editor and the
existing results in place and reports the problem in a toast.

### Send a search to an investigation

**Send to investigation** sends the search you ran into an
[investigation](#investigations). trawld runs it again as a capture under your
key and attaches the capture as a step, so the server, not the browser, attests
the rows. The capture's result becomes kept evidence of that investigation.

The control appears only when all of these hold:

- Your key holds `query`, `investigation_read`, and `investigation_write`.
- The page is in snapshot mode. Live mode has no control.
- A snapshot was accepted and the strip below the console shows its execution
  facts.
- The search does not read `from saved`. A saved report's rows have no time
  bounds to capture again.

The dialog takes the accepted execution when it opens: the executed query, the
filters, and the range. Later editor changes do not change what it sends. The
dialog lists the open investigations by title and short id, and sends nothing
until you choose one. It has no default choice. When no investigation is open,
it says so: an agent opens one through `trawl mcp`. **Purpose** is optional,
up to 1024 bytes.

The range decides what the dialog sends:

| Range | What is sent |
|-------|--------------|
| Absolute, both ends fixed | The query and filters, with the window `[from, to + 1 µs)`. An event exactly at `to` is inside, as it was in Search. The dialog sends at once |
| An absolute `earliest="…"` and `latest="…"` in the query text | The merged query text. The clauses bound the capture, and no window is sent |
| Any other time clause in the query text, such as `last=1h` or an `earliest=` with no `latest=` | Nothing. The query's own range moves, and the range control cannot pin it. The dialog says to remove the clause and pick the range in the range control, and has no **Send** button |
| Quick range such as `15m`, or an absolute range that ends at `now` | Nothing yet. The dialog says the range moves, and its button reads **Pin and re-run** |
| A range whose capture would cover more than the server's `[capture] max_span_secs`, 7 days by default, such as the pinned **7d** preset | Nothing. The dialog says the range is longer than one capture may cover and to pick a shorter one. It never shortens the range for you |

To pin a moving range, the dialog takes the **Started** instant of the accepted
snapshot, which is the server's clock and never the phone's. A quick range of
length `d` becomes `[Started − d, Started]`, and an open end becomes
`Started`. Both are widened to whole seconds, because `r` carries whole
seconds. Search then opens the pinned absolute range and runs it again. When
that run is accepted, the dialog sends the pinned search, so the rows you see
are the rows the capture is asked for.

The dialog reads the server's capabilities when it opens and offers nothing
until they answer. A pinned range counts toward the limit as it would be sent:
`[from, to + 1 µs)`. If the capabilities cannot be read, the dialog refuses
nothing for length, and trawld refuses a capture that is too long.

The capture runs the query again on the server. An event that arrived inside
the window after your search ran is in the capture too.

A capture that trawld accepted raises a toast, such as **Step 7 in Disk
pressure on node-2**, with an **Open the board** link. If the investigation was closed in the meantime, the dialog says
so, keeps what you entered, and lists the open investigations again. If the
answer is lost, the dialog says the outcome is unknown, replaces **Send** with
**Open the board**, and does not send again by itself. Open the board to see
whether the step arrived.

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

## Investigations

An investigation is a case file that an agent or a person keeps on the server:
the question, every search and correlation read run for it, the rows kept as
evidence and why, and the findings. The
[API reference](/reference/api/#investigations) describes the object, and
[Work an investigation from the browser](/use/investigations/) walks through
the board.

Both pages need `query` and `investigation_read`. Without them, the page says
`You do not have permission to read investigations.` and asks the server
nothing. A key that also holds `investigation_write` sees the controls that
write. `server_manage` grants neither.

| Control | Needs |
|---------|-------|
| Sidebar entry, list, board, evidence viewer, **Open in Search** | `query` and `investigation_read` |
| **Keep**, **Close**, **Reopen** | `query`, `investigation_read`, and `investigation_write` |
| **Send to investigation** on Search | `query`, `investigation_read`, and `investigation_write` |

A control a key may not use is absent, not disabled.

### The list

`/investigations` lists every investigation in the installation with
**Title**, **Status**, **Updated**, and **By**. Open investigations come
first, then closed ones, each group most recently changed first. Selecting a
row opens the board. **Refresh** reads the list again, and the list does not
read again by itself.

### The board

`/investigations/{id}` reads top to bottom:

1. The header: the title, the status, **Close** or **Reopen**, who opened the
   investigation, when and by whom it was last updated, the connection state,
   and when the board last checked.
2. **Question** and **Scope**.
3. **Latest finding**.
4. **Activity**: every entry, one line each, in the order it was written. Steps,
   keeps, findings, finding status changes, notes, and status, title, and
   question changes all appear here.
5. **Findings**: each finding with its status, who set it, its statement, the
   question that is **Still open**, and the **Evidence** and
   **Counterevidence** rows it cites.
6. **Steps**: each step is a disclosure over the text it executed, its
   **Bounds**, and its outcome. A step that captured a result has
   **View rows**.

A step that finishes, or a finding whose status changes, updates where it
stands, so an open step stays open. The board reads the log in pages. A read
you started that leaves entries unread is followed at once, up to 20 reads.
Until the log is whole, **Latest finding** and **Findings** say the page is
still reading. When the reads stop short, they name the entry they are current
to, and **Load more entries** reads on. An id that names no investigation shows
**Investigation not found**.

### Live updates

While the tab is visible, the board asks for the entries changed since its
last read every 5 seconds. While the tab is hidden, it reads nothing, not even
the read that follows a close, a reopen, a keep, or **Refresh**: that read
waits for the tab. The evidence waits too: a cited row, a row **Refresh** asks
for again, and a page of the evidence viewer, including one whose 429
`Retry-After` passed while the tab was hidden. A board opened in a hidden tab
shows **Paused** and loads when the tab shows. When the tab shows again, it
reads at once and then every 5 seconds. A read that waited for the tab also
waits out a 429's `Retry-After` that is still running, however often the tab
hides and shows, and a 403 or a 404 drops it. **Refresh** in a visible tab does
not wait for a `Retry-After`.

The header's badge names the connection state:

| State | Meaning |
|-------|---------|
| **Checking** | The first read has not answered yet |
| **Live** | The last read was accepted |
| **Paused** | The tab is hidden, so the board reads nothing |
| **Retrying** | The last read failed. The next read waits 5 seconds after the first failure, then 10, 20, 40, and at most 60. A 429 waits at least its `Retry-After` |
| **Stopped** | The server refused a read with 403, or no longer holds the investigation. The board reads nothing more on this visit and drops its write controls |

**checked** gives the time of the last read the board accepted, including a
read that brought nothing new. A failed read changes neither the board nor that
time, so the board never goes blank. A 401 opens sign-in.

A change you did not ask for, such as a new entry or a cited row that loads,
leaves the content you are reading at the same place on screen, however much
grew above it. The board scrolls by the distance the change moved that
content. A polite live region tells screen readers that a check failed, that
the board stopped or came back, and how many new entries a read brought. It
does not announce each check.

### Evidence and the viewer

A finding's cited rows load in place, through the captured-result page route,
once per row per page visit. The reads are paced to stay inside your key's rate
limit. When the server keeps refusing them, **Findings** says how many rows
wait, and **Resume reading** reads them again. A row whose read failed has
**Retry**, and **Refresh** asks again for every failed row.

**View rows** on a step, and **Open result** on a cited row, open the evidence
viewer. It is a drawer over the board, so opening or paging it never moves the
board. From a cited row, it opens at the page that holds the row and marks the
row.

The viewer reads the result's stored rows and nothing else. It never runs the
query again, so a result whose events have left corpus retention still shows
every row it captured. The server's page size decides a page. Cells read as
they do in the Search results table.

The rows come in the order the capture stored them. A capture of raw events,
with no pipeline, is sorted newest first (`| sort -_time, -_ingested`), and
Search shows a snapshot in stored order. A sent search's viewer can therefore
list the same events as Search in another order.

| The result | The viewer shows |
|------------|------------------|
| Readable | The rows, with **Row** numbers and a pager |
| Captured no rows | `This result captured no rows.` |
| Pruned (410 `result_gone`) | **Pruned** and the server's reason. The finding still cites the row |
| File missing (409) | `These rows are unavailable.` with **Check again** |
| Rate-limited (429) | The page it shows, and a line that it reads again shortly |
| Any other failure | The error with **Retry** |

None of these states reads as an empty result.

**Open in Search** sits below the rows. It opens Search with the result's
executed query over its bounds, through the same link builder the MCP server
uses. See [the search URL](/reference/mcp/#the-search-url). Search runs that
query again over the current corpus, so its rows can differ from the captured
rows. A result without bounds has no **Open in Search**.

### Keep a row

With `investigation_write`, the viewer has a **Keep** column. **Keep row N**
opens a form that asks **Why keep row N?**. The reason is required, at most
1024 bytes of UTF-8 once trimmed, and holds no NUL character. The form says
what is wrong beside the field before it sends anything. Enter or
**Keep row N** sends it, once however often you press it. **Cancel** closes the
form. Each keep records one row, under your key.

The **Keep** column is absent while the investigation is closed and on a result
that does not read, such as a pruned one.

| Outcome | The viewer says |
|---------|-----------------|
| Kept | `Kept row N. The board shows it in the activity.` The board then reads what changed |
| Closed in the meantime | Nothing was kept, because the investigation was closed |
| Answer lost | The outcome is unknown. The reason stays, and **Refresh the board** reads the board. **Keep** stays held until a read that started after the keep lands |
| Refused | `Could not keep row N.` and the server's message |

The page never sends a keep again by itself. An agent attached to the
investigation sees the keep in its next attached result. See
[activity](/reference/mcp/#activity).

### Close and reopen

With `investigation_write`, **Close** or **Reopen** sits beside the status. Any
writer can close or reopen any investigation. There is no confirmation. The
press sends one overwrite with the sequence the board holds at that moment.

A 409 is a conflict, whether it is `sequence_conflict` or
`investigation_closed`. The board shows a notice that nothing changed, reads
the investigation again at once, or when a hidden tab shows, and keeps your
place. Until that read lands, the notice says the board is reading the
investigation again, or that it reads it when the tab is shown. Then the notice
says the board has read it again, and the control matches the status that
stands, so press it again if you still mean to.

A success stays acknowledged even when the read after it fails. When the answer
is lost, the notice says the change may or may not have taken effect, and
**Refresh the board** reads it again. The control stays disabled until a read
that started after the press lands. The page never sends a close or a reopen
again by itself.

## Nets

`/jobs/nets` lists saved queries with **Name**, **Schedule**, **Last run**, and
**Actions**. **Name** and **Last run** have sorting controls. A row's drawer offers
**Rename**, **Edit** for the query text, **Open query in search**,
**Run now**, and **Runs**. **Delete** asks for confirmation.

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

**Run now** appears in the net's row and in its drawer header whenever the
saved net has a schedule, in any mode, including a paused schedule. A net with
no schedule does not show it. The button is disabled while its request is in
flight. A started run raises a toast with the window the server claimed, in
UTC, and a **View run** link to the run. Query text mode shows **Run started**
with no window. A refused run raises a toast with the server's message, for
example when a run is already in progress or there is nothing new to read.
Below the window choice, the form says what **Run now** reads in each windowed
mode.

Run history, in the drawer's **Runs** tab and on the **Runs** page, marks a
manual run **Manual**.

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
| Server health | any signed-in key | Overall state, version, and each named check. A `duckdb` check that reads `slow` shows as a warning, not a failure |
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
