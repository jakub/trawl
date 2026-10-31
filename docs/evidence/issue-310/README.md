# Issue #310 evidence: investigations over MCP and in the browser

Recorded on 2026-10-25 (UTC) against a disposable stack built from this
branch at `2f7f4907`. One run of the recipe below produced the transcripts,
the probe, `roles.txt` and `seed.ndjson`, in this order: stack up at
22:46:04, seed at 22:46:16, the agent and then the person's Claude Code
session from 22:46:19, the renders, the probe and a privacy scan at 22:47:17,
and stack down, done at 22:47:20. The four transcript files are the output of
`sanitize.py` and `render.py` run over the two raw streams that run recorded.
The raw streams are not committed: the repository's `.gitignore` refuses
`*.raw.jsonl` wherever a run writes it. The two frames files were reduced
after recording by `frames.py`, which drops Claude Code's own keys from each
request's `_meta` (see [Privacy](#privacy)) and changes nothing else. Nothing
here was edited by hand after recording.

The sessions ran Claude Code 2.1.310 (the `clientInfo` version in the first
client frame of each frames file). `transcripts.sh` asks for `--model opus`.

The phone captures were taken at `f16d660c`.
`git diff --stat f16d660c..2f7f4907 -- crates/trawl-web-ui` is empty, so the
SPA they show is the SPA at `2f7f4907`.

## What is here

| File | What it is | Criterion |
| --- | --- | --- |
| `transcript-agent.jsonl` | `sanitize.py` of the agent session's `claude -p --output-format stream-json` stream: 8 tool calls, their 8 results and 2 text events, one JSON object per line | Agent recipe end to end |
| `transcript-agent.md` | `render.py` of `transcript-agent.jsonl`: each tool call, its arguments and its result, then the final answer | Agent recipe end to end |
| `frames-agent.jsonl` | The 10 client frames `mcp-tee.sh` logged between Claude Code and `trawl mcp` in the agent session, with the `_meta` block Claude Code adds to each request reduced by `frames.py` to its MCP-standard `io.modelcontextprotocol/` keys | Agent recipe end to end |
| `transcript-human.jsonl` | `sanitize.py` of the person's session's stream: 3 tool calls, their 3 results and 1 text event | Agent recipe end to end, two principals |
| `transcript-human.md` | `render.py` of `transcript-human.jsonl` | Agent recipe end to end, two principals |
| `frames-human.jsonl` | The 5 client frames of the person's session, reduced the same way | Agent recipe end to end, two principals |
| `probe-direct.jsonl` | Every JSON-RPC request `probe.py` sent to `trawl mcp` and every response, in order, on both profiles | Agent recipe end to end (tool half), two principals |
| `probe-direct.txt` | `probe.py`'s stdout: each tool call and the text block it returned | Agent recipe end to end (tool half), two principals |
| `roles.txt` | `fleet-admin roles show` for the two roles `stack.sh` minted from `operate/access.md` | Agent recipe end to end, permissions |
| `investigation-phone.png` | The investigation page on an iPhone 13 viewport, captured whole | Read-only list and page on a phone |
| `investigations-phone.png` | The investigation list on the same viewport | Read-only list and page on a phone |
| `seed.ndjson` | The 50 events `stack.sh seed` sent to `/api/v1/ingest` | |
| `stack.sh`, `seed.py`, `transcripts.sh`, `mcp-tee.sh`, `sanitize.py`, `frames.py`, `render.py`, `probe.py`, `check.py` | The recipe, below | |

## The seed

`seed.py` counts back from the second it runs, here about `22:46:17Z`, and
puts a `shop-api` deploy 32 minutes before that, at `22:14:17Z`. Every event
has `env=prod`.

| Service and host | Rows sent | What they say |
| --- | --- | --- |
| `nginx` on `web-02` | 8 `POST /checkout 200`, one every 2 minutes from 20 minutes before the deploy; 10 `POST /checkout 502 upstream prematurely closed connection`, one every 30 s from 1 minute after it; 6 `GET /healthz 200` | Checkout worked, then answered 502; health checks kept passing |
| `shop-api` on `app-01` | 8 `order N placed`, each 1 s after its 200; `deploy 4812 started`; `deploy 4812 finished; pool size 20` 40 s later; 10 `db pool exhausted: connection refused`, each 40 ms before a 502 | The deploy, then a database pool that runs dry |
| `postgres` on `db-01` | 6 `FATAL: too many connections for role "shop"`, from 70 s after the deploy | The database refusing the pool |

