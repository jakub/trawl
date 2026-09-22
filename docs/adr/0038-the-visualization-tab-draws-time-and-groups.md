# The Visualization tab draws on a time axis, aligns grouped series on it, and offers three chart types

status: accepted (2026-09-21) — prep ruling record for #234

The Visualization chart draws x as the row's position in the result, not
the instant in its `_time` cell (`crates/trawl-web-ui/src/components/chart.rs:16-19`
at `c7cf9015`). Two lies follow. A bucket with no events has no row, so
the line closes over the quiet minute as if nothing happened. Two groups
with different bucket counts cannot share the axis, because row 5 is a
different minute for each, so the chart refuses every grouped result.
The refusal is documented as intended in `crates/trawl-web-ui/context.md`,
in the comment at the top of `components/cat_chart.rs`, and in
ADR-0037's Decision. The row-index axis itself was a recorded deferral:
commit `2dcf6a19` gave the service drawer's ingest chart a real time axis
and left this one as it was, with a comment saying so.

The prep for #234 ran a cross-family dialectic and a grill. The human
widened the scope from "axis and grouped lines" to a small chart-type
picker, and ruled that the chart draws the largest series and says what
it left out rather than refusing a wide result.

## Decision

**The Visualization tab draws a whole aggregation result as one of three
chart types: Line, Column, or Bar.** A control on the tab picks the type.
The choice is session state, not part of the search URL (ADR-0027). The
default follows the result's shape: Line for a `timechart`, Column for a
`stats … by`. A type that does not fit the result is shown disabled with
the reason. ADR-0037's coverage rule is unchanged and stays the last rung
of the refusal ladder: a response that is not the whole result is refused
with the count the server measured, whatever the type.

**Line draws x as the bucket instant parsed from `_time`, in UTC.** The
web UI sends `timezone: None` and the server defaults the offset to zero
(`crates/trawl-web-ui/src/api/mod.rs:422`,
`crates/trawl-server/src/handlers.rs:116-121`), so a snapshot `_time` is
UTC wall clock without a suffix and a live `_time` is RFC 3339 with a zero offset.
The chart parses exactly those two forms, strictly, and refuses any other
text in `_time`. The axis is labelled `UTC` and its ticks print the digits
the Events table prints. uPlot gets `utc: true` so the browser does not
shift them. Browser-local time is a separate product question. The live
stream spells the zero offset `+00:00` (chrono's `to_rfc3339`), so the
strict parser admits both `Z` and `+00:00`.

**Series come from the query, not from cell types.** The ladder reads the
`by` fields and the metric columns from the parsed query that produced
the response, never from the query being typed. An ungrouped `timechart`
draws one series per metric column. A grouped `timechart` draws one
series per distinct group value across any number of `by` fields, with
one metric, labelled by the group values joined with ` · ` in `by` order.
A grouped `timechart` with two metrics is refused with a sentence that
names the fix. Any number draws: integer, unsigned, float, negative. A
null cell is a gap. `pivot`, `top`, `rare`, a duplicate (bucket, series)
row, and a `_time` cell that does not parse are refused.

**Bucket width comes from one resolver in `trawl-core`.** The automatic
span is a function of the query's own `last=` filter
(`crates/trawl-core/src/emitter/pipeline.rs:466`), and the live compiler's
default (`crates/trawl-core/src/stream.rs:1456`) equals that function's
answer for no filter. The resolver becomes public and the emitter, the
live compiler, and the chart all call it. The chart never infers a
cadence from the data and the response carries no new field. With the
span known, the chart lays a grid from the earliest to the latest
returned bucket, at most 20,000 instants, and a (bucket, series) with no
row is a null drawn as a gap, never a zero. Above 20,000 instants the
chart refuses and says so. A snapshot timechart that runs as a rust
stage after `extract kv` is bucketed by the same resolver with the same
`last=` filter, so its buckets match the SQL lane's and the chart's; only
the live stream resolves with no filter, because a stream has no past
window.

**More than six series draws the six largest by total over the fetched
rows, with a caption under the chart that names what was left out.** The
caption is part of the contract: a legend of six over a result of
fourteen is a picture that lies by omission without it. Column and Bar
draw the twenty largest groups by value under the same rule. The number
in the caption is measured over the fetched rows, so a cut result never
claims a total it does not have.

**Column and Bar draw the shape the Events tab's categorical chart
already admits**: one `by` field, one metric, `stats` as the last stage,
numbers including floats and negatives, nulls tolerated
(`crates/trawl-web-ui/src/categorical.rs:76-131`). A trailing `sort` or
`head` after that `stats` is not admitted today, which the detector's
own comment (`last_stats`) already explains. Both types use uPlot with
an ordinal x scale and the group labels passed through the bridge. Bar
is Column rotated. The Events tab and its inline bars do not change.

**Live feeds the same component.** The live snapshot goes through the
same ladder, the same resolver, and the same detector. A grouped live
`timechart` draws; a live `stats … by` draws columns that re-rank by
value each frame; a seventh group becomes a caption, not a refusal.

**`vendor/src/uplot.ts` loses `rowIndex`**, gains the ordinal category
scale, and sets `spanGaps: false` explicitly. The bundle is rebuilt and
the CI `vendor-drift` job proves it. The six colours and dash patterns
stay a bridge-side constant, cross-referenced by comment with the Rust
cap.

## Considered and rejected

- **Publish the resolved span in the response envelope.** Correct, but it
  touches every literal that builds `QueryResponse` and both lanes for a
  number the client can already compute from the same crate.
- **Infer the span as the smallest gap between adjacent buckets.** Sparse
  `_severity>=warn` data at 12:00, 12:02 and 12:04 with a real one-minute
  span infers two minutes and hides the missing minutes. An invented
  cadence in a chart whose rule is no invented numbers.
- **Refuse above six series.** The human ruled that a truthful caption
  beats an empty tab.
- **Typed group identity on the live stream**, so that numeric `200` and
  string `"200"` stay two groups. Live keys groups by text today
  (`crates/trawl-core/src/row.rs:69-87`) and no reachable homelab query
  was shown that needs the distinction. The chart draws the groups the
  server returns.
- **Move the coverage rung ahead of the shape rungs.** Both orders only
  refuse. A shape refusal names a fix in the query and is the more useful
  first sentence.
- **Retire the Events tab's inline bars.** A visible removal on a tab this
  change did not set out to touch.

## Consequences

- `extract_series` in `crates/trawl-api/src/display.rs` leaves the web
  path. It stays for the CLI and TUI.
- Today's silent top-six for a seven-metric ungrouped `timechart` becomes
  a drawn six with a caption.
- A cut result, on a real axis, would draw as a line that ends early with
  nothing to show that it did. The coverage rung is what keeps that
  picture off the screen, and it stays.
- Pie and Area are not in this decision. Adding a type is an amendment
  here, not a new ADR.
