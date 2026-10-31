# Issue #308 evidence: the MCP server under Claude Code

Recorded on 2026-10-24 against a disposable stack built from this branch at
`935b1244` (the commit that completed the four tools), with Claude Code
2.1.310 as the client. Nothing here was edited by hand after recording.

The transcripts and frames were committed first as Claude Code wrote them.
On 2026-10-30 they were rebuilt from those recordings by
`docs/evidence/issue-310/sanitize.py` and `frames.py`, which drop the
recording machine's session metadata (see Privacy). `render.py` run over
the sanitized transcripts writes the two `transcript-*.md` files byte for
byte as they were rendered from the raw streams.

## What is here

| File | What it is | Criterion |
| --- | --- | --- |
| `transcript-current.jsonl` | `sanitize.py` of the Claude Code `--output-format stream-json` stream of one session with the default (2026-07-28) negotiation: 8 tool calls, their 8 results, 2 text events, the `init` and `result` summaries | AC3 |
| `transcript-legacy.jsonl` | The same session shape with `MCP_PROTOCOL_NEGOTIATION=legacy` (2025-11-25 `initialize`), sanitized the same way | AC3 |
| `transcript-*.md` | The two transcripts rendered by `render.py`: every tool call, its result, the final answer | AC3 |
| `frames-current.jsonl`, `frames-legacy.jsonl` | Every client-to-server JSON-RPC frame of each session, copied by `mcp-tee.sh` before it reached `trawl mcp`, with each `_meta` reduced to its MCP-standard keys by `frames.py`; the first frame names the era | AC3 |
| `transcript-spill-probe.jsonl` | `sanitize.py` of one session calling `search` with `rows: 200, max_bytes: 32768` to see what Claude Code does with a result at the cap | AC14 |
| `search-url.json` | The search URL the current-era session's `search` call emitted, with the executed query and the captured bounds | AC11 |
| `search-url-phone.png` | That URL opened on an iPhone 13 viewport by `crates/trawl-web-ui/e2e/tests/mcp-search-url.spec.ts` | AC11 |
| `stack.sh`, `transcripts.sh`, `mcp-tee.sh`, `render.py` | The recipe, below; `transcripts.sh` runs issue-310's `sanitize.py` and `frames.py` | |

## What the transcripts show

Both sessions list the four tools in the order orient, search, read, fields
and call every one of them: `orient`; `fields` with `source: "service"`;
`search` with `mode: "validate"` and then `mode: "execute"`; `read` with
`action: "page"` and `action: "cell"` on the captured result; `fields` with
`source: "result"`. No call returned `isError`. The current-era session
opens with `server/discover` carrying `io.modelcontextprotocol/protocolVersion`
`2026-07-28`; the legacy session opens with `initialize` at `2025-11-25`
followed by `notifications/initialized`. Both end with the client closing
the pipe and `trawl mcp` exiting 0.

The lag hint fired in the current-era session, whose query was a plain
search over events (the seed sends events hours after their `_time`); the
legacy session chose a `stats` pipeline, which carries no `_ingested`, so it
raised no hint. The search URL in `search-url.json` came from the
current-era session.

## Privacy

The raw streams held the recording machine's `SessionStart` hook output, its
local paths, its plugin and model inventory, session, message and tool-use
ids, token usage and cost records, and the model's thinking blocks with
their signatures. `sanitize.py` keeps the tool calls, their results, the
model's text, the Claude Code version and model from `system/init`, and the
outcome, turn count and duration from `result`; the call number replaces
each tool-use id. The raw frames carried Claude Code's tool-use id and a
`progressToken` in each `tools/call` frame's `_meta`; `frames.py` keeps only
the `io.modelcontextprotocol/` keys. The key lives only in the private HOME
the recipe builds, and `python3 -I docs/evidence/issue-310/check.py
docs/evidence/issue-308` reports `check: 13 files, 0 hits`.

## The structuredContent probe (AC14)

Claude Code 2.1.310 hands the model the `structuredContent` object as a
plain JSON string in place of the text block: every `tool_result` in the
transcripts is a string holding the mirror. The text block is not shown to
the model by this client, so the mirror carries the whole answer and the
budget counts both. At the cap (`transcript-spill-probe.jsonl`, 200 rows asked,
`max_bytes: 32768`), the result reached the model inline as 16,528
characters; Claude Code's spill-to-file threshold is 50,000 characters, so
no result this server can emit is spilled.

## Recipe

From a checkout with `cargo build -p trawl-server --bin trawld -p trawl-cli
--bin trawl -p fleet-admin` done, Docker available, and Claude Code signed in:

```bash
docs/evidence/issue-308/stack.sh up      # postgres:18 on 127.0.0.1:5454, trawld on 127.0.0.1:18443, an agent key, a private HOME
docs/evidence/issue-308/stack.sh seed    # 600 events: shop-api and auth, web-01 and web-02, prod and lab
docs/evidence/issue-308/transcripts.sh /tmp/out   # the two sessions; raw and sanitized streams and frames land in /tmp/out
docs/evidence/issue-308/render.py /tmp/out/current.jsonl /tmp/out/current.md
docs/evidence/issue-308/stack.sh down
```

`stack.sh` keeps the key and the certificate under the run directory, in
the CLI config of a private HOME that only the `trawl mcp` child sees
through the MCP server's `env`. The agent key has the `query`,
`schema_read` and `validate` permissions and nothing else. The phone
capture is `cargo xtask e2e --grep "MCP search URL"`; the spec reads
`search-url.json`.
