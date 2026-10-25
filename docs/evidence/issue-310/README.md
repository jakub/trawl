# Issue #310 evidence: investigations over MCP and in the browser

Recorded on 2026-10-25 (UTC). The MCP probe ran against a disposable stack
built from this branch at `f16d660c`. The phone captures were taken at the
same commit; the SPA code there is the same as at `367592de`, the commit that
completed the investigation pages. Nothing here was edited by hand after
recording.

**The Claude Code transcripts are not recorded yet.** The seat that built
this directory runs under a hook that refuses any `claude` command,
`claude --version` included: "flow seat (no model through the shell)". So
`transcripts.sh` has not run, and `transcript-agent.jsonl`,
`transcript-agent.md`, `transcript-human.jsonl`, `transcript-human.md`,
`frames-agent.jsonl` and `frames-human.jsonl` do not exist. What is here is
the recipe, ready to run, and `probe-direct.*`: the same tool calls sent to
`trawl mcp` over stdio by a script, with no model in the loop. The probe
shows what the tools answer on this seed. It does not show what Claude Code
does with those answers. The Claude Code version on the recording machine is
2.1.310, read from the install path because the hook also refused
`claude --version`.

## What is here

| File | What it is | Criterion |
| --- | --- | --- |
| `probe-direct.jsonl` | Every JSON-RPC request `probe.py` sent to `trawl mcp` and every response, in order, on both profiles | Agent recipe end to end (tool half), two principals |
| `probe-direct.txt` | `probe.py`'s stdout: each tool call and the text block it returned | Agent recipe end to end (tool half), two principals |
| `roles.txt` | `fleet-admin roles show` for the two roles `stack.sh` minted from `operate/access.md` | Agent recipe end to end, permissions |
| `investigation-phone.png` | The investigation page on an iPhone 13 viewport, captured whole | Read-only list and page on a phone |
| `investigations-phone.png` | The investigation list on the same viewport | Read-only list and page on a phone |
| `seed.ndjson` | The 50 events `stack.sh seed` sent to `/api/v1/ingest` | |
| `stack.sh`, `seed.py`, `transcripts.sh`, `mcp-tee.sh`, `render.py`, `probe.py` | The recipe, below | |
| `transcript-agent.jsonl`, `transcript-agent.md`, `frames-agent.jsonl` | Not recorded: run `transcripts.sh` outside a flow seat | Agent recipe end to end |
| `transcript-human.jsonl`, `transcript-human.md`, `frames-human.jsonl` | Not recorded: run `transcripts.sh` outside a flow seat | Agent recipe end to end, two principals |

## The seed

`seed.py` counts back from the second it runs, here about `08:13:35Z`, and
puts a `shop-api` deploy 32 minutes before that, at `07:41:35Z`. Every event
has `env=prod`.

| Service and host | Rows sent | What they say |
| --- | --- | --- |
| `nginx` on `web-02` | 8 `POST /checkout 200`, one every 2 minutes from 20 minutes before the deploy; 10 `POST /checkout 502 upstream prematurely closed connection`, one every 30 s from 1 minute after it; 6 `GET /healthz 200` | Checkout worked, then answered 502; health checks kept passing |
| `shop-api` on `app-01` | 8 `order N placed`, each 1 s after its 200; `deploy 4812 started`; `deploy 4812 finished; pool size 20` 40 s later; 10 `db pool exhausted: connection refused`, each 40 ms before a 502 | The deploy, then a database pool that runs dry |
| `postgres` on `db-01` | 6 `FATAL: too many connections for role "shop"`, from 70 s after the deploy | The database refusing the pool |

The first 502 (`07:42:35.040Z`) comes 40 ms after the first pool exhaustion
(`07:42:35Z`), a minute after the deploy started. The sessions search
`last=1h`, so they must run within about 30 minutes of the seed.

## What the probe shows

`probe.py` ran about 20 seconds after the seed and opened two `trawl mcp`
sessions, one after the other, each through `initialize` at `2025-11-25`.
Both sessions exited 0. Only the last call returned `isError`, as intended.

The agent session, on the `agent` profile (`claude-agent`, a service key on
the `trawl-agent` role):

1. `orient`: permissions `query, schema_read, validate, investigation_read,
   investigation_write`; three groups over the last 24 hours, `nginx`/`web-02`
   24, `shop-api`/`app-01` 20, `postgres`/`db-01` 6; "open investigations:
   none".
2. `investigation` `create`: investigation
   `d43c04dbb2e51b8620a536ec0ed185aa`, open, seq 0, created by the key's
   prefix and name, evidence kept until 7 days later and at most 30 days
   after creation.
3. `search` attached to it, `service=nginx status=502 last=1h`: 10 rows,
   result `d43923ef860509477a41ad6addd1559a`, `kept`. The step is entry #1;
   its completion is seq 2.
4. `investigation` `keep`, rows `[0, 9]` of that result: entry #3.
5. `search` attached, `service=shop-api last=1h`: 20 rows, result
   `3809f3b39631716fc22b2f8a3885d9d6`, `kept`, 16 printed within the
   16384-byte budget. Step #4, completed at seq 5.
