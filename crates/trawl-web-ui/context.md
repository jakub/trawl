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
The well at the foot of the console reporting what the last query actually ran with: the effective window, the active filter chips, the mode badge and the row count. It describes the executed query only, so typing never changes it, and a malformed link leaves it empty behind the banner.
_Avoid_: meta strip, summary bar, filter bar

**Structured state**:
The URL parameters that are not the query text: page, mode, filters and range. They are folded into the DSL at request time; the query text is never rewritten to carry them.
_Avoid_: URL state (that includes `q`), extras

**Executed query**:
The DSL text the URL carries in `q` and the app last ran or will run. Distinct from the editor buffer, which changes on every keystroke and never touches the URL until submit.
_Avoid_: current query, query text (that is the buffer)

**Effective query**:
The DSL string that actually goes to the server: the executed query with the structured state merged in.
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
Snapshot (a paginated one-shot query) or live (an SSE stream). The URL carries `mode=live` or nothing. Live Tail enters live; Stop live or any range selection leaves it, always by navigation. Nothing pauses it: a URL that says live streams, or shows why it cannot. While live, no snapshot query runs.
_Avoid_: view, tab, pause

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
The bar-per-group companion beside the exact table, drawn for the one shape it can state exactly — one group column and one numeric metric — with the value printed on every bar. Any other aggregate shape shows the table alone.
_Avoid_: bar chart (unqualified), visualization (that is the other tab)

**Count**:
The Events tab shows rows on screen. The footer names its source: Last (rows the last snapshot returned), Received (events delivered since the stream opened), Updates (aggregation frames since the stream opened).
_Avoid_: total, matches, hits

**Navigator**:
The one closure that turns `(query, page, mode, filters, range)` into a search URL and pushes or replaces it on the router's history.
_Avoid_: router (that is leptos's), goto

### Nets

**Net**:
A saved query as the browser names it, with its optional schedule and its recorded runs.
_Avoid_: saved search, report (that is a run's output)

**Run**:
One stored execution of a net's schedule: the resolved query text, the window it covered and the result rows the server kept. The drawer pages a fetched result locally; a run is never re-fetched by page.
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
