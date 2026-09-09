# trawl-web-ui

Vocabulary for the browser SPA. Cross-cutting terms (event, field, pin, lane) live in the root `context.md`; this slice names only what the SPA owns. Decisions live in `docs/adr/`.

## Language

### The search page

**Search URL**:
The address-bar form of the search page's whole state: `q`, `page`, `mode`, `f` and `r`. One producer writes it, one reader parses it, and bookmarks, Back/Forward and "copy link" are all that one round trip.
_Avoid_: deep link, permalink, share URL

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
Snapshot (a paginated one-shot query) or live (an SSE stream). The URL carries `mode=live` or nothing.
_Avoid_: view, tab

**Navigator**:
The one closure that turns `(query, page, mode, filters, range)` into a search URL and pushes or replaces it on the router's history.
_Avoid_: router (that is leptos's), goto

### List tables

**Row control**:
The one link or button in a list row's primary cell. Its hit area is stretched over the whole row, so the pointer opens the row from anywhere while the keyboard reaches exactly one element; nested controls sit above it. The row element itself carries no handler.
_Avoid_: clickable row, row handler, stretched link (that is the CSS technique, not the term)

### Settings surfaces

**Health page**:
The Settings destination at `/settings/health`: an operational overview over the server's health, stats and dashboard routes plus the running-queries list. Its sections gate individually — health for every signed-in user, admin telemetry only for a key holding server-manage, cancel controls only where the caller's permissions could succeed — and its live data is the shell's one dashboard stream, never a second connection.
_Avoid_: status page, dashboard (that is the server's snapshot type), health check (that is the endpoint)
