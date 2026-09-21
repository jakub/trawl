# The Visualization chart and the whole result

Issue #228. The Visualization tab drew the browser's current 50-row page and
presented it as the result, and the bars beside an aggregate table were scaled
against whichever rows that page happened to hold. This directory holds one
capture of that behaviour and one of the behaviour after the fix, both produced
by [`capture.mjs`](capture.mjs) in this directory.

## The two revisions

| Label | Commit | What it is |
| --- | --- | --- |
| `before` | `9ed451ccdcbd28eb98fd2278015c66f014f6eee7` | the branch's base, `origin/main` at capture time |
| `after` | `c1e82824bbb9ae682142534788a17dd3b061a027` | the branch head |

`c1e82824` differs from `4dddb311`, the last product commit of the fix, only in
`scripts/app-experiment/run.mjs`: the experiment runner's live-tail selector,
which had to follow the button into the range dialog's footer before the runner
could hold an instance at all. No crate changed between the two.

Both captures ran with `capture.mjs` at SHA-256
`f97584ae8b5be2973b937981578edef62abeb8be27c3e7feca1c8088cff3a41c`, which each
transcript repeats as `captureSha256`. A transcript whose hash does not match
the file beside it came from a different script.

## What was measured, and how

Each instance came from `bin/app-experiment --hold-seconds`, which owns a
disposable PostgreSQL container, a real `trawld`, the session proxy and a real
Chromium. `capture.mjs` then seeds its own corpus through the ingest API and
drives the SPA through its own controls.

The corpus occupies the four-hour window `2026-01-01T02:00:00Z` to
`2026-01-01T06:00:00Z`, which the experiment runner's own workload (fixed at
`2026-01-01T12:00:00Z`) does not touch. Inside it:

- 230 `level=error` events placed in 59 distinct `span=2m` buckets, so
  `timechart` returns 59 rows — more than one 50-row page.
- 74 `level=info` events over 60 distinct `message` values: `group-00` carries
  15 events and every other group carries exactly one.

That shape is deliberate. `stats` emits no `ORDER BY`, so which 50 groups land
on the table's first page is the engine's business. With one peak and 59
singletons, whichever page does not hold the peak holds nothing but count-1
groups however it was chosen, and a bar scaled against its own page draws every
one of them full width. The measurement does not depend on a group staying put.

Three things are recorded per page, and only the third is revision-dependent:

1. every `/api/v1/query` request body the SPA posted, with its `limit`,
   `offset` and the `pagination` object that came back;
2. the categorical bar widths, read from each bar's own inline `style`;
3. the plotted point count. The `after` revision publishes it on the chart host
   as `data-points`. The `before` revision has no such attribute and a canvas
   cannot be read back, so that number comes from the response's
   `pagination.returned` — the rows the browser had to draw from. Each
   transcript names its source in `chart.source`.

The queries are the issue's own, unchanged:

```
level=error | timechart span=2m count()
level=info | stats count() by message
```

## What was observed

### `timechart`: the chart drew the page, not the result

| | `before` (9ed451cc) | `after` (c1e82824) |
| --- | --- | --- |
| requests posted, both pages | 2 | 1 |
| page 1 request | `limit: 50, offset: 0` | `limit: 20000, offset: 0` |
| page 1 response | `returned: 50`, no total on the wire | `returned: 59, total: 59` |
| page 2 request | `limit: 50, offset: 50` | none posted |
| page 2 response | `returned: 9` | — |
| points plotted, page 1 | 50 | 59 |
| points plotted, page 2 | 9 | 59 |
| table footer, page 1 | `Page 1 · showing 50 rows` | `1–50 of 59` |
| table footer, page 2 | `Page 2 · showing 9 rows` | `51–59 of 59` |

Page 2 of the `before` capture is the defect in one image: nine points on an
axis that starts again at position 0, under a query whose answer is 59 buckets.

| `before`, page 2 | `after`, page 2 |
| --- | --- |
| ![before](results/before/timechart-visualization-page2.png) | ![after](results/after/timechart-visualization-page2.png) |

Page 1 of each: [before](results/before/timechart-visualization-page1.png) ·
[after](results/after/timechart-visualization-page1.png).

### `stats count() by message`: the bars were scaled against the page