The first 502 (`22:15:17.040Z`) comes 40 ms after the first pool exhaustion
(`22:15:17Z`), a minute after the deploy started. The sessions search
`last=1h`, so they must run within about 30 minutes of the seed.

## What the transcripts show

Both sessions ran `claude -p` with the prompts in `transcripts.sh`. Each
transcript ends with the model's text answer.

The agent session, on the `agent` profile (`claude-agent`, a service key on
the `trawl-agent` role), made eight tool calls:

1. `ToolSearch` with `select:mcp__trawl__orient,mcp__trawl__investigation,mcp__trawl__search`.
   This is Claude Code's own tool. It loads the schemas of deferred MCP tools
   and sends no frame to `trawl mcp`. Its result holds three
   `tool_reference` blocks, which the render shows as 0 characters.
2. `orient`: permissions `query, schema_read, validate, investigation_read,
   investigation_write`; `nginx`/`web-02` 24, `shop-api`/`app-01` 20,
   `postgres`/`db-01` 6 events in the last 24 hours;
   `"open_investigations_note":"none"`; `"degraded_fields":[]`.
3. `investigation` `create`, with the title, question and scope the prompt
   gives: investigation `ef79a020cf440d6009d606e5bf707de2`, open, seq 0.
4. `search` attached to it, `service=nginx status=502 last=1h`, purpose
   "Find when checkout started answering 502 and how long it kept doing so.":
   10 rows, result `3f9e0da2c7ff997f9e957eb73734969e`. The step is entry #1,
   completed at seq 2.
5. `investigation` `keep`, rows `[9, 0]` of that result, with the reason
   "Result is sorted newest first: row 9 (22:15:17Z, order=7731) is the first
   checkout 502 in the hour, row 0 (22:19:47Z, order=7740) the last; together
   they bound the 502 window.": entry #3. A text event follows: "kept the 502
   bookends (rows 9 and 0). now pulling shop-api's logs around 22:15z."
6. `search` attached, `service=shop-api last=1h`: 20 rows, result
   `40df5f02060806246b8b6fc91414cc73`. The reply carries 16 rows
   (`"rows_dropped_to_fit":4`) and a `next_actions` entry for `read` `page`
   at offset 16. The step is entry #4, completed at seq 5.
7. `investigation` `finding`, `supported`: entry #6.
8. `investigation` `read`: "Investigation
   "ef79a020cf440d6009d606e5bf707de2" is open at seq 6.", with the four
   entries #1, #3, #4 and #6; the two steps carry `updated_seq` 2 and 5.

The finding's statement is "Checkout 502s began at 22:15:17Z, 20 s after
shop-api deploy 4812 finished with a db pool of 20, and every 502 coincides
to the second with a shop-api "db pool exhausted: connection refused" error
whose waiting count climbs from 41 to 59." It cites rows 9 and 0 of the nginx
result (the first and the last 502) and rows 11, 10, 9 and 0 of the shop-api
result (deploy started, deploy finished, the first and the last pool
exhaustion). Its remaining question is "Why does the pool exhaust after deploy
4812: did the deploy shrink the pool or leak connections, or is postgres
refusing them? postgres logs and the pre-deploy pool size are not yet
examined."

The final answer repeats the ids, the kept rows, the finding, the cited rows,
the open question and the read's sequence. It also qualifies its own finding
against what the tools printed:

> "to the second" is loose: nginx stamps land at .040 and shop-api at .000,
> so the two line up within 40 ms. "every" covers only the shop-api rows the
> reply printed, 16 of 20, and i never paged the other 4.

The person's session, on the `human` profile (`evidence-human`, a human key
on the `trawl-investigation-reader` role), made three tool calls:

1. `ToolSearch` with `select:mcp__trawl__orient,mcp__trawl__investigation`.
2. `orient`: permissions `query, schema_read, investigation_read`;
   `"degraded_fields":[]`; `open_investigations` lists
   `ef79a020cf440d6009d606e5bf707de2`, "502s on checkout", open, seq 6,
   updated by `claude-agent` (`dliKY9MU`).
3. `investigation` `read` on that id: the same 3600-character board the agent
   session read, open at seq 6.

Its final answer names the investigation, says "it's the SAME id the agent
session created, a clean match.", names `claude-agent` as the opener, and
restates the latest finding, its cited rows and the open question.

