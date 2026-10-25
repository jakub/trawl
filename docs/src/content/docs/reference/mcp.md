---
title: MCP server
description: The tools, resources, text contract, limits, refusals, and command line of the trawl MCP server.
---

The MCP server lets an MCP client, such as Claude Code or Claude Desktop, ask
questions of a Trawl server. It is a client of trawld: it holds one API key,
sends every request over the [HTTP API](/reference/api/), and so is subject to
every permission, quota, and rate limit that applies to that key. It contains
no model and builds no SQL. To set it up, see
[Ask Trawl from Claude](/use/agents/).

The 1.0 server speaks over the stdio transport only. It negotiates the
2025-11-25 protocol revision by `initialize` and the 2026-07-28 revision by
`server/discover`.

## Command

```text
trawl mcp --profile NAME [--token-file PATH]
trawl -p NAME mcp [--token-file PATH]
```

`trawl mcp` reads MCP messages on stdin and writes them on stdout. Stdout
carries protocol frames only. Logs go to stderr after a successful start, at
the level `RUST_LOG` names (default `warn`). No terminal UI starts.

The command takes its target, key, and trust only from the named profile and
`--token-file`. The profile binds the server URL and the TLS trust. The key
comes from the profile's own `token`, or from the file that `--token-file`
names, with surrounding whitespace removed. `[server].token` is never read. See
[client configuration](/reference/configuration/#client-configuration) for the
profile keys, and [the `trial` profile](/reference/cli/#the-reserved--p-trial-profile)
for the reserved profile.

| Flag | Value | Description |
|------|-------|-------------|
| `-p, --profile` | `<NAME>` | The profile to use. Required. This is the global option |
| `--token-file` | `<PATH>` | Read the API key from this file, for a profile that holds no `token` |

The command does not contact the server at startup. The first tool call does,
so a server that is down gives that call a refusal, not the process an exit.
The display zone of `orient` is the `ui.timezone` setting of the CLI config on
the machine that runs `trawl mcp`: `local` (the default), `UTC`, or a fixed
offset such as `+05:30`.

### Exit codes

| Code | Meaning |
|------|---------|
| `0` | The client closed the session after it started |
| `1` | The session did not start, or the transport failed. This includes a client that closes stdin before `initialize` |
| `2` | `trawl mcp` refused the command line or the profile, and printed one `trawl: ...` line to stderr |

A profile with insecure trust also prints the usual insecure-connection
warning on stderr before the server starts.

### Refusals

`trawl mcp` exits `2` for each of these cases. No message contains a value of
a variable, a flag, or a key.

- Any of `TRAWL_URL`, `TRAWL_TOKEN`, `TRAWL_PROFILE`, or `TRAWL_INSECURE` is
  set, even empty. The message names every variable that is present.
- `--url`, `--token`, or `--insecure` is given.
- No profile is named.
- Two key sources are given: a profile that holds a `token` together with
  `--token-file`, or `-p trial` with `--token-file`, because the trial profile
  always carries its operator key.
- No key source is given. The message names `[profiles.NAME]` and
  `--token-file`.
- The profile does not resolve: the config file is missing or invalid, the
  profile does not exist, its URL is not valid, no trial exists for `-p trial`,
  or its `ca_cert` file holds no certificate.
- `ui.timezone` is not `local`, `UTC`, or a fixed offset.

## Tools

The server lists four tools, always in this order: `orient`, `search`,
`read`, `fields`. Every input is a flat object. An enum field (`mode`,
`action`, or `source`) picks the variant, and a property that only one variant
reads is optional. No input schema has a root `anyOf` or `oneOf`, and every
property name stays within `[A-Za-z0-9_.-]`. Every tool has an `outputSchema`
for its success result.

| Tool | `readOnlyHint` | `destructiveHint` | `idempotentHint` | `openWorldHint` |
|------|----------------|-------------------|------------------|-----------------|
| `orient` | `true` | `false` | `true` | `true` |
| `search` | `false` | `false` | `false` | `true` |
| `read` | `true` | `false` | `true` | `true` |
| `fields` | `true` | `false` | `true` | `true` |

`search` is not read-only because `mode=execute` records a new
[captured result](/reference/api/#captured-results) on each call. A call with
arguments that do not decode into the tool's input is an `isError` result that
names the problem. A call to a tool name that is not listed is a protocol error.

`tools/list` and `resources/list` carry a private cache scope and a time to
live of 60 seconds.

### orient

`orient` takes no arguments. It describes the installation in one call:

- the trawld version, the time now in UTC and in the display zone, the key's
  name, prefix, and permissions, and the health status with its problems;
- the inventory: every environment, service, and host that sent events in the
  last 24 hours, with the event count and the newest `_time`, busiest first;
- the retention reach of each environment;
- the degraded fields, the limits that apply, and the installed DSL stages and
  functions;
- the `trawl://dsl/` resources and a short block of DSL essentials, for a client
  that reads no resources;
- at most two proposed next calls.

`orient` reads `whoami`, `capabilities`, `health`, and the schema services.
Two parts depend on the key. The inventory runs one query over `last=24h`
grouped by environment, service, and host, so it needs `query`, and `orient`
asks for at most 200 groups. Without `query`, or when trawld refuses the
query, one line says why and the rest of the answer stands. Retention reach
comes from the dashboard, so it needs `server_manage`. Without it the line
reads `retention reach: not readable with this key`.

When `whoami` or `capabilities` is refused, `orient` is refused with trawld's
code.

### search

| Input | Type | Description |
|-------|------|-------------|
| `mode` | `validate` or `execute` | Required. `validate` checks the query without running it. `execute` runs it and keeps the rows as a captured result |
| `query` | string | Required. The DSL query. Put the time bounds in the text, such as `last=1h` |
| `rows` | integer, 1 to 200 | For `execute`. Rows to print. Default 50 |
| `max_bytes` | integer, 4096 to 32768 | For `execute`. The response budget. Default 16384 |

`validate` calls [`POST /api/v1/validate`](/reference/api/) and needs the
`validate` permission. A query that does not validate is a successful call:
the outcome line says `The query did not validate: N errors.`, and each error
follows with a caret excerpt. A valid query answers `The query is valid.` with
the formatted query and one proposed call to run it.

`execute` captures the query with [`POST /api/v1/results`](/reference/api/#capture-a-result),
which needs `query`, then reads the first page of the capture. It never runs
the query twice. The answer follows the [text contract](#text-contract) and
carries the result id and the time until it expires. To read more rows, call
`read`.

`execute` computes these [hints](#hints) from the capture: dominance, lag, a
field not in the catalog, and, for a capture with no rows, the nearest bounds.

### read

| Input | Type | Description |
|-------|------|-------------|
| `action` | `page` or `cell` | Required. `page` prints rows. `cell` prints the text of one cell |
| `result_id` | string | Required. The id that `search` returned |
| `offset` | integer | For `page`. The first captured row to print, counted from 0. Default 0 |
| `rows` | integer, 1 to 200 | For `page`. Rows to print. Default 50 |
| `row` | integer | For `cell`. The captured row, counted from 0 |
| `column` | integer | For `cell`. The column position, counted from 0 |
| `byte_offset` | integer | For `cell`. The byte of the cell's text to continue from. Default 0 |
| `max_bytes` | integer, 4096 to 32768 | The response budget. Default 16384 |

`read` runs nothing again. It needs `query`, and a key reads only its own
captures. A page prints no search URL: the reason line says the search that
captured the result printed it. A `cell` call prints the whole cell, or as many
bytes as the budget holds. The reply names the `byte_offset` where the rest
starts, and proposes the call that reads it.

A result past its deadline answers trawld's `result_gone`, and an unknown id
answers `not_found`. Both are refusals, never an empty page.

### fields

| Input | Type | Description |
|-------|------|-------------|
| `source` | `service` or `result` | Required. `service` lists the fields of one service. `result` lists the columns of a captured result |
| `service` | string | With `service`. The service name |
| `result_id` | string | With `result`. The id that `search` returned |

Give `service` only with `source=service`, and `result_id` only with
`source=result`. Any other combination is an `isError` result that says which
input is missing or unexpected.

Each field has a provenance, which says where its facts come from:

| Provenance | Source |
|------------|--------|
| `catalog_observation` | The field catalog: the pinned type, whether the pin is degraded, the rows that carried the field, and when the catalog last saw it |
| `parquet_footer` | The footers of the stored Parquet files: the stored type, the row count, and the null count |
| `cached_sample` | The daemon's cached sample of recent values. The output schema lists it, and no 1.0 tool prints it |
| `capture` | The captured result itself: the type, the row and null counts, and the distinct count and listed values from its column facts |

`source=service` needs `schema_read`. It reads the catalog and the stored
schema as two records, which count different things, and prints a field that
both know twice, catalog first. When one record is refused, the other answers
and the outcome line names what failed. `source=result` reads a zero-row page of
the capture, so it needs `query`, and nothing runs again.

## Resources

The DSL reference is served as MCP resources. The text is the published
[DSL reference](/reference/dsl/), compiled into the binary, so it matches the
`trawl` version and changes only with it. Each resource has the MIME type
`text/markdown`. A resource holds at most 32 KiB of text.

| URI | Content |
|-----|---------|
| `trawl://dsl/overview` | The page text before its first `##` heading |
| `trawl://dsl/<slug>` | One `##` section of the page, such as `trawl://dsl/search-stage` |
| `trawl://dsl/<slug>/<sub-slug>` | One `###` section of a `##` section that is larger than 32 KiB |

A slug is the heading in lowercase with hyphens. `resources/list` returns the
URIs in page order. `orient` lists them in its text. A URI that is not listed is
a resource-not-found error.

## Text contract

Every tool result has one text block that stands alone, and a
`structuredContent` object that mirrors it under the tool's `outputSchema`. A
client that renders only the text loses nothing.

### Layout of a search or read result

The text block holds these parts from top to bottom. A part that is empty is
left out.

1. The outcome line, then `result:` with the result id and the time the result
   is readable until.
2. `bounds:` with the earliest bound (inclusive) and the latest bound
   (exclusive), and `executed query:` with the text trawld ran.
3. Four sentences of truncation facts, described [below](#truncation-facts).
4. One `facet` line per facet, then one `hint:` line per hint.
5. At most two `next:` lines: a tool name and its arguments as a JSON object,
   then the reason.
6. `search URL:` with the link, or `no search URL:` with the reason.
7. One columns line.
8. `rows from captured row N:` and the rows, one JSON array per line.

Bounds and the expiry time are RFC 3339 in UTC with microseconds.

Every value that came from stored events prints as JSON: each cell, each facet
value, each column name, and the executed query. Event text is therefore always
a JSON string inside a JSON array, whatever it holds. The server also escapes
DEL, the C1 control characters, and U+2028 and U+2029 as `\uXXXX`, on top of the
escapes JSON requires. The free text that the server writes, and trawld's
messages, print on one line with line breaks and control characters escaped.

A cell is its wire JSON value, as the [API](/reference/api/) returns it. A
cell whose wire text is longer than 512 bytes prints as a JSON string of its
first 512 bytes, cut on a character boundary. The string ends with a marker
that names the omitted bytes and the call that reads the cell:
`…[+N bytes: read action=cell row=R column=C]`. The mirror lists each
abbreviated cell as its row, column, and total bytes. `read action=cell`
returns the whole value.

### Truncation facts

Four sentences tell the reader what the rows are not. They use fixed wording.

1. `The execution produced N rows.` The rows the query returned.
2. `The capture kept all N rows.` or `The capture kept N of the M rows and
   truncated the result.` The rows the capture stored, which is at most the
   capture bound `max_rows`.
3. `The DSL did not sample, limit or deduplicate the rows.` Or the sentence
   names the stages that did: `sample`, `limit` or `tail`, and `dedup`. The
   server reads these from the executed query's pipe stages.
4. `This response prints N rows from captured row R, abbreviates N cells and
   omits N columns; N captured rows follow.` When the budget dropped anything,
   a last clause lists what: rows, the facets, the hints, the proposed next
   calls.

### Facets

A facet counts the values of one column over the captured rows. A result prints
at most 3 facets of at most 5 values each, most frequent first. Only a column
whose facts list every distinct value is a facet. Time columns, trailing
metadata such as `_raw` and `_ingested`, and columns with a value longer than
512 bytes are not.

### Next actions

A result proposes at most 2 next calls. After a page they are the next page
and, when a cell was abbreviated, the call that reads the first such cell. After
a valid `validate` it is the call that runs the query. Each call's arguments
are valid under that tool's input schema.

### The search URL

The search URL opens [Search](/reference/web-ui/) with the executed query and
its bounds. The server builds it with the same codec as Search, so the page
that opens shows what the tool ran. The `q` parameter carries the executed
text, including the exact `earliest=` and `latest=` of the capture, and `r`
carries those bounds widened to whole seconds. The origin is the trawld setting
[`[server] link_origin`](/reference/configuration/#server). A request never
chooses it.

A result has no search URL, and its `no search URL:` line says why, when:

- trawld has no `link_origin`;
- the result has no bounds, because it was read from `from saved` report rows;
- the bounds do not parse as instants;
- Search would refuse the link, because its query string is over 32 KiB or 64
  parameters;
- the whole link, origin included, is longer than 32 KiB;
- the link did not fit the response budget;
- trawld's capabilities could not be read, so the origin is unknown. The reason
  names the code trawld answered.

## Limits

| Limit | Value |
|-------|-------|
| Response size, `text` plus `structuredContent` | 16384 bytes by default; the call's `max_bytes` sets it, from 4096 to 32768 |
| Rows per response | 50 by default; the call's `rows` sets it, from 1 to 200 |
| Rows per page request to trawld | the call's `rows`, held to trawld's `[capture] page_max_rows` (`bounds.page_max_rows` in capabilities); a created continuation stays inside it |
| Cell bytes per request to trawld | the call's `max_bytes`, held to trawld's `[capture] max_wire_bytes` (`bounds.max_wire_bytes`); the rest follows through continuations |
| Cell | 512 bytes of wire JSON; a longer cell is abbreviated |
| Facets per result | 3, of 5 values each |
| Next actions per reply | 2 |
| Error text | 8192 bytes, cut on a character boundary with a marker |
| DSL resource | 32768 bytes |

The hard budget of 32 KiB stays under the 50,000 characters at which Claude
Code spills a result to a file, even if a client counts the text and the
mirror together. The server counts both against `max_bytes`.

When a reply is over its budget, the server fits it in this order. It drops
whole rows from the end of the page, never cutting a row or a cell, and says
how many. The outcome line of a `read` page names the rows the response
prints, not the rows it read; when it prints none, the line says that the row
at the offset does not fit. If no rows fit, it also drops the facets, then the next calls, then
the search URL (and `no search URL:` says so), then the hints, and prints the
most rows that fit. A page cut this way always proposes the rest: the
continuation call moves to the end of the rows printed, or is created when the
server page was the last one. When not even one row fits, the rows block says
`no row fits max_bytes N; see next actions`, and the proposed call reads that
row alone at the hard cap, or, when no budget prints the row, reads its cells
one at a time. A cell range cut the same way proposes the rest from the byte
where printing stopped; a range with no room for a single byte is an `isError`
result that names the smallest `max_bytes` that prints one.
If the reply does not fit with all of those gone, the call is an `isError`
result that names the result id and `max_bytes`. The result is kept, so a
`read` with a larger `max_bytes` returns it. `orient` and `fields` fit their
replies the same way, dropping inventory rows or fields from the end before
their other parts.

## Refused calls

A call that trawld refuses, or that fails on the way, is an `isError` result
with one text block and no `structuredContent`. The text holds these parts:

```text
search was refused: validation_error (HTTP 400).
trawld says: unknown field in stats
error: unexpected token
  service=nginx | stat count()
                  ^^^^
  hint: did you mean "stats"?
retry after: 3 seconds
request id: 7c1e0f52
```

- The tool, the error code, and the HTTP status.
- `trawld says:` and trawld's message, on one line.
- One `error:` line for each diagnostic, up to 8, then a count of the rest. A
  diagnostic with a span carries a caret excerpt cut from the text that was
  sent, and its hint when trawld gave one. The excerpt keeps 60 bytes of the
  line on each side of the span. A multi-line text names the line number. The
  server drops an excerpt whose span is reversed, past the end of the text, or
  off a character boundary, and keeps the message.
- `retry after: N seconds` when trawld sent a `Retry-After` header, or `retry
  after: trawld named no wait` for a 429 without one.
- `request id:`, which an operator can search for in the trawld logs.

When the text is over 8192 bytes, the head is cut, and the `retry after` and
`request id` lines stay.

A failure in transport, or a response that does not parse, prints a sentence
that names its class only, such as `no connection to trawld opened`. It holds
no library message, address, or path.

A failed validation is not a refusal. `search mode=validate` on a query that
does not parse is a successful call.

## Hints

A hint is computed from facts that trawld answered, never generated. Each hint
names the rows or the range it was computed over. A hint that needs a call to
trawld stays silent when trawld refuses the call, so a hint never turns a
search into an error. `search` prints these four.

| Hint | When it prints | Threshold |
|------|----------------|-----------|
| Dominance | One `service` or one `host` value holds a large share of the captured rows | At least 80% of the captured rows (`DOMINANCE_PERCENT`), among at least 2 distinct values (`DOMINANCE_MIN_DISTINCT`). A column with more than 5 distinct values is not judged |
| Lag | A sender's `_ingested` is far ahead of its `_time` | More than 300 seconds (`LAG_THRESHOLD_SECS`) on a row of the page this response read. The hint names the service and host with the largest gap and counts the other senders over the threshold |
| Not in the catalog | The catalog has no pin for a field that the search stage names, or, when the query names one `service=`, no observation of it in that service | The hint states what the catalog holds now. The catalog evicts pins and observations, so stored events can still carry the field. The check covers at most 4 fields (`MAX_FIELD_CHECKS`), none that begin with `_` and none of the well-known log fields, and reads at most 3 catalog pages of 100 observations (`CATALOG_MAX_PAGES`, `CATALOG_PAGE_ROWS`). A check that does not finish gives no hint. It needs `schema_read` |
| Nearest bounds | The capture has no rows | The query is a plain search stage with no pipe stages. One aggregate widens the bounds by 7 days (`NEAREST_REACH_DAYS`) before the earliest bound and after the latest bound, clipped at now, and reports the last event before the bounds and the first after. Any other shape prints `nearest bounds: not computed for this shape` |

The constants are in `crates/trawl-mcp/src/hints.rs`.

## Out of scope in 1.0

These are not in the 1.0 server:

- the streamable HTTP transport, and so a client that connects from a cloud
  service to a tailnet address;
- the `watch` and `operations` tools;
- the `context`, `sightings`, and `compare` tools;
- investigations;
- OAuth.

The reasons and the planned order are in
[ADR-0058](https://github.com/jakub/trawl/blob/main/docs/adr/0058-the-mcp-server-is-a-thin-client-that-answers-in-text-first.md).
The captured results it reads are
[ADR-0057](https://github.com/jakub/trawl/blob/main/docs/adr/0057-a-capture-is-an-immutable-expiring-result-that-later-reads-page.md).
