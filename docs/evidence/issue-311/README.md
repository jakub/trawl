# Issue #311 evidence: the live board, two principals

Recorded on 2026-10-27 (UTC) against a disposable stack built from this
branch at `832a1204`: trawld and trawl-web debug builds, and the SPA built
with `trunk build --release`. One run of the recipe below produced every
file here except the two stub-suite captures, in this order: stack up from
01:54:59 to 01:55:17, seed at 01:55:17, the two-principal loop from 01:55:25
to 01:55:44, `check.py` and a scan for the run's four key values at
01:55:49, and stack down. Nothing here was edited by hand after recording.

The stub-suite captures, `board-phone-before.png` and
`board-phone-after.png`, were written at `832a1204` by the Playwright suite,
not by the stack (see [The stub-suite captures](#the-stub-suite-captures)).

## What is here

| File | What it is | Criterion |
| --- | --- | --- |
| `agent-frames.jsonl` | Every JSON-RPC frame `agent.py` sent to and received from its one `trawl mcp --profile agent` process, in order, each with the UTC instant it was logged: `initialize`, `tools/list` and nine `tools/call` | AC1, AC2, AC3 |
| `agent.txt` | `agent.py`'s log: each tool call, the text block it returned, and each wait for the browser | AC2 |
| `board.jsonl` | Every observation `board.mjs` logged: the board's scroll measurements, the keep, each Search query, each capture request with its answer, the board's steps and the rows on both sides, and the controls each session has | AC1, AC2, AC3, AC4 |
| `board-phone-live-before.png`, `board-phone-live-after.png` | The board on an iPhone 13 viewport before and after the agent's step arrived, the finding at the top edge in both | AC1 |
| `board-phone-kept.png` | The evidence viewer after the keep, saying "Kept row 9." | AC2 |
| `send-phone-absolute.png`, `send-phone-pin.png` | The Send to investigation dialog for the fixed range and for the moving range, before each send | AC3 |
| `board-phone-final.png` | The top of the board after the loop | AC1, AC3 |
| `writer-aria.txt`, `reader-aria.txt` | The board header, the viewer's row 9 and Search's trailing actions as `ariaSnapshot` text, for the writer's session and for the read-only person's session | AC4 |
| `board-phone-before.png`, `board-phone-after.png` | The stub suite's AC1 captures | AC1 |
| `roles.txt` | `fleet-admin roles show` for the three roles `stack.sh` minted from `operate/access.md` | AC4, AC8 |
| `keys.txt` | `fleet-admin keys list`: each key's lookup prefix, name, kind and role, never the key | AC2 |
| `seed.ndjson` | The 50 events `stack.sh seed` sent to `/api/v1/ingest` | |
| `stack.sh`, `run.sh`, `agent.py`, `board.mjs`, `check.py` | The recipe, below | |

## The stack and the principals

`stack.sh` runs Postgres 18 in Docker on `[::1]:5451`, trawld on
`127.0.0.1:18460` and trawl-web on `127.0.0.1:18461`. trawl-web serves the
release SPA and pins trawld's generated certificate. trawld's `link_origin`
is trawl-web's origin, so the links the agent's replies print open the
browser stack.

It mints the three roles of `operate/access.md` verbatim (`roles.txt`) and a
key on each:

| Key | Prefix | Kind | Role | Used by |
| --- | --- | --- | --- | --- |
| `claude-agent` | `eO5EQUCT` | service | `trawl-agent`: `query`, `schema_read`, `validate`, `investigation_read`, `investigation_write`, 60 requests a minute | `agent.py`, through a CLI profile in a private HOME |
| `evidence-writer` | `XzIEEYs0` | human | `trawl-investigation-writer`: `query`, `schema_read`, `investigation_read`, `investigation_write` | `board.mjs`, signed in through trawl-web |
| `evidence-reader` | `8KwL1OXN` | human | `trawl-investigation-reader`: `query`, `schema_read`, `investigation_read` | `board.mjs`, for AC4 |

A fourth key, `evidence-sender`, holds only `ingest` and sends the seed.

## The seed

`stack.sh seed` runs `docs/evidence/issue-310/seed.py`, the #310 incident,
unchanged. It counts back from the second it runs, here `01:55:17Z`, and
puts the `shop-api` deploy 32 minutes earlier, at `01:23:17Z`. Checkout on
`nginx` answers 502 from `01:24:17.040Z`, 40 ms after `shop-api` first logs
`db pool exhausted`, and `postgres` refuses connections from `01:24:27Z`.
[Issue #310's README](../issue-310/README.md#the-seed) lists every row. The
agent searches `last=1h`, so the loop must run within about 8 minutes of
the seed for `shop-api`'s first orders to stay inside the hour.

## What the run shows

`run.sh` starts `agent.py` and `board.mjs` side by side. They hand over
through files in the run's `signals/` directory; `agent.py`'s module
documentation lists them.

### The agent sets up (calls 1 to 6)

`agent.py` keeps one `trawl mcp` process open for the whole run, so the
`seen_seq` that process holds for the investigation lasts from the first
attached search to the last. It calls `orient`, then `investigation`
`create`: investigation `6990775352dac57478211e8fa0b948ce`. It runs two
attached searches, `service=nginx status=502 last=1h` (10 rows, result
`d8c09c28…`) and `service=shop-api last=1h` (20 rows, result `c5820d0a…`),
records a `supported` finding citing row 9 of each (the first 502 and the
first pool exhaustion) as entry #5, and writes a note as #6. Every call
prints "activity by other keys since #0, in change order: none."

### AC1: the agent writes while the person reads

`board.mjs` signs in as `evidence-writer` and opens
`/investigations/6990775352dac57478211e8fa0b948ce` on an iPhone 13 viewport
(390 × 664 CSS px; the page's scroller is 596 px high). Once the board says
"Live", it scrolls the page's scroller so the finding (`finding-5`) starts
at the scroller's top edge, 0.25 px below it, with `scrollTop` 965. By the
board's own rule (`pick_anchor` in
`crates/trawl-web-ui/src/investigation_live.rs`, which `board.mjs` mirrors
to measure) the finding is the element that holds the reader's place. It
then tells the agent.

The agent runs `service=postgres last=1h` attached (call 7, 6 rows). Its
step lands below the finding, and its activity line lands above it. The
board read it at its next poll: the step was on the board 5298 ms after the
board told the agent, one poll interval. The board grew by 161 px (86 px of
it above the finding), `scrollTop` went from 965 to 1051, and the finding
stayed at 0.5 px below the top edge: it moved 0.25 px. The page's live
region said "1 new entry." `board-phone-live-before.png` and
`board-phone-live-after.png` show the same view before and after.

### AC2: a keep from the board reaches the agent once

The person taps "Open Row 9 of result d8c09c28 among its rows" on the
finding's first cited row. The evidence viewer opens on that row's page
(1–10 of 10) with row 9 marked, `01:24:17.04`, the first checkout 502. They
tap Keep row 9, write the reason "The first checkout 502, 40 ms after
shop-api first ran out of pool connections" and press Enter. The board
posts `{"kind":"keep","result_id":"d8c09c28…","rows":[9],"reason":…}` and
trawld answers 201 with entry #9 under `XzIEEYs0` `evidence-writer`.

The agent's next attached search (call 8, `service=nginx path=/healthz
last=1h`) carries it:

```text
activity by other keys since #0, in change order: 1 entry
#9 keep by XzIEEYs0 "evidence-writer" at 2026-10-27T01:55:37.850207Z: kept rows [9] of result "d8c09c284efe7e5bfa4aaa605b0f62d9"; reason "The first checkout 502, 40 ms after shop-api first ran out of pool connections"
```

The call after that (call 9) starts from #9 and prints the person's two
sends below, not the keep.

### AC3: Search sends to the investigation

The fixed range. `board.mjs` opens Search with `service=nginx status=502`
and `r=2026-10-27T01:23:17Z..2026-10-27T01:33:17Z`, the ten minutes after
the deploy: 10 rows. The dialog offers the investigation by title and short
id with none chosen, shows the query `service=nginx status=502` and "From
2026-10-27T01:23:17.000000Z up to, not including,
2026-10-27T01:33:17.000001Z". The person chooses it, gives the purpose "The
502s in the ten minutes after deploy 4812" and sends. The capture request
carries that text, the window `[01:23:17.000000Z, 01:33:17.000001Z)` and
the investigation. trawld answers 200 with result `1d3dfe14…`, and the toast
says "Step 12 in 502s on checkout".

The moving range. Search with `service=shop-api` and `r=1h` ran at
`2026-10-27T01:55:39.134035533Z` (`execution.started_at`) and showed 20
rows. The dialog says the range moves and offers to pin it to
"2026-10-27T00:55:39Z to 2026-10-27T01:55:40Z", the whole-second hull of the
hour that ran. Pin and re-run navigated Search to
`r=2026-10-27T00:55:39Z..2026-10-27T01:55:40Z`, ran
`_time>="2026-10-27T00:55:39Z" _time<="2026-10-27T01:55:40Z"
service=shop-api` (20 rows), then sent `service=shop-api` with the window
`[00:55:39.000000Z, 01:55:40.000001Z)`: result `b52c16b4…`, step 14.

On the board, each send is a step by `evidence-writer` whose executed text
is the one trawld answered, for example
`earliest="2026-10-27T01:23:17.000000Z" latest="2026-10-27T01:33:17.000001Z"
service=nginx status=502` and `| sort -_time, -_ingested` on its own line.
The agent's call 9 prints both steps as the server stored them, under
`XzIEEYs0`, with bounds equal to the windows the browser sent:

```text
activity by other keys since #9, in change order: 2 entries
#12 (changed at #13) step by XzIEEYs0 "evidence-writer" … from 2026-10-27T01:23:17.000000Z (inclusive) to 2026-10-27T01:33:17.000001Z (exclusive) …
#14 (changed at #15) step by XzIEEYs0 "evidence-writer" … from 2026-10-27T00:55:39.000000Z (inclusive) to 2026-10-27T01:55:40.000001Z (exclusive) …
```

Each step's viewer pages the events the Search table showed: 10 of 10 and
20 of 20. As a set of rows, the viewer's rows equal Search's cell for cell:
the same `_raw` and the same displayed text in every column, `_severity`
included, which reads `error` (and `info` in the 20-row step) in both. The
rows come in a different order; see [Observations](#observations).

### AC4: the read-only person has no write control

`board.mjs` signs in again as `evidence-reader` and records the same three
views as for the writer. `writer-aria.txt` has `button "Close the
investigation"`, `button "Keep row 9"` and `button "Send to investigation"`.
`reader-aria.txt` has the same header, row and actions without any of them.

## What check.py verifies

`python3 -I check.py docs/evidence/issue-311` reads the committed files
and prints one line per claim:

- Privacy: issue-310's patterns over every file but the PNGs, with its
  `[Cc]ookie` word rule narrowed to a cookie header, a storage state's
  cookie list or trawl-web's session cookie name, because `stack.sh` writes
  trawl-web's cookie configuration keys.
- AC1: the step arrived within 10 s; the finding holds the reader's place
  before and after and moved at most 1 px.
- AC2: the keep is printed once, in the first attached call after it, under
  the writer key's prefix (from `keys.txt`) and name, with the row and
  reason the board sent; no later attached call prints it.
- AC3: each of the two sends has exactly one capture record and one
  `ac3 board step` observation, paired by purpose; both captures went to
  the agent's investigation; each server step is
  the writer's, with bounds equal to the window sent and the executed text
  trawld answered and the board shows; each viewer paged the Search table's
  events, and every displayed cell of those rows, `_severity` token
  included, equals Search's as a set of rows; the moving range was pinned
  to the whole-second hull of `[started_at − 1h, started_at]` and Search ran
  the pinned range before the send.
- AC4: the reader has no Close, Reopen, Keep or Send, and the writer has
  Close, Keep and Send.

It exits 1 on any failure. Over this directory it reports 22 `ok` lines
and `check: 0 failures`, and a `note order:` line per step. The negative
leg ran `check.py` over six tampered copies of the recorded files, each
changing one thing; each exited 1 on the claims named:

| Tamper | Failed claims |
| --- | --- |
| the keep repeated in call 9's activity | AC2 printed once; AC2 no later call prints it |
| the first capture's window 1 µs short | AC3 the step's bounds equal the window |
| the finding moved 5 px | AC1 holds the reader's place and moved at most 1 px |
| a Keep button in the reader's aria file and flag | AC4 both claims |
| the viewer's `_severity` cells as numbers (`17`, `9`) | AC3 the cells equal Search's, both steps |
| one viewer cell with a character appended | AC3 the cells equal Search's, first step |

`check.py` was tightened after the recording, at 2a16e347, to require one
board step per send and pair it with its capture by purpose; the recorded
files are unchanged. Over two more tampered copies it exits 1: with every
`ac3 board step` record removed, on AC3's board-step count; with every
viewer cell but `_raw` replaced, on AC3's cells for both steps.

## The stub-suite captures

`board-phone-before.png` and `board-phone-after.png` come from
`crates/trawl-web-ui/e2e/tests/investigation-board.spec.ts`, "on a phone ›
the board grows above and below a reader halfway down it without moving
what they read", against the suite's stub server with the clock frozen at
`2026-10-24T09:45:00Z` and the status bar masked. A stub finding lands above
the reader and a stub step below. The run at `832a1204` wrote these bytes:

```text
ee0e9ca19fb3c880f8ec60605790da2703ccbf2df47eb710e4b1cbe8f5ac82c6  board-phone-before.png
27a4dbab247c0b228bfef875fc495473c49ec66a2027eaac6b213ef236a45729  board-phone-after.png
```

## Observations

- The viewer's rows and Search's rows are the same set, cell for cell,
  `_severity` token included. Their order differs. trawld appends
  `| sort -_time, -_ingested` to every events-shaped capture
  (`reference/api.md`), so the viewer is newest first (`01:28:47.04` first
  for the nginx step). The `_time>=… _time<=…` query Search runs has no sort
  stage, so its table came back in the order the rows were stored (oldest
  first here, `01:24:17.04`). `send-to-investigation.spec.ts` asserts the two
  tables equal in order because its stub serves one fixed order to both.
- The board holds the place of the first keyed element still showing below
  its top edge, as `pick_anchor` documents. When the reader sits lower, so
  that the last activity line straddles the edge, that line holds the place
  and the finding below it moves down by the height of an activity line the
  agent adds. This recording reads the finding at the top edge, where the
  finding holds the place; `READ_AT=<px>` reads it lower.
- On the phone, the "Sent to investigation" toast covers the foot of the
  page, and its link keeps focus, which holds the toast open. `board.mjs`
  taps "Dismiss notification" to reach the step below it.
- In the `structuredContent` of `trawl mcp` replies, the executed text
  carries the line break before `| sort` as the two characters `\n`, as the
  text block prints it (`render::one_line`). The capture API and the board
  carry a real line break. `check.py` compares the two with that
  substitution.
- The agent's nginx result has a `deploy` column, `NULL` in every row,
  between `status` and `_raw`. This run's viewers and Search tables named
  the same columns. Why the column exists was not examined.
- Each search printed a `lag` hint, as in #310: the seed stamps events up to
  52 minutes in the past and sends them in one request.
- A private Postgres binds `[::1]` in its container. sqlx in `fleet-admin`
  could not resolve a DSN with the host `[::1]` ("failed to lookup address
  information"), so `stack.sh` connects through `localhost`.

## Privacy

The committed files hold no credential. The four keys and the browser
sessions' storage state existed only under the run directory, in
`private/` and the private HOME, which `stack.sh down` deleted. Before
`stack.sh down`, the run counted the four key values in every output file,
the uncommitted logs included, and in trawld's and trawl-web's logs: 0 in
each. `check.py` ran over the output and reported 0 privacy hits.

`keys.txt`, `agent.txt` and `agent-frames.jsonl` name each key's
8-character lookup prefix by design. The prefix is not the key, and the
keys expired with the database they lived in. `board.mjs` signs in on a
browser context that records nothing and works on new contexts that load
the stored session, so neither the key nor the sign-in form is in the
video. The video and the raw logs (`agent.stdout`, `agent.stderr`,
`board.stdout`) stay in the run's scratch directory and are not committed.

## Recipe

From a checkout with Docker, Node and the e2e suite's Playwright
installed (`(cd crates/trawl-web-ui/e2e && npm ci && npx playwright install
chromium)`):

```bash
cargo build -p trawl-server --bin trawld -p trawl-cli --bin trawl -p trawl-web --bin trawl-web && cargo build -p fleet-admin
(cd crates/trawl-web-ui && trunk build --release --dist "$PWD/../../target/spa-evidence-311")
docs/evidence/issue-311/stack.sh up      # postgres:18 on [::1]:5451, trawld on :18460, trawl-web on :18461, three roles and four keys
docs/evidence/issue-311/stack.sh seed    # the #310 incident; writes seed.ndjson to the run directory
docs/evidence/issue-311/run.sh /tmp/out /tmp/video   # within 8 minutes of the seed
python3 -I docs/evidence/issue-311/check.py /tmp/out   # must report 0 failures
docs/evidence/issue-311/stack.sh down
```

`fleet-admin` builds in its own command because `--bin` flags limit cargo to
the named binaries, and a stale `fleet-admin` leaves the Fleet database
short of the migrations the new trawld needs (#310's README). The run
directory is `$TRAWL_EVIDENCE_DIR`, by default `target/evidence-311`;
`$TRAWL_EVIDENCE_SPA` overrides the SPA directory. `run.sh` copies
`seed.ndjson`, `roles.txt` and `keys.txt` into the output directory beside
what the two drivers write. Copy `agent-frames.jsonl`, `agent.txt`,
`board.jsonl`, the PNGs, the two aria files, `roles.txt`, `keys.txt` and
`seed.ndjson` in. `READ_AT=<px>` in the environment makes `board.mjs` read
the finding that far below the top edge instead.

The stub-suite captures:

```bash
env -u NO_COLOR TRAWL_E2E_EVIDENCE=1 E2E_PORT=8631 cargo xtask e2e --grep "on a phone.*halfway"
```