In both transcripts, the result of every `trawl` tool call is one string that
parses as a JSON object. Each object has the top-level fields of the
`structuredContent` that `probe.py` received for the same tool and action in
`probe-direct.jsonl`. The one difference is the agent's `orient`, which has
`open_investigations_note` in place of `open_investigations`, because no
investigation was open yet. The text block that `trawl mcp` also returns, the
one `probe-direct.txt` prints (for example the `version: trawld 0.9.3` line of
`orient`), appears in neither transcript. Claude Code handed the model the
structured JSON and not the text block. The renders show that JSON for every
call. The agent's mid-session text event is in `transcript-agent.jsonl`; the
render prints only the last text event, as the final answer.

The frames files hold what Claude Code sent. Each session opens with
`server/discover` at protocol version `2026-07-28`, then `resources/list` and
`tools/list`, then one `tools/call` per `trawl` tool call: seven in the agent
session, two in the person's.

## What the probe shows

`probe.py` ran at 22:47:17, after the transcripts, on the same stack. It
opened two `trawl mcp` sessions, one after the other, each through
`initialize` at `2025-11-25`. Both sessions exited 0. Only the last call
returned `isError`, as intended.

The agent session, on the `agent` profile:

1. `orient`: permissions `query, schema_read, validate, investigation_read,
   investigation_write`; the same three groups; one open investigation,
   `"ef79a020cf440d6009d606e5bf707de2" open, seq 6, changed
   2026-10-25T22:46:51.274922Z by dliKY9MU "claude-agent": "502s on checkout"`,
   the transcript's; "degraded fields: none".
2. `investigation` `create`: investigation
   `675f9afb0e85f530f868cceb8fbbe149`, open, seq 0, evidence kept until
   2026-11-01 and at most until 2026-11-24.
3. `search` attached to it, `service=nginx status=502 last=1h`: 10 rows,
   result `253dd7bdb5f202c24735740948e7208f`, `kept`. The step is entry #1;
   its completion is seq 2.
4. `investigation` `keep`, rows `[0, 9]` of that result, reason "The last and
   the first 502 of the burst": entry #3.
5. `search` attached, `service=shop-api last=1h`: 20 rows, result
   `666bae84c8e12db99506a0e41f2bfa7c`, `kept`. "This response prints 16 rows
   from captured row 0, … To fit the 16384-byte budget it dropped 4 rows."
   Step #4, completed at seq 5.
6. `investigation` `finding`, `supported`, "Checkout answers 502 because
   shop-api exhausts its database pool right after deploy 4812.", citing row 9
   of each result (the first 502 and the first pool exhaustion): entry #6.
7. `investigation` `read`: "open at seq 6", four entries in change order, the
   two steps marked "(changed at #2)" and "(changed at #5)".

Both attached searches, the keep and the finding printed "activity by other
keys since #0, in change order: none."

The person's session, on the `human` profile:

1. `orient`: permissions `query, schema_read, investigation_read`; two open
   investigations, most recently changed first: `675f9afb0e85f530f868cceb8fbbe149`
   and `ef79a020cf440d6009d606e5bf707de2`, both open at seq 6, both changed by
   `claude-agent`; "degraded fields: none".
2. `investigation` `read` on `675f9afb0e85f530f868cceb8fbbe149`: open at seq 6,
   the same four entries.
3. `read` `page` on result `253dd7bdb5f202c24735740948e7208f`, offset 9, one
   row: the first 502 (`22:15:17.040Z`), read by a key that did not capture
   it.
4. `investigation` `note`: refused with `isError`:

   ```text
   investigation was refused: forbidden (HTTP 403).
   trawld says: insufficient permissions
   investigation action=note needs the query, investigation_read and investigation_write permissions; orient lists this key's.
   ```

## The roles

`roles.txt` shows `trawl-agent` with `trawl:investigation_read,
trawl:investigation_write, trawl:query, trawl:schema_read, trawl:validate` at
60 requests a minute, and `trawl-investigation-reader` with
`trawl:investigation_read, trawl:query, trawl:schema_read` at the class
default. The second role is the one `operate/access.md` creates for a person
(`docs/src/content/docs/operate/access.md`, lines 118-119). The person's key
holds `schema_read`, so its `orient` reads the per-service schema: the
transcript prints `"degraded_fields":[]` and the probe prints "degraded
fields: none".

## The phone captures

