# Issue #197 evidence: the histogram counts the whole result

Recorded on 2026-11-10 (UTC) by `run.sh`. The stack was built from this
branch at `bb7a70ba`, the commit before this directory. Nothing here was
edited by hand after recording.

## What is here

| File | What it is |
| --- | --- |
| `run.sh` | The recipe: it builds, starts a disposable stack, seeds it, captures both pages and tears the stack down |
| `seed.ndjson` | The 120 events `run.sh` sends to `/api/v1/ingest` |
| `page-1.png` | The Search page at page 1 of `service=checkout \| sort -_time` |
| `page-2.png` | The same search after one press of **Next** |

## The seed

The 120 `checkout` events start at `2026-09-01T10:00:00Z` and are 30
seconds apart, so the last one is at `10:59:30Z`. The 30 oldest events
(001 to 030, `10:00:00Z` to `10:14:30Z`) have `level=error`. The other 90
have `level=info`. Each event names itself in `message` (`event 001: …`).

The query sorts newest first, so page 1 holds events 120 to 071, page 2
holds 070 to 021, and page 3 holds 020 to 001. Page 1 has no error rows.
Page 2 has 10. Page 3 has 20.

## What the captures show

Both pages show the same caption, `All 120 events in this result · first
to last event · ≈1.2min per bucket · UTC`. They also show the same 48 bins,
and `run.sh` compares every bar's tooltip on both pages to confirm this.
The first 12 bins, `10:00` to `10:15`, are red: they hold the 30 errors.

- `page-1.png`: the table shows events 120 to 107, all `info`, and the
  filter rail counts 50 `info` rows on the page. The strip still shows the
  30 errors that are on pages 2 and 3.
- `page-2.png`: the table is scrolled to the end of page 2 and shows events
  033 to 021, with errors 030 to 021. The rail counts 10 `error` rows on
  this page. The strip shows all 30 errors, including the 20 on page 3.

`run.sh` printed this for the recorded run:

```text
page 1: caption "All 120 events in this result · first to last event · ≈1.2min per bucket · UTC"; strip errors 30; error rows on the page 0
page 2: caption "All 120 events in this result · first to last event · ≈1.2min per bucket · UTC"; strip errors 30; error rows on the page 10
same caption and bins on both pages: true
```

## Run it again

From the repository root:

```sh
(cd crates/trawl-web-ui/e2e && npm ci --no-audit --no-fund && npx playwright install chromium)
docs/evidence/issue-197/run.sh
```

`run.sh` needs Docker and Trunk. It builds `trawld`, `trawl-web`,
`fleet-admin` and the SPA from the checkout. It uses the container
`trawl-197-evidence` and ports 5473, 18497 and 18498. It never reads a
saved CLI profile and never touches a `bin/dev` stack. Its keys, data and
SPA build live in a temporary directory under the Cargo target directory.
On exit it stops both daemons, removes the container and deletes that
directory, whether the run passed or failed.

The script fails if the two pages differ in caption or bins, if the strip
does not show 30 errors, or if page 1 has an error row. It overwrites
`page-1.png` and `page-2.png`. Each run executes the query again, so the
`Started` time in the captures changes.
