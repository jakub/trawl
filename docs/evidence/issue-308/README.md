# Issue #308 evidence: the MCP server under Claude Code

Recorded on 2026-10-24 against a disposable stack built from this branch at
`935b1244` (the commit that completed the four tools), with Claude Code
2.1.310 as the client. Nothing here was edited by hand after recording.

## What is here

| File | What it is | Criterion |
| --- | --- | --- |
| `transcript-current.jsonl` | Claude Code `--output-format stream-json` of one session with the default (2026-07-28) negotiation | AC3 |
| `transcript-legacy.jsonl` | The same session shape with `MCP_PROTOCOL_NEGOTIATION=legacy` (2025-11-25 `initialize`) | AC3 |
| `transcript-*.md` | The two transcripts rendered by `render.py`: every tool call, its result, the final answer | AC3 |
| `frames-current.jsonl`, `frames-legacy.jsonl` | Every client-to-server JSON-RPC frame of each session, copied by `mcp-tee.sh` before it reached `trawl mcp`; the first frame names the era | AC3 |
| `spill-probe.jsonl` | One session calling `search` with `rows: 200, max_bytes: 32768` to see what Claude Code does with a result at the cap | AC14 |
| `search-url.json` | The search URL the current-era session's `search` call emitted, with the executed query and the captured bounds | AC11 |
| `search-url-phone.png` | That URL opened on an iPhone 13 viewport by `crates/trawl-web-ui/e2e/tests/mcp-search-url.spec.ts` | AC11 |
| `stack.sh`, `transcripts.sh`, `mcp-tee.sh`, `render.py` | The recipe, below | |

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

The raw `transcript-*.jsonl` files keep Claude Code's `system/init` event as
emitted, so they name local paths of the recording machine: the working
directory under `~/.local/state/flow/issue-308/`, the plugin cache under
`~/.claude/plugins/cache/`, and that directory's auto-memory path. They hold
no credential: the key lives only in the private HOME the recipe builds, and
the scan in the review found no bearer value, key pattern or cookie.

## The structuredContent probe (AC14)

Claude Code 2.1.310 hands the model the `structuredContent` object as a
plain JSON string in place of the text block: every `tool_result` in the
transcripts is a string holding the mirror. The text block is not shown to
the model by this client, so the mirror carries the whole answer and the
budget counts both. At the cap (`spill-probe.jsonl`, 200 rows asked,
`max_bytes: 32768`), the result reached the model inline as 16,528
characters; Claude Code's spill-to-file threshold is 50,000 characters, so
no result this server can emit is spilled.

## Recipe

From a checkout with `cargo build -p trawl-server --bin trawld -p trawl-cli
--bin trawl -p fleet-admin` done, Docker available, and Claude Code signed in:

```bash
docs/evidence/issue-308/stack.sh up      # postgres:18 on 127.0.0.1:5454, trawld on 127.0.0.1:18443, an agent key, a private HOME
docs/evidence/issue-308/stack.sh seed    # 600 events: shop-api and auth, web-01 and web-02, prod and lab
docs/evidence/issue-308/transcripts.sh /tmp/out   # the two sessions; frames and stream-json land in /tmp/out
docs/evidence/issue-308/render.py /tmp/out/current.stream.jsonl /tmp/out/current.md
docs/evidence/issue-308/stack.sh down
```

`stack.sh` keeps the key and the certificate under the run directory, in
the CLI config of a private HOME that only the `trawl mcp` child sees
through the MCP server's `env`. The agent key has the `query`,
`schema_read` and `validate` permissions and nothing else. The phone
capture is `cargo xtask e2e --grep "MCP search URL"`; the spec reads
`search-url.json`.
