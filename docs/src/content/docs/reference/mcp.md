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

The server lists seven tools, always in this order: `orient`, `search`,
`read`, `context`, `sightings`, `compare`, `fields`. Every input is a flat
object. An enum field (`mode`, `action`, or `source`) picks the variant, and a
property that only one variant reads is optional. No input schema has a root `anyOf` or `oneOf`, and every
property name stays within `[A-Za-z0-9_.-]`. Every tool has an `outputSchema`
for its success result.

| Tool | `readOnlyHint` | `destructiveHint` | `idempotentHint` | `openWorldHint` |
|------|----------------|-------------------|------------------|-----------------|
| `orient` | `true` | `false` | `true` | `true` |
| `search` | `false` | `false` | `false` | `true` |
| `read` | `true` | `false` | `true` | `true` |
| `context` | `false` | `false` | `false` | `true` |
| `sightings` | `false` | `false` | `false` | `true` |
| `compare` | `false` | `false` | `false` | `true` |
| `fields` | `true` | `false` | `true` | `true` |

`search` is not read-only because `mode=execute` records a new
[captured result](/reference/api/#captured-results) on each call. `context`,
`sightings`, and `compare` record one on every call, for the same reason. A call with
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

A page of a result that `context`, `sightings`, or `compare` captured prints
that tool's [block](#correlation-tools) on every page, between the truncation
facts and the next calls. The denominators and the warnings therefore travel
with every page.

A result past its deadline answers trawld's `result_gone`, and an unknown id
answers `not_found`. Both are refusals, never an empty page.

### Correlation tools

`context`, `sightings`, and `compare` each send one correlation read to
trawld, which builds the DSL and keeps the answer as a new captured result.
Each tool reads trawld's capabilities first, then reads the first page of the
new result, at most 50 rows and never more than trawld's
`bounds.page_max_rows`, and answers in the
[layout of a result](#layout-of-a-search-or-read-result). Between the
truncation facts and the next calls, each prints its block: what the read
established. The block is never dropped to fit the budget. To read more rows,
call `read` with the result id. A reply that the budget cut proposes the call
that continues it, as [Limits](#limits) describes. Each tool needs `query`.

A refused read is an `isError` result with trawld's code, as described in
[Refused calls](#refused-calls). The tools add these 400 codes:

| Code | Meaning |
|------|---------|
| `focus_incomplete` | The focus row has no `_time`, or no value for a key that the scope names |
| `focus_ambiguous` | The focus row has two columns of the same name for a focus key |
| `windows_overlap` | The compare windows overlap |
| `correlation_invalid` | A compare input cannot be built into a query: a filter that does not parse or has a pipe stage or a time clause, or a group field that the statement assigns, such as `compare_status` |

A value over its cap, or a name that the read refuses, such as a reserved
`fields` name, is `bad_request`. A read that finds more than 5000 groups is
`result_too_large`, and the message says what to narrow. The
[HTTP API](/reference/api/#correlation-reads) lists every case.

### context

| Input | Type | Description |
|-------|------|-------------|
| `result_id` | string | Required. The captured result that holds the focus row |
| `row` | integer | Required. The focus row, counted from 0 |
| `scope` | `env_and_host`, `env`, or `host` | The focus row's keys that hold the read. Default `env_and_host` |
| `seconds` | integer, at least 1 | Seconds on each side of the focus row's `_time`. Default 30 |
| `before` | integer, 0 to 50 | Rows strictly earlier than the focus row. Default 10 |
| `after` | integer, 0 to 50 | Rows strictly later than the focus row. Default 10 |
| `max_bytes` | integer, 4096 to 32768 | The response budget. Default 16384 |

`context` reads the events around one captured row, with the `env` and `host`
values of that row. It never widens the read: a focus row without `_time`, or
without a key that the scope names, is refused with `focus_incomplete`.

The block holds these lines:

- `scope used:` with the `env` and `host` that held the read, or `any`, and the
  seconds on each side.
- `focus:` with the result id, the row, its `_time`, `env`, and `host`.
- `earlier:`, `same _time:`, and `later:`, each with its range of captured rows
  and its row count. At most 20 rows share the focus row's `_time`.
- The sentence `this is a new read; equal timestamps establish no order`.

The rows are a new read, so the focus event is one of the rows at its `_time`
and is not marked. Two events with equal cells print as two rows.

### sightings

| Input | Type | Description |
|-------|------|-------------|
| `literal` | string, 1 to 1024 bytes | Required. The text to find, such as an IP address |
| `earliest` | RFC 3339 instant | The start of the bounds, inclusive |
| `latest` | RFC 3339 instant | The end of the bounds, exclusive. Default now |
| `last` | DSL duration | A duration that ends now, such as `24h`, instead of `earliest` and `latest` |
| `fields` | list of up to 16 field names | Restricts which fields the literal is attributed to. Not `_raw`, `sighting_count`, `sighting_first`, or `sighting_last`, in any letter case |
| `max_bytes` | integer, 4096 to 32768 | The response budget. Default 16384 |

`sightings` finds where one literal occurs. trawld runs the bare-term text
search once, over `message` and `_raw`. The bounds default to the last 24
hours and span at most 7 days. trawld counts every match by `env` and
`service`, with the first and last `_time` seen, and captures the 5000 most
recent matches.

The block holds these lines:

- `sightings of` with the literal, then the denominator sentence:
  `field counts are over the N captured matches; service counts are over all
  M`.
- `attribution restricted to fields` with the names, when the call gave
  `fields`. The capture stores the list, so every later `read` page of the
  result prints the same line; a requested field with no match still appears
  in it. Without the line, every field was attributed.
- The groups, one JSON array per line: env, service, matches, first seen, last
  seen, most matches first.
- `matches per service:` and `matches per env:`, which the server sums from the
  groups.
- `fields holding the literal`, with the captured matches for each field.
  trawld lists at most 256; `and N more fields held the literal; pass fields
  to attribute them` counts the rest.
- `_raw only:`, the captured matches in which no field but `_raw` holds the
  literal, counted over every field whatever `fields` says. `_raw` can match
  field names and syntax, so these are counted apart.

`fields` changes only the list of fields holding the literal. The matches, the
totals, the captured matches, and the `_raw only:` count stay the same with or
without it. The next calls propose trawld's follow-up queries as `search` calls
over the same bounds: the literal on its busiest service, and its volume by
service over time.

A field holds the literal when its text contains it in any letter case. A
number or a boolean is read as its text: `true`, `false`, the digits of an
integer, and the shortest decimal form of a float, such as `3` for 3.0. So the
literal `3.0`, copied from the JSON of a float cell, does not match that cell.
Each captured match carries the `sighting_count`, `sighting_first`, and
`sighting_last` of its group, in place of any event field of those names.

### compare

| Input | Type | Description |
|-------|------|-------------|
| `filter` | string | Required. DSL search terms, such as `service=auth "failed password"`. No pipe stage, and no time clause at any depth, under `NOT` or in parentheses included |
| `foreground_earliest` | RFC 3339 instant | Required. The start of the window under question, inclusive |
| `foreground_latest` | RFC 3339 instant | Required. Its end, exclusive |
| `baseline_earliest` | RFC 3339 instant | The start of the baseline window, inclusive |
| `baseline_latest` | RFC 3339 instant | Its end, exclusive |
| `group_by` | list of up to 2 field names | The fields to group the counts by. Not a column that the statement assigns: those all start with `compare_`, such as `compare_status`. A user field such as `status` groups |
| `min_count` | integer, at least 1 | A group is listed when either window counts at least this many events for it. Default 1 |
| `max_bytes` | integer, 4096 to 32768 | The response budget. Default 16384 |

`compare` counts one filter in two windows. Without a baseline bound, the
baseline is the window of equal duration just before the foreground. The
windows can touch but not overlap. One statement scans both windows, so both
counts come from the same data. Each row of the result is one group, with both
counts, both rates, and the absolute and relative change where they are
defined. A group with no events in the baseline is `new_in_foreground`, never
an infinite change.

The block holds these lines:

- `foreground:` and `baseline:`, each with its start, end, and duration.
- `grouped by:` with the group fields.
- The number of groups listed, the minimum count, and the ranking: absolute
  rate change descending, then count, then group key.
- The rate unit: events per minute of each window.
- One `warning` line for each warning, with trawld's text. A warning says that
  a window starts before the oldest data that retention kept, or that a group
  field has a degraded pin. With no warnings, the line `warnings: none raised`.

trawld raises at most one retention warning per window, which names up to
eight environments and counts the rest, and at most one degraded-pin warning
per group field. So a compare has at most four warnings.

The server prints every warning in full and never drops one to fit the budget.
A warning means the counts can be incomplete. No warning is a claim that they
are complete.

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
4. One `facet` line per facet, then one `hint:` line per hint. A result of
   `context`, `sightings`, or `compare` prints its block here instead.
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
and, when a cell was abbreviated, the call that reads the first such cell.
After `sightings`, its follow-up queries come after the next page. After
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
most rows that fit. A correlation block is never dropped. Only when a
`sightings` reply still does not fit does the server leave entries off the
ends of its groups, totals, and fields lists, and a line names how many. The
denominator sentence stays. A page cut this way always proposes the rest: the
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
- investigations;
- OAuth.

The reasons and the planned order are in
[ADR-0058](https://github.com/jakub/trawl/blob/main/docs/adr/0058-the-mcp-server-is-a-thin-client-that-answers-in-text-first.md).
The captured results it reads are
[ADR-0057](https://github.com/jakub/trawl/blob/main/docs/adr/0057-a-capture-is-an-immutable-expiring-result-that-later-reads-page.md).