The two PNGs come from the Playwright suite, not from the stack above. The
suite serves recorded wire bodies (`crates/trawl-web-ui/e2e/harness/wire/`)
through its stub server, with the clock fixed at `2026-10-24T09:45:00Z` and
the status bar masked (the magenta band). Two runs at `f16d660c` wrote the
same bytes:

```text
a1a3d4b47c11dddbaf9833794ee6f8838de4a61bbb7ca20c561759060b294b6e  investigation-phone.png
02ae60854666474d919385c3a4a695ba23ffff803de4f5682187010fbb12d7ab  investigations-phone.png
```

The tests that pin the pages are in
`crates/trawl-web-ui/e2e/tests/investigations.spec.ts`:

- "on a phone › an investigation reads question, latest finding, findings
  with their rows, then the log" holds the whole page in one `ariaSnapshot`
  and writes `investigation-phone.png`.
- "on a phone › the list is one row per investigation, each a link to it"
  writes `investigations-phone.png`.
- "without investigation_read there is no rail entry, and the pages say so
  and ask nothing" shows the controls are absent for a key without the grant.

## Observations

- The agent session's first search returned the hint "the catalog has no pin
  for field "status"", 15 seconds after ingest. The probe's same query, 43
  seconds later, printed no catalog hint. Both matched all 10 rows.
- Every search printed a `lag` hint: 1862 s for `nginx`, 3121 s for
  `shop-api`. These measure the seed, not a slow sender. `seed.py` stamps
  events up to 52 minutes in the past and sends them in one request, and
  every row carries `_ingested` `22:46:19.937220Z`.
- Each `claude -p` stderr log held 152 lines of `unknown format "uint64"
  ignored in schema at path …` (150) and `unknown format "uint16" …` (2) from
  Claude Code's schema validator. The tool schemas carry the integer formats
  schemars writes, and the client ignores them. The agent's log held one more
  line, `Warning: no stdin data received in 3s, proceeding without it.`,
  because `transcripts.sh` runs `claude -p` without a stdin redirect. The
  stderr logs are not committed.
- `transcripts.sh` passes `--setting-sources user`, so the sessions ran with
  the recording machine's user settings. Neither prompt asks for lower case,
  and both final answers are in it.
