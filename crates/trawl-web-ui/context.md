# trawl-web-ui

Vocabulary for the browser SPA. Cross-cutting terms (event, field, pin, lane) live in the root `context.md`; this slice names only what the SPA owns. Decisions live in `docs/adr/`.

## Language

### The search page

**Search URL**:
The address-bar form of the search page's whole state: `q`, `page`, `mode`, `f` and `r`. One producer writes it, one reader parses it, and bookmarks, Back/Forward and "copy link" are all that one round trip.
_Avoid_: deep link, permalink, share URL

**Query console**:
The raised region at the top of the search sheet that holds everything the next query is made of: the label and draft state, the editor well, the range trigger, Haul, the tool row, and the executed-scope strip that closes it. The one lifted surface on the page.
_Avoid_: editor (that is the CodeMirror mount inside it), search bar, toolbar

**Draft state**:
The console header shows "Edited" when the editor buffer differs from the executed query. It shows no draft message when they agree. Derived text, never a control, and it never navigates.
_Avoid_: dirty flag, unsaved indicator, status

**Executed-scope strip**:
The well attached to the foot of the console, holding removable active filter chips and facts about the active result. A completed snapshot shows rows returned, server execution duration and its absolute UTC start time, without a mode badge. Live shows a Live badge and buffered rows without snapshot timing. Typing does not change completed facts; a pending, failed or malformed search cannot display another query's execution facts.
_Avoid_: meta strip, summary bar, filter bar

**Structured state**:
The URL parameters that are not the query text: page, mode, filters and range. A snapshot folds them into the DSL at request time; a live stream folds the filters and never the range (see Mode). The query text is never rewritten to carry them.
_Avoid_: URL state (that includes `q`), extras

**Executed query**:
The DSL text the URL carries in `q` and the app last ran or will run. Distinct from the editor buffer, which changes on every keystroke and never touches the URL until submit.
_Avoid_: current query, query text (that is the buffer)

**Effective query**:
The DSL the current mode runs: the executed query with the structured state merged in as the mode folds it. Export always uses the snapshot form, with the range, because a download is bounded.
_Avoid_: wire query, final query

**Filter**:
One include or exclude clause on a field value, driven by the facet sidebar or a detail-row tag. Filters combine with AND. Carried in `f` as an opaque, versioned payload.
_Avoid_: facet (the sidebar control), chip (its rendering), pill

**Range**:
The time window the date-range popover selects: a quick range (a label from a closed set, `1h`) or an absolute range (two UTC instants, the right one may be `now`). Carried in `r` readably.
_Avoid_: time filter, period, window (unqualified)

**Malformed link**:
A search URL whose structured state does not parse or makes a false claim. It is shown with a banner and never run; a repair rewrites the URL only when clicked.
_Avoid_: invalid URL, broken link (too broad), stale link

**Mode**:
Snapshot (a one-shot query, paged by the server for events and in the browser for an aggregation) or live (an SSE stream). The URL carries `mode=live` or nothing. Live Tail enters live; Stop live or any range selection leaves it, always by navigation. Nothing pauses it: a URL that says live streams, or shows why it cannot. While live, no snapshot query runs. A live stream carries the query text and the filter chips, never the range. The range in `r` is what Stop live returns to.
_Avoid_: view, tab, pause

**Query error**:
A snapshot or live submission the server refused as a parse or validation failure. It renders in the results region on both results tabs as a notice quoting the server's message and, for each detail with a span, an excerpt of the query as sent with a caret under the span. It offers no Retry. Any other failure is a load error and keeps its Retry.
_Avoid_: 400, bad request, syntax banner

**Draft diagnostic**:
What the local parser says about the editor buffer while you type: the gutter marker and the visible line under the editor that carries the first message in full, with the rest behind a disclosure. Advisory only: it never blocks Haul and never speaks for the server.
_Avoid_: lint, squiggle (that is one rendering of it), validation

**Live ring**:
The newest raw events the stream delivered, up to a fixed size. In live it is what the Events tab, its count and the filter rail show; the footer's Received count is everything delivered since the stream opened, including what rolled off.
_Avoid_: buffer (unqualified), tail rows, history

**Filter rail**:
The sidebar counting field values over the rows on screen: the snapshot page or the live ring. Its counts describe what is shown, never the corpus, and it is absent for an aggregation-shaped result because a value filter on an aggregate column names a field no event carries.
_Avoid_: facet sidebar (the code name), facets

**Histogram**:
The strip above the snapshot table: the page's rows bucketed over their own time span. Absent in live. Neither ingest volume nor the full distribution of matches.
_Avoid_: timeline, chart (that is Visualization)

