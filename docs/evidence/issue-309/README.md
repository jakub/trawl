# Issue #309 evidence: context, sightings and compare over MCP

Recorded on 2026-10-25 (UTC) against a disposable stack built from this
branch at `fc5bbd3c` (the commit that completed the three reads). Nothing
here was edited by hand after recording.

**The Claude Code transcript for AC10 is not recorded yet.** The seat that
built this directory runs under a hook that refuses any `claude` command,
`claude --version` included: "flow seat (no model through the shell)". So
`transcripts.sh` has not run, and `transcript-current.jsonl`,
`transcript-current.md` and `frames-current.jsonl` do not exist. What is here
is the recipe, ready to run, and `probe-direct.*`: the same tool calls sent
to `trawl mcp` over stdio by a script, with no model in the loop. The probe
shows what the tools answer on this seed. It does not show what Claude Code
does with those answers. The Claude Code version on the recording machine is
2.1.310, read from the install path because the hook also refused
`claude --version`.

## What is here

| File | What it is | Criterion |
| --- | --- | --- |
| `probe-direct.jsonl` | Every JSON-RPC request `probe.py` sent to `trawl mcp` and every response, in order | AC10 (tool half), AC1, AC2, AC3, AC7 |
| `probe-direct.txt` | `probe.py`'s stdout: each tool call and the text block it returned | AC10 (tool half), AC1, AC2, AC3, AC7 |
| `seed.ndjson` | The 73 events `stack.sh seed` sent to `/api/v1/ingest` | |
| `focus.json` | The focus row `seed.py` wrote for the transcript prompt | |
| `stack.sh`, `seed.py`, `transcripts.sh`, `mcp-tee.sh`, `render.py`, `probe.py` | The recipe, below | |
| `transcript-current.jsonl`, `transcript-current.md`, `frames-current.jsonl` | Not recorded: `transcripts.sh` output, pending a run outside a flow seat | AC10 |

## The seed

`seed.py` counts back from the second it runs, here `2026-10-25T06:23:52Z`.
Every event has `env=prod` and its own `_raw` line, which differs from its
fields. Each scenario sits on its own hosts or level, so no read sees the
rows of another.

| Scenario | Rows sent | Expected answer |
| --- | --- | --- |
| sightings, `level=info`, 110 to 150 min back | `nginx` on `web-02` with `client_ip=203.0.113.77`: 4. `sshd` on `web-01` with `src=203.0.113.77`: 3. `sshd` on `web-01` with the IP only in `_raw`: 1. `nginx` on `web-02` with `client_ip=198.51.100.9`: 1 | 8 matches. `nginx` 4, `sshd` 4. `prod` 8. Fields: `client_ip` 4, `src` 3. `_raw` only: 1. Both denominators 8. With `fields: ["client_ip"]`: the same 8 matches and groups, fields `client_ip` 4 only, `_raw` only still 1 |
| context, `level=info`, `nginx` on `web-02`, focus at `05:43:52Z` | 2 rows at `05:43:51Z`, the focus (`POST /checkout 502 order=7731`) and 2 more at `05:43:52Z`, 2 rows at `05:43:53Z`. No other `web-02` row within 30 s | Earlier 2, same `_time` 3, later 2, on `env=prod`, `host=web-02`, 30 s either side, with the no-order statement |
| compare, `level=error`, baseline 75 to 105 min back, foreground 2 to 45 min back | `sshd` `auth_failure` on `web-01`: 3 then 30. `nginx` `status=500` on `web-03`: 10 then 10. `cron` on `web-01`: 4 then 0 | Grouped by `service`: `sshd` 3 to 30 (rose), `cron` 4 to 0 (vanished), `nginx` 10 to 10 (flat), in that rank order |

The compare rows fit the windows only if the session asks within 15 minutes
of the seed: the foreground is the hour before the `now` that `orient`
reports, and the baseline is the hour before that.

## What the probe shows

`probe.py` ran 15 seconds after the seed, through `initialize` at
`2025-11-25`, and called seven tools. No call returned `isError`, and
`trawl mcp` exited 0. Every answer matched the seed.