6. `investigation` `finding`, `supported`, citing row 9 of each result (the
   first 502 and the first pool exhaustion): entry #6.
7. `investigation` `read`: "open at seq 6", four entries in change order,
   the two steps marked "(changed at #2)" and "(changed at #5)".

Both attached searches, the keep and the finding printed "activity by other
keys since #0, in change order: none.", since no other key wrote.

The person's session, on the `human` profile (`evidence-human`, a human key
on the `trawl-investigation-reader` role):

1. `orient`: permissions `query, investigation_read`; it lists
   `d43c04dbb2e51b8620a536ec0ed185aa`, open, seq 6, changed by `claude-agent`,
   "502s on checkout".
2. `investigation` `read`: the same board, seq 6, the same four entries.
3. `read` `page` on result `d43923ef860509477a41ad6addd1559a`, offset 9, one
   row: the first 502, read by a key that did not capture it.
4. `investigation` `note`: refused, `forbidden (HTTP 403)`, "trawld says:
   insufficient permissions", and the line naming the three permissions a
   note needs.

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

- The person's `orient` prints "degraded fields: unknown: the per-service
  schema could not be read: forbidden (HTTP 403)". The read-only role that
  `operate/access.md` recipes holds `query` and `investigation_read` only,
  so the schema read is refused. Orient says so and goes on.
- The first search printed "hint: the catalog has no observation of field
  "status" in service "nginx"". The probe ran seconds after ingest, most
  likely before the catalog had observed the new fields; the search itself
  matched all 10 rows.
- The #308 and #309 recipes build with `cargo build -p trawl-server --bin
  trawld -p trawl-cli --bin trawl -p fleet-admin`. With `--bin` flags, cargo
  builds only the named binaries, so `fleet-admin` is not rebuilt. A stale
  `fleet-admin` migrates the Fleet database only as far as the migrations it
  embeds, and the new trawld refuses to start with `PendingMigration`. The recipe below builds
  `fleet-admin` in its own command.

## Privacy

The probe files hold no credential. The three keys exist only under the run
directory, in the private HOME and `private/`, which `stack.sh down` deletes.
`orient` prints the key's name and its 8-character lookup prefix, as it does
by design; the prefix is not the key, and the key expired with the database
it lived in. The committed files were scanned with `grep` for `Bearer `,
`trawl_` and `flt_` followed by a key character, `Cookie` and `cookie`, and
for the full agent, human and sender key values read from the run directory
before `stack.sh down`. The only match is the `curl` line in `stack.sh` that
expands `$SENDER_KEY` at run time. The phone captures show only the wire
fixtures' invented names and data.

When `transcripts.sh` runs, its raw stream files keep Claude Code's
`system/init` event as emitted, so they name local paths of the recording
machine, as the #308 transcripts do.

## Recipe

From a checkout with Docker available and Claude Code signed in:

```bash
cargo build -p trawl-server --bin trawld -p trawl-cli --bin trawl && cargo build -p fleet-admin
docs/evidence/issue-310/stack.sh up      # postgres:18 on 127.0.0.1:5480, trawld on 127.0.0.1:18450, the two roles and keys, a private HOME
docs/evidence/issue-310/stack.sh seed    # the 50 events above; writes seed.ndjson to the run directory
docs/evidence/issue-310/transcripts.sh /tmp/out   # within 30 minutes of the seed; agent then human
docs/evidence/issue-310/render.py /tmp/out/agent.stream.jsonl /tmp/out/agent.md
docs/evidence/issue-310/render.py /tmp/out/human.stream.jsonl /tmp/out/human.md
python3 -I docs/evidence/issue-310/probe.py target/debug/trawl target/evidence-310/home /tmp/out/probe.jsonl
docs/evidence/issue-310/stack.sh down
```

The run directory is `$TRAWL_EVIDENCE_DIR`, by default
`target/evidence-310`. `stack.sh` is #309's with its own container,
`trawl-310-pg-e`, its own ports, this seed, and the two roles of
`operate/access.md` ("Create a key for an agent") minted verbatim:
`trawl-agent` (`query`, `schema_read`, `validate`, `investigation_read`,
`investigation_write`, 60 requests a minute) for a service key with a 90-day
expiry, and `trawl-investigation-reader` (`query`, `investigation_read`) for
a human key. `mcp-tee.sh` is #308's with the profile taken from
`TRAWL_EVIDENCE_PROFILE`. `transcripts.sh` runs the agent session, takes the
investigation id it created from the stream, and then runs the person's
session.

The phone captures need a release build of the SPA:

```bash
(cd crates/trawl-web-ui && trunk build --release --dist "$PWD/../../target/spa-evidence")
(cd crates/trawl-web-ui/e2e && npm ci && E2E_PORT=8613 TRAWL_E2E_DIST="$PWD/../../../target/spa-evidence" \
  TRAWL_E2E_EVIDENCE=1 npx playwright test tests/investigations.spec.ts)
```