- The #308 and #309 recipes build with `cargo build -p trawl-server --bin
  trawld -p trawl-cli --bin trawl -p fleet-admin`. With `--bin` flags, cargo
  builds only the named binaries, so `fleet-admin` is not rebuilt. A stale
  `fleet-admin` migrates the Fleet database only as far as the migrations it
  embeds, and the new trawld refuses to start with `PendingMigration`. The
  recipe below builds `fleet-admin` in its own command.

## Privacy

The committed files hold no credential. The three keys existed only under the
run directory, in the private HOME and `private/`, which `stack.sh down`
deleted. Before `stack.sh down`, the run scanned every output file, the three
uncommitted stderr logs included. It searched for the full agent, human and
sender key values read from the run directory, and for the pattern
`Bearer |trawl_[A-Za-z0-9]|flt_[A-Za-z0-9]|[Cc]ookie`. Every file had 0 hits;
the run log, which is not committed, printed the count per file.

`orient` prints each key's name and its 8-character lookup prefix by design:
`dliKY9MU` for `claude-agent`, `Z-s4EsgL` for `evidence-human`. The prefix is
not the key, and the key expired with the database it lived in. The phone
captures show only the wire fixtures' invented names and data.

The raw Claude Code streams are never committed. They held the recording
machine's `SessionStart` hook output, its local paths, its plugin and model
inventory, session, message and tool-use ids, token usage and cost records,
and the model's thinking blocks with their signatures. None of that is here.
`sanitize.py` builds each committed transcript from an allowlist and keeps
five shapes: a `tool_use` event with a call number, the tool name and its
input; a `tool_result` event with the call number, `is_error` and the content
as the tool returned it; a `text` event with the model's prose; an `init`
event with the Claude Code version and the model; and a `result` event with
the session's outcome, turn count and duration. The two transcripts here
were sanitized before `sanitize.py` kept `init` and `result`, so they hold
only the first three. It drops
every other event, block and field, including every id; the call number
replaces the tool-use id. It then checks its own output as
`sanitize.py --verify` does and fails on any failure: `check.py`'s patterns
over each line and over every string decoded from it, and the five shapes.
CI and the pre-commit hook run `--verify` over every tracked `*.jsonl` and
`*.ndjson` under `docs/` and `visual-evidence/`.

The frames files held tool-use ids too. Claude Code adds a `_meta` block to
each request it sends `trawl mcp`. Beside the MCP-standard
`io.modelcontextprotocol/` keys (the protocol version, `clientInfo` and the
client capabilities), each `tools/call` frame's block carried Claude Code's
tool-use id and a `progressToken`. `frames.py` keeps every frame as
`mcp-tee.sh` logged it, except that each `_meta` keeps only its
`io.modelcontextprotocol/` keys, so the tool-use ids and progress tokens are
removed and the `clientInfo` version cited above stays. It writes the frames
in the compact form they were logged in, so a second run changes nothing, and
it scans its output with `check.py`'s patterns and fails on a hit.

Before the files are committed, `check.py` runs over this directory. It reads
every file except the two PNGs and fails, naming file and line, on an absolute
home path, a cost, usage, signature, session id or uuid field, the bearer
scheme followed by a token, a `trawl_` or `flt_` key, a match for
`[Cc]ookie`, Claude Code's tool-use id key, or a `toolu_` id. Over the
committed directory it prints `check: 20 files, 0 hits`.

## Recipe

From a checkout with Docker available and Claude Code signed in:

```bash
cargo build -p trawl-server --bin trawld -p trawl-cli --bin trawl && cargo build -p fleet-admin
docs/evidence/issue-310/stack.sh up      # postgres:18 on 127.0.0.1:5480, trawld on 127.0.0.1:18450, the two roles and keys, a private HOME
docs/evidence/issue-310/stack.sh seed    # the 50 events above; writes seed.ndjson to the run directory
docs/evidence/issue-310/transcripts.sh /tmp/out   # within 30 minutes of the seed; agent then human
python3 -I docs/evidence/issue-310/render.py /tmp/out/agent.jsonl /tmp/out/agent.md
python3 -I docs/evidence/issue-310/render.py /tmp/out/human.jsonl /tmp/out/human.md
python3 -I docs/evidence/issue-310/probe.py target/debug/trawl target/evidence-310/home /tmp/out/probe.jsonl
docs/evidence/issue-310/stack.sh down
python3 -I docs/evidence/issue-310/check.py docs/evidence/issue-310   # after copying the outputs in; must report 0 hits
```

`transcripts.sh` keeps each session's raw stream as `/tmp/out/<name>.raw.jsonl`
and runs `python3 -I sanitize.py /tmp/out/<name>.raw.jsonl /tmp/out/<name>.jsonl`;
only the sanitized `<name>.jsonl` is copied into this directory, as
`transcript-<name>.jsonl`. It keeps each session's frames log as
`/tmp/out/<name>.frames.raw.jsonl` and runs
`python3 -I frames.py /tmp/out/<name>.frames.raw.jsonl /tmp/out/<name>.frames.jsonl`;
only the reduced `<name>.frames.jsonl` is copied in, as `frames-<name>.jsonl`.
To sanitize a raw stream, or reduce a frames log, again:

```bash
python3 -I docs/evidence/issue-310/sanitize.py /tmp/out/agent.raw.jsonl docs/evidence/issue-310/transcript-agent.jsonl
python3 -I docs/evidence/issue-310/frames.py /tmp/out/agent.frames.raw.jsonl docs/evidence/issue-310/frames-agent.jsonl
```

The run directory is `$TRAWL_EVIDENCE_DIR`, by default
`target/evidence-310`. `stack.sh` is #309's with its own container,
`trawl-310-pg-e`, its own ports, this seed, and the two roles of
`operate/access.md` ("Create a key for an agent") minted verbatim:
`trawl-agent` (`query`, `schema_read`, `validate`, `investigation_read`,
`investigation_write`, 60 requests a minute) for a service key with a 90-day
expiry, and `trawl-investigation-reader` (`query`, `schema_read`,
`investigation_read`) for a human key. `mcp-tee.sh` is #308's with the
profile taken from `TRAWL_EVIDENCE_PROFILE`. `transcripts.sh` runs the agent
session, takes the investigation id from the result of its `create` call in
the sanitized stream, and then runs the person's session.

The phone captures need a release build of the SPA:

```bash
(cd crates/trawl-web-ui && trunk build --release --dist "$PWD/../../target/spa-evidence")
(cd crates/trawl-web-ui/e2e && npm ci && E2E_PORT=8613 TRAWL_E2E_DIST="$PWD/../../../target/spa-evidence" \
  TRAWL_E2E_EVIDENCE=1 npx playwright test tests/investigations.spec.ts)
```