| | `before` (9ed451cc) | `after` (c1e82824) |
| --- | --- | --- |
| requests posted, both pages | 2 | 1 |
| page 1 request | `limit: 50, offset: 0` | `limit: 20000, offset: 0` |
| page 2 request | `limit: 50, offset: 50` | none posted |
| `group-00` (count 15), page 1 | `width:100.00%` | `width:100.00%` |
| the 49 count-1 bars on page 1 | `width:6.67%` | `width:6.67%` |
| the 10 count-1 bars on page 2 | `width:100.00%` | `width:6.67%` |
| distinct widths among count-1 bars, page 2 | one: `width:100.00%` | one: `width:6.67%` |

The same measurement — one event — is drawn fifteen times longer on page 2 than
on page 1 before the change, and identically on both pages after it.

| `before`, page 2 | `after`, page 2 |
| --- | --- |
| ![before](results/before/stats-events-page2.png) | ![after](results/after/stats-events-page2.png) |

Page 1 of each: [before](results/before/stats-events-page1.png) ·
[after](results/after/stats-events-page1.png).

### A second finding, recorded but not the subject

Asking the server for page 2 of an aggregation is a second execution, and
nothing makes two executions of `stats count() by message` agree on an order.
In the `before` capture the two pages between them held 50 of the 60 groups:
ten appeared on both pages and ten were never shown at all. In the `after`
capture the two pages held all 60, each exactly once, because they are slices
of one fetched result. Each transcript records this under `coverage`.

The exact group names on a `before` page are therefore not stable. A rerun will
put different groups on page 2. The widths will be the same.

## Rerun

Two instances are needed, one per revision, and each capture must run against a
freshly held instance: the seed is not idempotent, and a second capture against
the same instance would double every count.

Create a worktree at the revision, then hold an instance from it:

```bash
git -C /path/to/trawl worktree add --detach /path/to/wt 9ed451cc   # or c1e82824
cd /path/to/wt
CARGO_TARGET_DIR=/path/to/a/disk-backed/cache \
  bin/app-experiment --events 20 --batch-size 10 --rate 20 --hold-seconds 1200
```

The runner prints the browser origin and the path of `private/browser-key`. It
mints no ingest key on disk, so mint one against that run's own database, whose
password is in `private/postgres.env` and whose mapped port comes from
`docker port <container> 5432/tcp` with the container named in `instance.json`:

```bash
DATABASE_URL="postgres://experiment:PASSWORD@127.0.0.1:PORT/fleet" \
  "$CARGO_TARGET_DIR/debug/fleet-admin" keys create \
  --name evidence-ingest --kind service --role experiment-ingest --expires 2h
```

Then capture, from the checkout that holds this directory:

```bash
TRAWL_EVIDENCE_LABEL=before \
TRAWL_URL=http://127.0.0.1:PORT \
TRAWL_API_URL=https://127.0.0.1:PORT \
TRAWL_BROWSER_KEY_FILE=/path/to/run/private/browser-key \
TRAWL_INGEST_KEY=THE_MINTED_KEY \
  node visual-evidence/visualization-whole-result/capture.mjs
```

`TRAWL_URL` is the browser origin the runner printed; `TRAWL_API_URL` is the
daemon's own `upstream` from `instance.json`, used for seeding and for one
independent whole-result probe. `TRAWL_EVIDENCE_OUT` overrides the results
root. Results land in `results/<label>/`. Stop each runner when the capture is
done and check that run's `report.json` for `cleanup` before moving on.

No credential, origin, port or hostname reaches a transcript or an image. The
footer prints the endpoint it is connected to, so the script replaces that text
with `Connected (redacted)` before every screenshot.

## What this does not prove

- Nothing about speed. The daemons are debug builds on a loaded workstation,
  and no timing here is a benchmark.
- Nothing above the fetch ceiling. What the chart does when an execution
  produces more rows than one fetch carries — the refusal naming both counts —
  is not exercised here; the browser suite's `aggregate-whole.spec.ts` covers
  it against the harness.
- Nothing about live mode. Both captures are snapshot queries.
- The `before` plotted counts are the rows that reached the browser, not pixels
  counted off the canvas. That revision publishes no plotted-length attribute,
  which is itself part of what the fix added.
- One corpus, one four-hour window, one viewport (1280×800), one browser: the
  Chromium pinned by `crates/trawl-web-ui/e2e`. It is a demonstration on chosen
  data, not a proof over all results.