**Reading mode**:
One of the two optional presentations of snapshot results, both off by default and persisted in `UiPrefs`: `details` chooses between the inline expanded row and the docked inspector, `rows` between the compact table and message-first rows. Chosen through the "View" disclosure in the result header. Live results and aggregation shapes ignore both.
_Avoid_: view mode, layout, density

**Docked inspector**:
The `details = inspector` presentation: a panel beside the results table (below it under 900px) showing every field of the selected event with Include, Exclude and Copy per row. Selecting a row highlights it and leaves the table in place; the selection names an event, not a row position, so sorting does not move it.
_Avoid_: detail drawer, side panel, preview

**Message-first rows**:
The `rows = message-first` presentation: time, severity and the message at full width, with service, host and latency as a muted second line. Every other column moves to the inspector or the inline detail.
_Avoid_: compact mode (that is the default), log view

**Exact table**:
What the Events tab renders for an aggregation-shaped result: the group and metric columns with no expansion column and no Include control on a generated metric, because a filter on a computed column names a field no event carries. Only the group-by columns offer "search this group".
_Avoid_: aggregate table, stats table, summary

**Categorical chart**:
The bar-per-group companion beside the exact table on the Events tab, drawn for the one shape it can state exactly — one group column and one numeric metric — with the value printed on every bar. It draws the bars the table beside it shows, and scales them against the whole fetched result, so a value's bar is the same length on every page. Any other aggregate shape shows the table alone.
_Avoid_: bar chart (unqualified), visualization (that is the other tab), Column, Bar (those are chart types)

**Visualization**:
The results tab that draws a whole aggregation result, snapshot or live, as the selected chart type. It draws the whole result or nothing: an aggregation snapshot is fetched whole, up to 20,000 rows, and a response that is not the whole result is refused with the count the server measured, never drawn in part. It has no pager, because the chart of one page is a different picture from the chart of the result. Every refusal names the fix or opens Events.
_Avoid_: chart (unqualified; the categorical chart is the other one), graph, plot

**Chart type**:
One of Line, Column, or Bar, picked by a control on the Visualization tab and kept for the session, never in the search URL. Line draws a `timechart` on a UTC time axis; Column and Bar draw a `stats … by` as one bar per group, upright or rotated. The default follows the result's shape, and a type that does not fit the result is shown disabled.
_Avoid_: chart mode, view, renderer

**Series**:
One line on a Line chart: a metric column of an ungrouped `timechart`, or one group value of a grouped one, labelled by its group values joined with ` · `. A bucket with no row for a series is a gap, never a zero. The chart draws at most six, the largest by total, and a caption under it names how many it left out.
_Avoid_: line (that is the chart type), trace, group (a group value names a series; the series is the line)

**Count**:
The Events tab shows rows on screen. For an aggregation that is the rows fetched, and the pager under the exact table names the slice on screen. The footer names its source: Last (rows the last snapshot returned), Received (events delivered since the stream opened), Updates (aggregation frames since the stream opened).
_Avoid_: total, matches, hits

**Quick start**:
The guidance both results tabs show before a snapshot query runs: four executable examples, sender and reserved field references, and severity bands. Examples use the selected range and filters, then open Events. A submitted query with no matches shows its no-results state instead.

**Navigator**:
The one closure that turns `(query, page, mode, filters, range)` into a search URL and pushes or replaces it on the router's history.
_Avoid_: router (that is leptos's), goto

### Nets

**Net**:
The browser's surface for a net (defined in the root `context.md`): the nets page, the net drawer and the save-as-net modal.
_Avoid_: saved search, report (that is a run's output)

**Run**:
The drawer's view of one run (defined in the root `context.md`). The drawer pages a fetched result locally; a run is never re-fetched by page.
_Avoid_: report, execution, job

**Run receipt**:
The facts panel beside a stored run's result: net, outcome, duration, rows recorded and the query as it ran. It reports what was recorded, so re-running from it produces a new result and never replaces the one on screen.
_Avoid_: summary, metadata, details

### List tables

**Row control**:
The one link or button in a list row's primary cell. Its hit area is stretched over the whole row, so the pointer opens the row from anywhere while the keyboard reaches exactly one element; nested controls sit above it. The row element itself carries no handler.
_Avoid_: clickable row, row handler, stretched link (that is the CSS technique, not the term)

### Settings surfaces

**Health page**:
The Settings destination at `/settings/health`: an operational overview over the server's health, stats and dashboard routes plus the running-queries list. Its sections gate individually — health for every signed-in user, admin telemetry only for a key holding server-manage, cancel controls only where the caller's permissions could succeed — and its live data is the shell's one dashboard stream, never a second connection.
_Avoid_: status page, dashboard (that is the server's snapshot type), health check (that is the endpoint)
