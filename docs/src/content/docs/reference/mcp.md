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

The server lists five tools, always in this order: `orient`, `search`,
`read`, `fields`, `investigation`. Every input is a flat object. An enum field
(`mode`, `action`, or `source`) picks the variant, and a property that only one
variant reads is optional. No input schema has a root `anyOf` or `oneOf`, and every
property name stays within `[A-Za-z0-9_.-]`. Every tool has an `outputSchema`
for its success result.

| Tool | `readOnlyHint` | `destructiveHint` | `idempotentHint` | `openWorldHint` |
|------|----------------|-------------------|------------------|-----------------|
| `orient` | `true` | `false` | `true` | `true` |
| `search` | `false` | `false` | `false` | `true` |
| `read` | `true` | `false` | `true` | `true` |
| `fields` | `true` | `false` | `true` | `true` |
| `investigation` | `false` | `false` | `false` | `true` |

`search` is not read-only because `mode=execute` records a new
[captured result](/reference/api/#captured-results) on each call.
`investigation` is not read-only or idempotent because its writes append
entries. None of its actions deletes anything. A call with
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
- the open investigations;
- the degraded fields, the limits that apply, and the installed DSL stages and
  functions;
- the `trawl://dsl/` resources and a short block of DSL essentials, for a client
  that reads no resources;
- at most two proposed next calls.

`orient` reads `whoami`, `capabilities`, `health`, and the schema services.
Three parts depend on the key. The inventory runs one query over `last=24h`
grouped by environment, service, and host, so it needs `query`, and `orient`
asks for at most 200 groups. Without `query`, or when trawld refuses the
query, one line says why and the rest of the answer stands. Retention reach
comes from the dashboard, so it needs `server_manage`. Without it the line
reads `retention reach: not readable with this key`. The open investigations
come from the investigation list, so they need `query` and
`investigation_read`. `orient` prints at most 5, most recently changed first,
one line each. A last line says how many more are open, or `none` when no
investigation is open. When trawld refuses the list, one line names its code
and the rest of the answer stands. Without those permissions, `orient` prints
no investigation line.

When `whoami` or `capabilities` is refused, `orient` is refused with trawld's
code.

### search

| Input | Type | Description |
|-------|------|-------------|
| `mode` | `validate` or `execute` | Required. `validate` checks the query without running it. `execute` runs it and keeps the rows as a captured result |
| `query` | string | Required. The DSL query. Put the time bounds in the text, such as `last=1h` |
| `rows` | integer, 1 to 200 | For `execute`. Rows to print. Default 50 |
| `max_bytes` | integer, 4096 to 32768 | For `execute`. The response budget. Default 16384 |
| `investigation` | string | For `execute`. The id of the investigation to attach the search to |
| `purpose` | string, at most 1024 bytes | For `execute` with `investigation`. Why the search runs, recorded on its step |

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

With `investigation`, `execute` attaches the capture to that investigation.
trawld records the search as a step of the investigation and keeps the whole
result as its evidence. The attached search needs `query`,
`investigation_read`, and `investigation_write`. The answer prints an
`investigation:` line with the id and the kept result's id, then the
[activity](#activity) since this session last saw the investigation. The
server checks the id's shape and the length of `purpose` before it sends a
request, and refuses `purpose` without `investigation`. `validate` runs
nothing, so it ignores both inputs and names the ones the call gave: beside
the investigation id when the call names one, else on the outcome line.

A refused attached search is an `isError` result with trawld's code. For
`investigation_closed`, the text names the `status` call that reopens the
investigation. For `evidence_budget_exceeded`, it prints trawld's message with
the bytes used and the limit, and says that evidence frees only when it ages
out or an operator deletes an investigation.

### read

| Input | Type | Description |
|-------|------|-------------|
| `action` | `page`, `cell`, or `investigation` | Required. `page` prints rows. `cell` prints the text of one cell. `investigation` prints an investigation's entries |
| `result_id` | string | For `page` and `cell`. The id that `search` returned |
| `investigation_id` | string | For `investigation`. The investigation's id |
| `since` | integer | For `investigation`. Print the entries changed after this sequence. Default 0, from the start |
| `offset` | integer | For `page`. The first captured row to print, counted from 0. Default 0 |
| `rows` | integer, 1 to 200 | For `page`. Rows to print. Default 50 |
| `row` | integer | For `cell`. The captured row, counted from 0 |
| `column` | integer | For `cell`. The column position, counted from 0 |
| `byte_offset` | integer | For `cell`. The byte of the cell's text to continue from. Default 0 |
| `max_bytes` | integer, 4096 to 32768 | The response budget. Default 16384 |

`read` runs nothing again. It needs `query`. A key reads its own captures,
and, with `investigation_read`, the results that investigations keep as
evidence. A page prints no search URL: the reason line says the search that
captured the result printed it. A `cell` call prints the whole cell, or as many
bytes as the budget holds. The reply names the `byte_offset` where the rest
starts, and proposes the call that reads it.

A result past its deadline answers trawld's `result_gone`, and an unknown id
answers `not_found`. Both are refusals, never an empty page.

`action=investigation` prints the same board as `investigation action=read`,
with the same renderer, and its continuation is a `read` call. It needs
`query` and `investigation_read`.

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

### investigation

An investigation is a shared, append-only log of the work on one question:
the searches that ran for it, the rows kept as evidence, findings, and notes.
Every key with `query` and `investigation_read` reads every investigation, so
an agent and a person work in the same one. The
[investigations API](/reference/api/) stores it. Each entry has a sequence
number, `seq`, and the sequence of its last change, `updated_seq`.

| Input | Type | Description |
|-------|------|-------------|
| `action` | `create`, `read`, `list`, `note`, `finding`, `keep`, `finding_status`, or `status` | Required. The call to make |
| `id` | string, 32 lowercase hex digits | For every action except `list`. The investigation's id. Optional for `create` |
| `title` | string, at most 200 bytes | For `create`. Required |
| `question` | string, at most 2048 bytes | For `create`. Required. The question the investigation answers |
| `scope` | string, at most 2048 bytes | For `create`. What the investigation covers |
| `text` | string, at most 4096 bytes | For `note`. Required |
| `statement` | string, at most 4096 bytes | For `finding`. Required |
| `status` | `hypothesis`, `supported`, `refuted`, `unresolved`, `open`, or `closed` | For `finding` and `finding_status`, one of the first four. For `status`, `open` or `closed`. Required by all three |
| `references` | array of `{result_id, row}`, at most 32 | For `finding`. The rows that support it |
| `counterevidence` | array of `{result_id, row}`, at most 32 | For `finding`. The rows that count against it |
| `remaining_question` | string, at most 1024 bytes | For `finding`. What the evidence leaves open |
| `result_id` | string | For `keep`. Required. The captured result whose rows to keep |
| `rows` | array of integers, at most 200 | For `keep`. Required. The row numbers, counted from 0 |
| `reason` | string, at most 1024 bytes | For `keep`. Required |
| `finding_seq` | integer | For `finding_status`. Required. The `seq` of the finding |
| `expected_seq` | integer | For `finding_status` and `status`. Required. The investigation's `seq` that the call saw last |
| `since` | integer | For `read`. Print the entries changed after this sequence. Default 0 |
| `max_bytes` | integer, 4096 to 32768 | The response budget. Default 16384 |

| Action | trawld route | Permissions |
|--------|--------------|-------------|
| `create` | `PUT /api/v1/investigations/{id}` | `query`, `investigation_read`, `investigation_write` |
| `read` | `GET /api/v1/investigations/{id}?since=` | `query`, `investigation_read` |
| `list` | `GET /api/v1/investigations` | `query`, `investigation_read` |
| `note`, `finding`, `keep` | `POST /api/v1/investigations/{id}/entries` | `query`, `investigation_read`, `investigation_write` |
| `finding_status` | `PUT /api/v1/investigations/{id}/findings/{seq}/status` | `query`, `investigation_read`, `investigation_write` |
| `status` | `PATCH /api/v1/investigations/{id}` | `query`, `investigation_read`, `investigation_write` |

A key that works in investigations through this server therefore holds
`query`, `schema_read`, `validate`, `investigation_read`, and
`investigation_write`.

The server refuses a call before it sends a request when the call gives a
field that its action does not take, lacks a field that the action needs,
gives a malformed `id`, or gives a value past its bound. The text names the
field and lists the fields the action takes.

`create` without an `id` mints one: 16 random bytes as 32 lowercase hex
digits. The model never chooses the id. The answer prints the id first. A
`create` with the same `id` and the same fields returns the existing
investigation, so to recover a `create` whose answer was lost, send it again
with the printed id. A failed `create` prints the id it sent for this reason.

`read` prints the investigation and its board: every entry changed after
`since`, in change order, one line each. It asks trawld for at most 50 entries
a call. `keep` keeps the named rows, and when they are rows of the key's own
temporary result, it keeps the whole result as evidence. `finding_status` and
`status` overwrite, so they carry `expected_seq`. `status` sends the status
alone, one field a request. When the investigation changed after
`expected_seq`, trawld answers `sequence_conflict`, and the text prints the sequence the call
expected, the current one, and the `read` call that prints what changed.

These are not actions: changing the title or the question, deleting an
investigation, and keeping its evidence past the retention ceiling. Deleting
and extending need `server_manage`, an operator's permission, through the
[investigations API](/reference/api/).

A refused call is an `isError` result with trawld's code. Nothing retries it.

| Refusal | What the text adds |
|---------|--------------------|
| HTTP 403 | The permissions the action needs. `orient` lists the key's permissions |
| HTTP 404 | No investigation has that id. `investigation action=list` lists them |
| `sequence_conflict` | The expected and current sequence, and `investigation action=read id=ID since=EXPECTED` |
| `investigation_exists` | An investigation with that id has another creator or other fields. Read it, or create without an id |
| `investigation_closed` | `investigation action=status id=ID status=open` reopens it |
| `evidence_budget_exceeded` | Evidence frees only when it ages out or an operator deletes an investigation |

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
DEL, the C1 control characters, the bidirectional controls, the zero-width
characters and joiners, the byte order mark, the soft hyphen, and U+2028 and
U+2029 as `\uXXXX`, on top of the escapes JSON requires. These characters can
reorder or hide the text around them. The free text that the server writes, and
trawld's messages, print on one line with line breaks and the same characters
escaped.

`structuredContent` carries the same escaped text. In every string of the
mirror, each of these characters is the six characters of its `\uXXXX` escape,
as the text block prints it. A client can show the model the mirror instead of
the text block, and Claude Code does. The control characters below U+0020 stay
as they are in the mirror, because JSON escapes them in the serialized mirror.
A mirror string that holds one of these escapes does not decode to the stored
text. To get the stored bytes, read the row or the entry through the
[HTTP API](/reference/api/).

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

### Activity

Activity is what other keys changed in an investigation since this session
last saw it. One `trawl mcp` process is one session. For each investigation,
the session remembers the last sequence it printed, and it prints each entry
that another key changed once, at most, in that session.

- `note`, `finding`, `keep`, and an attached `search` send the session's
  sequence to trawld as `seen_seq`. trawld answers with the entries that other
  keys changed after it, at most 50, and says when more follow. The answer
  prints them under `activity by other keys since #N, in change order`.
- The first call that touches an investigation in a session sends no
  `seen_seq`, and its answer prints no activity. Its `activity:` line says that
  the session starts to follow the investigation there, and names the `read`
  call that prints the board. After a `create`, a write, or a status change,
  the session starts at that sequence. trawld's answer to an attached search
  names the sequence of the step it appended, `step_seq`, and the session
  starts at that step once the answer fits its budget.
- `finding_status` and `status` answer with no activity. The next `note`,
  `finding`, `keep`, or attached search prints it.
- A board that `read` prints counts as seen, unless its `since` is past the
  session's sequence. Such a board leaves a gap, so the session keeps its
  sequence.
- The session's sequence moves only to the `updated_seq` of the last entry the
  answer printed, and only after the answer fits its budget. A refused or
  failed call moves nothing, and neither does an answer that does not fit.
  This holds for the first attached search too: when its page fails or its
  answer does not fit, the session does not start at its step, and the next
  attached call sends no `seen_seq`.
- After trawld accepted an attached search, the search is a step of the
  investigation and its result is kept for it. An error after that point, such
  as a page that fails or an answer that does not fit, opens with a line that
  names the investigation and the kept result.

Each entry prints as one line:
`#SEQ [(changed at #UPDATED_SEQ)] KIND by PREFIX "NAME" at TIME: BODY`. The
key's prefix and name are the ones it had when it wrote the entry. Stored text
prints as JSON strings with the same characters escaped as above, so a title,
note, or name another key wrote cannot reorder or hide the text around it. In
an activity block, an entry whose line is longer than 1024 bytes prints its
header and a marker instead of its body:
`…[+N bytes: investigation action=read id=ID since=S]`, where the call prints
the entry on the board. A board prints each entry whole, limited only by the
response budget. When the budget does not hold the first
entry of a board, that entry prints its header and a marker. The marker names
the same read with `max_bytes=32768` when that budget holds the entry. Else it
names the investigation's page in the web UI.

An entry's header, its sequence, kind, and attribution, is never shortened.
When the budget does not hold even the header of a board's first entry, for
example a key name of 40000 bytes, the read is an error, never an answer that
prints no entry. The error names the investigation, the entry's sequence, and
the read past it, with `since` set to the entry's `updated_seq`. It names a
larger `max_bytes` when 32768 holds the entry, and the investigation's page in
the web UI.

When the budget cuts activity or a board, it drops whole entries from the end,
never from the middle, and the block says how many it dropped. The answer
then proposes `investigation {"action":"read","id":ID,"since":S}`, where `S`
is the `updated_seq` of the last entry printed. It proposes the same call when
trawld said more entries follow. On a `read action=investigation` board the
proposed call is the matching `read` call. The continuation lives in the
activity block, so it stays when the budget drops the other next calls.

A write that trawld accepted is acknowledged at every `max_bytes`, from 4096
to 32768. The outcome line of every `create`, `note`, `finding`, `keep`,
`finding_status`, and `status` answer names the investigation id, and every
answer but `create`'s names the sequence written. A `create`'s outcome line
names only the id. The header line that follows it prints the sequence,
`seq 0`. If the answer is still over its budget without its entries and next
calls, the server shortens the answer, not the acknowledgement:

1. The entry written prints its header and the marker
   `…[+N bytes: investigation action=read id=ID since=S]`.
2. The title, question, and scope print as prefixes cut on a character
   boundary. Each prefix ends with `…[+N bytes: WHERE]`. `WHERE` is
   `investigation action=read` with `max_bytes=32768` when that budget holds
   the whole header, else the investigation's page in the web UI.

`N` counts the bytes of escaped text left out. `structuredContent` carries the
same prefixes, escaped as the text block prints them, and lists each shortened
text under `investigation.abbreviated`.
The id, status, sequence, retention, and attribution are never shortened. A
board read shortens its header the same way.

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
| Activity entry line | 1024 bytes; a longer activity entry prints its header and a marker. A board prints entries whole, within the response budget |
| Activity entries per answer | 50, trawld's limit, then a continuation |
| Board entries per read request to trawld | 50 |
| Open investigations in `orient` | 5 |
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
  excerpt prints each control character, bidirectional control, zero-width
  character, joiner, byte order mark, soft hyphen, and line or paragraph
  separator as U+FFFD, and keeps tabs. The server drops an excerpt whose span
  is reversed, past the end of the text, or off a character boundary, and keeps
  the message.
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
- OAuth.

The reasons and the planned order are in
[ADR-0058](https://github.com/jakub/trawl/blob/main/docs/adr/0058-the-mcp-server-is-a-thin-client-that-answers-in-text-first.md).
Investigations are
[ADR-0059](https://github.com/jakub/trawl/blob/main/docs/adr/0059-an-investigation-is-grant-visible-and-its-evidence-outlives-the-corpus.md).
The captured results it reads are
[ADR-0057](https://github.com/jakub/trawl/blob/main/docs/adr/0057-a-capture-is-an-immutable-expiring-result-that-later-reads-page.md).