1. `orient` listed four groups over the last 24 hours: `sshd`/`web-01` 37,
   `nginx`/`web-03` 20, `nginx`/`web-02` 12, `cron`/`web-01` 4. These are the
   seeded counts per host.
2. `sightings` with literal `203.0.113.77`, `last: "24h"`: 8 matches, groups
   `nginx` 4 and `sshd` 4, `prod` 8, fields `client_ip` 4 and `src` 3,
   `_raw` only 1, and the sentence "field counts are over the 8 captured
   matches; service counts are over all 8". Result
   `9479302d86fce9e40a1281d83fa2ad9d`. **Match.**
3. `sightings` with the same literal and `fields: ["client_ip"]`: the same 8
   matches and groups, the line "attribution restricted to fields
   "client_ip"; the matches counted are the same", fields `client_ip` 4,
   `_raw` only 1. Result `f23b9ee923b8f0a9af4a1eaf576a3cc0`. **Match.**
4. `search` with mode `execute` for `service=nginx host=web-02 "order=7731"
   last=2h`: 1 row, the focus row. Result `dc70070603b39b6c12cef905f9338a8c`.
5. `context` on that result, row 0: scope `env "prod", host "web-02"`, 30 s
   either side. Earlier: rows 0 to 1, 2 rows. Same `_time`: rows 2 to 4, 3
   rows. Later: rows 5 to 6, 2 rows. It printed "this is a new read; equal
   timestamps establish no order". The three rows at `05:43:52Z` print as
   three distinct rows. Result `fa26add65283ff3f07293904e5f6cdfa`. **Match.**
6. `compare` with filter `level=error`, foreground `05:24:07Z` to
   `06:24:07Z`, no baseline bounds, `group_by: ["service"]`: the baseline
   defaulted to `04:24:07Z` to `05:24:07Z`. Rows in rank order: `sshd` 30 and
   3 (rate change +0.45 per minute, relative 9.0, `both`); `cron` 0 and 4
   (-0.067 per minute, relative -1.0, `vanished`); `nginx` 10 and 10 (0.0,
   `both`). No warnings raised. Result `4e76baa924fc80b81f9c8527da0f2014`.
   **Match.**
7. `read` with action `page`, offset 0, rows 20, on result
   `9479302d86fce9e40a1281d83fa2ad9d` from step 2, inside its 15-minute TTL:
   rows 0 to 7 of 8, with the sightings block printed again on the page.
   **Pageable.**

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

When `transcripts.sh` runs, its raw `transcript-current.jsonl` keeps Claude
Code's `system/init` event as emitted, so it names local paths of the
recording machine, as the #308 transcripts do.

## Recipe

From a checkout with `cargo build -p trawl-server --bin trawld -p trawl-cli
--bin trawl -p fleet-admin` done, Docker available, and Claude Code signed in:

```bash
docs/evidence/issue-309/stack.sh up      # postgres:18 on 127.0.0.1:5455, trawld on 127.0.0.1:18444, an agent key, a private HOME
docs/evidence/issue-309/stack.sh seed    # the 73 events above; writes seed.ndjson and focus.json to the run directory
docs/evidence/issue-309/transcripts.sh /tmp/out   # within 15 minutes of the seed; stream-json and frames land in /tmp/out
docs/evidence/issue-309/render.py /tmp/out/current.stream.jsonl /tmp/out/current.md
python3 -I docs/evidence/issue-309/probe.py target/debug/trawl target/evidence-309/home target/evidence-309/focus.json /tmp/out/probe.jsonl
docs/evidence/issue-309/stack.sh down
```

The run directory is `$TRAWL_EVIDENCE_DIR`, by default
`target/evidence-309`. `stack.sh` is #308's with its own container,
`trawl-309-pg-e`, its own ports and this seed. The agent key has the
`query`, `schema_read` and `validate` permissions and nothing else.
`transcripts.sh` runs one session with the default negotiation; #308's
legacy-negotiation session is not repeated, since #308 already records both
eras.
