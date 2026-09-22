# The chart draws the whole result or nothing: an aggregation snapshot is fetched whole, paged in the browser, and the response names its total

status: accepted (2026-09-20) — prep ruling record for the Visualization
pagination defect. Amended 2026-09-21 by ADR-0038: the chart's x axis is
now the bucket instant, and grouped results draw. The coverage rule and
its place last in the ladder are unchanged.

`api::query` sends `limit: PAGE_SIZE` with `offset: page * PAGE_SIZE` for
every snapshot query (`crates/trawl-web-ui/src/api/mod.rs:408-417`), and
the Visualization arm hands the response straight to `<Chart/>`
(`crates/trawl-web-ui/src/pages/search.rs:1040`). The request path never
asks what shape the query is; `is_aggregation_shape` is read only in the
view layer (`:560`). So `level=error | timechart span=2m count()` over
four hours, which produces 59 buckets, draws 50 of them. The chart's x
axis is result position, not time (`components/chart.rs:14-17`), so the
missing buckets are not a gap in the line — they are absent from it, and
the picture ends early with nothing to show that it did. A browser check
against a real daemon on 2026-09-20 read the spike as ending at 02:10
when the data ran to 02:28. Turning to page 2 draws the remaining nine
buckets on the same axis, starting again at position 0; the tab has no
pager of its own, so that page is reachable only through the Events tab.

The same result feeds the categorical chart beside the exact table, and
its scale is computed from whatever rows it is handed
(`crates/trawl-web-ui/src/categorical.rs:100-121`, `:56-65`). Measured
on the same instance: for `stats count() by message`, a group whose
count is 1 draws at `width:100.00%` on page 1 and `width:6.67%` on page
2. One value, two pictures.

Nothing on the page contradicts this today, because nothing on the page
claims otherwise. `context.md` has no entry for **Visualization** at all;
it appears only in the `_Avoid_` lines of **Histogram** and **Categorical
chart**. ADR-0030's inventory of paginated surfaces names four — history,
runs, the net drawer, the results table — and not this one.

The existing `Truncated` badge cannot carry the signal. `truncated` is
`total >= max_result_rows` (`crates/trawl-server/src/handlers.rs:232`),
but the executor refuses with `ResultTooLarge` once `rows.len() >=
max_rows` while collecting (`crates/trawl-engine/src/executor.rs:525-527`),
so a result can hold `max_rows` rows and never more. Against a daemon
with `max_result_rows = 100`: 99 rows answered `truncated: false`, 100
answered `truncated: true`, 101 answered HTTP 400 `result_too_large`.
The flag is true on one row count in the whole domain, and on that count
the result is complete. It answers a question this endpoint does not have.

## Decision

**The request window is decided by the query's shape, and the chart draws
a whole result or refuses.** One pure `FetchPlan::for_query(query, page)`
returns `Page { page }` for a raw-event query — today's `limit = 50`,
`offset = page * 50`, unchanged — and `Whole` for any query
`is_aggregation_shape` admits: `limit = AGGREGATE_FETCH_ROWS`, `offset =
0`, once per effective query. A `page` change under `Whole` posts no
query; it slices the result already in hand. `is_aggregation_shape` runs
the same `trawl_core` parser the server runs
(`crates/trawl-web-ui/src/facets.rs:59`), so the plan cannot disagree
with the server about what shape a query is.

**`AGGREGATE_FETCH_ROWS` is 20,000, and it is a request, not a mirror.**
The automatic bucket interval tops out at 168 buckets for the largest
quick range (`crates/trawl-core/src/emitter/pipeline.rs:439-456`), so the
ceiling only ever bites a span the operator typed. It clears the largest
one the shipped controls can ask for: seven days at `span=1m` is 10,080
buckets. The server clamps `limit` to its own `max_result_rows`
(`handlers.rs:73`) and, with `offset = 0`, the `offset + limit` refusal
at `:76-80` is unreachable. Past the server's ceiling the query fails as
it does today — `result_too_large`, an error, not a short result.

**`PaginationMeta` gains `total`: the rows this successful execution
produced before the response window was cut from them.** The server
already computes it and discards it (`handlers.rs:231`). It is a required
field; there is no `serde(default)` and no shim.

**`QueryResponse.truncated` is removed.** This endpoint has two outcomes,
a whole result or an error, and the flag names a third that does not
exist. The `Truncated` badge (`pages/search.rs:891`) and the
`" (truncated)"` pager suffix (`components/results_table.rs:548`,
`components/exact_table.rs:201`) go with it.

**Coverage is read from the response, and the chart is where it is
enforced.** A chart draws only when `offset == 0 && returned == total`.
The rung sits after the existing shape ladder in `chart_hint`, so a
result the ladder refuses for its shape keeps the refusal that tells the
operator something actionable. *Amended 2026-09-21 (ADR-0038): a grouped
result is no longer a shape the ladder refuses; the rung still comes
last.* Above the ceiling the chart does not draw and says
so with the number the server measured; a complete 20,000-row result
does draw.

**The categorical chart keeps the exact table's local slice, and takes
its scale from the whole fetched result.** Its bars are `aria-hidden`
because every number in them is already in the table beside it, which is
the accessible representation (`components/cat_chart.rs:15-17`). Drawing
100 bars beside 50 rows would give a sighted reader comparisons the
accessible page does not carry, so the pairing stays. Computing the
scale over the fetched result rather than the slice is what fixes the
rescaling defect, and it needs no coverage gate: under `Whole` the
fetched result is every group.

**The exact table pages the fetched rows with `PageTotal::Known`**, the
stored-run preview's rule (ADR-0030, amended 2026-09-12), and prints the
cap line in that preview's words when the result exceeds what was
fetched.

## Consequences

- The live lane does not change. It sends no window
  (`state/stream_session.rs:101-112`), the server emits every retained
  bucket (`handlers.rs:3242-3255`), and `snapshot_timechart` sorts and
  emits them all. Live is already whole by construction; the chart reads
  no coverage there.
- The raw-event **Histogram** is untouched. It buckets its own page's
  rows and says so in the glossary. Covering the whole search window is
  #197, and it needs a companion aggregation this change does not.
- The raw results table keeps `PageTotal::Probe`. `total` now makes
  `Known` possible there, which would change every pagination summary
  the specs pin; that is a visible change of its own and is not this
  one.
- A required `total` moves every literal that builds a `PaginationMeta`:
  CLI and TUI, three web-ui sites, the e2e harness, and every wire
  fixture and spec mock. A mock without it fails to decode, which is how
  the specs are made to go red first.
- **20,000 rows is a row bound, not a byte bound.** Measured per-row wire
  cost on real data was 21, 26 and 48 bytes across three aggregation
  shapes, so an ordinary chart is well under a megabyte. A grouped shape
  with long group keys is not bounded by that number at all. Today's
  50-row page hides this; the honest remedy is a byte ceiling on the
  response, server-side, and it is not in scope here.
- Fetching whole removes a request per page turn on aggregations, so the
  change spends fewer requests against the interactive rate limit than
  the behaviour it replaces, not more.
