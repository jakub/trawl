# Issue #309 evidence: context, sightings and compare over MCP

Recorded on 2026-10-25 (UTC) against a disposable stack built from this
branch at `24dc7291` (the head after the third review round's fixes).
Nothing here was edited by hand after recording.

The transcript and frames were committed first as Claude Code wrote them.
On 2026-10-30 they were rebuilt from those recordings by
`docs/evidence/issue-310/sanitize.py` and `frames.py`, which drop the
recording machine's session metadata (see Privacy). `render.py` run over
the sanitized transcript writes `transcript-current.md` as it was rendered
from the raw stream, except for its title, which names the input file.

Two recordings sit side by side. `probe-direct.*` is the same seven tool
calls sent to `trawl mcp` over stdio by `probe.py`, with no model in the loop:
it shows what the tools answer on this seed. `transcript-current.*` is
`transcripts.sh`'s session: Claude Code 2.1.310 running `claude-opus-5-5`
against the same stack, with the prompt in `transcripts.sh`, which shows what
Claude Code does with those answers. The flow seat that built this directory
runs under a hook that refuses any `claude` command, so the orchestrating
session ran `transcripts.sh` and `probe.py` itself, both inside the seed's
15-minute window.

## What is here

| File | What it is | Criterion |
| --- | --- | --- |
| `probe-direct.jsonl` | Every JSON-RPC request `probe.py` sent to `trawl mcp` and every response, in order | AC10 (tool half), AC1, AC2, AC3, AC7 |
| `probe-direct.txt` | `probe.py`'s stdout: each tool call and the text block it returned | AC10 (tool half), AC1, AC2, AC3, AC7 |
| `seed.ndjson` | The 73 events `stack.sh seed` sent to `/api/v1/ingest` | |
| `focus.json` | The focus row `seed.py` wrote for the transcript prompt | |
| `stack.sh`, `seed.py`, `transcripts.sh`, `mcp-tee.sh`, `render.py`, `probe.py` | The recipe, below | |
| `transcript-current.jsonl` | `sanitize.py` of Claude Code's `stream-json` output for the session: 8 tool calls, their 8 results, 2 text events, the `init` and `result` summaries | AC10 |
| `transcript-current.md` | `render.py`'s reading of it: each tool call with its arguments, the first 600 characters of each result, and the final answer | AC10 |
| `frames-current.jsonl` | The 10 JSON-RPC requests Claude Code wrote to `trawl mcp`'s stdin, through `mcp-tee.sh`, with each `_meta` reduced to its MCP-standard keys by `frames.py`: a `server/discover` probe, `resources/list`, `tools/list` and the seven `tools/call`s | AC10 |

## The seed

`seed.py` counts back from the second it runs, here `2026-10-25T07:55:57Z`.
Every event has `env=prod` and its own `_raw` line, which differs from its
fields. Each scenario sits on its own hosts or level, so no read sees the
rows of another.

| Scenario | Rows sent | Expected answer |
| --- | --- | --- |
| sightings, `level=info`, 110 to 150 min back | `nginx` on `web-02` with `client_ip=203.0.113.77`: 4. `sshd` on `web-01` with `src=203.0.113.77`: 3. `sshd` on `web-01` with the IP only in `_raw`: 1. `nginx` on `web-02` with `client_ip=198.51.100.9`: 1 | 8 matches. `nginx` 4, `sshd` 4. `prod` 8. Fields: `client_ip` 4, `src` 3. `_raw` only: 1. Both denominators 8. With `fields: ["client_ip"]`: the same 8 matches and groups, fields `client_ip` 4 only, `_raw` only still 1 |
| context, `level=info`, `nginx` on `web-02`, focus at `07:15:57Z` | 2 rows at `07:15:56Z`, the focus (`POST /checkout 502 order=7731`) and 2 more at `07:15:57Z`, 2 rows at `07:15:58Z`. No other `web-02` row within 30 s | Earlier 2, same `_time` 3, later 2, on `env=prod`, `host=web-02`, 30 s either side, with the no-order statement |
| compare, `level=error`, baseline 75 to 105 min back, foreground 2 to 45 min back | `sshd` `auth_failure` on `web-01`: 3 then 30. `nginx` `status=500` on `web-03`: 10 then 10. `cron` on `web-01`: 4 then 0 | Grouped by `service`: `sshd` 3 to 30 (rose), `cron` 4 to 0 (vanished), `nginx` 10 to 10 (flat), in that rank order |

The compare rows fit the windows only if the session asks within 15 minutes
of the seed: the foreground is the hour before the `now` that `orient`
reports, and the baseline is the hour before that.

## What the probe shows

`probe.py` ran seconds after the seed, through `initialize` at
`2025-11-25`, and called seven tools. No call returned `isError`, and
`trawl mcp` exited 0. Every answer matched the seed.

1. `orient` listed four groups over the last 24 hours: `sshd`/`web-01` 37,
   `nginx`/`web-03` 20, `nginx`/`web-02` 12, `cron`/`web-01` 4. These are the
   seeded counts per host.
2. `sightings` with literal `203.0.113.77`, `last: "24h"`: 8 matches, groups
   `nginx` 4 and `sshd` 4, `prod` 8, fields `client_ip` 4 and `src` 3,
   `_raw` only 1, and the sentence "field counts are over the 8 captured
   matches; service counts are over all 8". Result
   `920f2976a53082b255af9b1aad10d381`. **Match.**
3. `sightings` with the same literal and `fields: ["client_ip"]`: the same 8
   matches and groups, the line "attribution restricted to fields
   "client_ip"; the matches counted are the same", fields `client_ip` 4,
   `_raw` only 1. Result `83e7ca0d5efd406b3d7168ae175eefd0`. **Match.**
4. `search` with mode `execute` for `service=nginx host=web-02 "order=7731"
   last=2h`: 1 row, the focus row. Result `fc2328422ba8ecff9ff4a804709e54f2`.
5. `context` on that result, row 0: scope `env "prod", host "web-02"`, 30 s
   either side. Earlier: rows 0 to 1, 2 rows. Same `_time`: rows 2 to 4, 3
   rows. Later: rows 5 to 6, 2 rows. It printed "this is a new read; equal
   timestamps establish no order". The three rows at `07:15:57Z` print as
   three distinct rows. Result `6bceb4183072e5c980f4563c1406ecab`. **Match.**
6. `compare` with filter `level=error`, foreground `06:56:03Z` to
   `07:56:03Z`, no baseline bounds, `group_by: ["service"]`: the baseline
   defaulted to `05:56:03Z` to `06:56:03Z`. Rows in rank order: `sshd` 30 and
   3 (rate change +0.45 per minute, relative 9.0, `both`); `cron` 0 and 4
   (-0.067 per minute, relative -1.0, `vanished`); `nginx` 10 and 10 (0.0,
   `both`). No warnings raised. Result `06f62a34f28ce2442b65a5eaabdc46a6`.
   **Match.**
7. `read` with action `page`, offset 0, rows 20, on result
   `920f2976a53082b255af9b1aad10d381` from step 2, inside its 15-minute TTL:
   rows 0 to 7 of 8, with the sightings block printed again on the page.
   **Pageable.**

## What the transcript shows

`transcripts.sh` started about 25 seconds after the seed. The session took 9 turns and
32 seconds, ended with `subtype: success`, and made 8 tool calls: one
`ToolSearch` to load the six `trawl` tools, then the seven calls the prompt
asks for, in the prompt's order. No tool result carried `is_error`. The
session's answers are the probe's.

1. `orient`, then `sightings` with the literal and no window: 8 matches,
   `nginx` 4 and `sshd` 4, fields `client_ip` 4 and `src` 3, `_raw` only 1.
   Result `9defb61cf9ac0c887d816b010a0216ff`.
2. `sightings` with `fields: ["client_ip"]`: the same 8 matches, fields
   `client_ip` 4 only, `_raw` only 1. Result
   `cc8ee94f92736bb93bf3e4283f45a7e2`.
3. `search` with mode `execute` for the focus message, with explicit bounds
   `07:15:00Z` to `07:17:00Z` around the time the prompt gave instead of the
   probe's `last=2h`: 1 row. Result `3f18cc94c33295eb19bb3bfdbfa604eb`.
4. `context` on that result, row 0: earlier 2, same `_time` 3, later 2, scope
   `env "prod", host "web-02"`, 30 s either side, with the no-order
   statement. Result `107a73ccbd40b0b97491736cbe441744`.
5. `compare` with filter `level=error`, foreground `06:56:23Z` to
   `07:56:23Z`, `group_by: ["service"]`: `sshd` 30 and 3, `cron` 0 and 4
   (`vanished`), `nginx` 10 and 10, no warnings. Result
   `89e798ff07d9f707654378f9fa4ed961`.
6. `read` with action `page` on the step 1 result: rows 0 to 7 of 8, with the
   sightings block repeated.

The final answer reports every count above, the rank order rose, vanished
and flat, the five result ids, and the lag hint the search raised, because
the seed back-dates `_time` and the rows were ingested 40 minutes later. It
also remarks that `raw_only_rows` stays 1 under `fields: ["client_ip"]`
while the three `src` rows no longer match a listed field, and asks whether
that is by design. It is: `raw_only_rows` counts the captured matches that
hold the literal in no field at all, and `fields` restricts which fields are
attributed, not which rows match. The `src` rows still hold the literal in a
field, so they are neither attributed nor `_raw` only.

## Privacy

The probe files hold no credential. The agent key and the sender key exist
only under the run directory, in the private HOME and `private/`, which
`stack.sh down` deletes. `orient` prints the key's name and its 8-character
lookup prefix, as it does by design; the prefix is not the key, and the key
expired with the database it lived in. The committed files were scanned with
`grep` for `Bearer `, `trawl_` and `flt_` followed by a key character,
`Cookie` and `cookie`, and for the full agent key and sender key values read
from the run directory. The only match is the `curl` line in `stack.sh` that
expands `$SENDER_KEY` at run time.

The raw stream held the recording machine's `SessionStart` hook output, its
local paths, its plugin and model inventory, session, message and tool-use
ids, token usage and cost records, and the model's thinking blocks with
their signatures. `sanitize.py` keeps the tool calls, their results, the
model's text, the Claude Code version and model from `system/init`, and the
outcome, turn count and duration from `result`; the call number replaces
each tool-use id. The raw frames carried Claude Code's tool-use id and a
`progressToken` in each `tools/call` frame's `_meta`; `frames.py` keeps only
the `io.modelcontextprotocol/` keys. `python3 -I
docs/evidence/issue-310/check.py docs/evidence/issue-309` reports one hit,
on the scan description above.

## Recipe

From a checkout with `cargo build -p trawl-server --bin trawld -p trawl-cli
--bin trawl` and `cargo build -p fleet-admin` done (the `--bin` filters apply
to every `-p`, so `fleet-admin` needs its own build), Docker available, and
Claude Code signed in:

```bash
docs/evidence/issue-309/stack.sh up      # postgres:18 on 127.0.0.1:5455, trawld on 127.0.0.1:18444, an agent key, a private HOME
docs/evidence/issue-309/stack.sh seed    # the 73 events above; writes seed.ndjson and focus.json to the run directory
docs/evidence/issue-309/transcripts.sh /tmp/out   # within 15 minutes of the seed; raw and sanitized streams and frames land in /tmp/out
docs/evidence/issue-309/render.py /tmp/out/current.jsonl /tmp/out/current.md
python3 -I docs/evidence/issue-309/probe.py target/debug/trawl target/evidence-309/home target/evidence-309/focus.json /tmp/out/probe.jsonl
docs/evidence/issue-309/stack.sh down
```

Each step exits nonzero when its run fails: `stack.sh seed` exits nonzero
on an ingest refused with an HTTP error status, `transcripts.sh` returns the
exit status of a failed `claude` session, and `probe.py` exits nonzero on a
tool call that returns `isError`, a JSON-RPC error or a nonzero exit of
`trawl mcp`. These checks were added to the scripts after the recording.
The scripts made the recording unchanged otherwise, and it had none of
these failures.

The run directory is `$TRAWL_EVIDENCE_DIR`, by default
`target/evidence-309`. `stack.sh` is #308's with its own container,
`trawl-309-pg-e`, its own ports and this seed. The agent key has the
`query`, `schema_read` and `validate` permissions and nothing else.
`transcripts.sh` runs one session with the default negotiation; #308's
legacy-negotiation session is not repeated, since #308 already records both
eras.
