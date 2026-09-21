---
title: Query Language (DSL)
description: Look up the exact syntax and evaluation rules of trawl's pipeline query language.
---

A trawl query is a search stage followed by pipe stages. The search stage selects events. Each `|` passes rows to the next stage.

The [event reference](/reference/events/) defines the input fields. [Query execution](/architecture/query-execution/) describes how the lanes run a query.

## Query structure

```text
[search stage] | [pipe stage] | [pipe stage] ...
```

The search stage is optional, so a query may start with its first pipe stage. Whitespace between search tokens is an implicit AND.

- A query may be 65536 bytes long. A longer one returns `query too long (N bytes, max 65536)`.
- One parse reports at most 8 errors, and the last message carries `(N earlier errors omitted)`.
- A `/pattern/` filter value holds at least one character, cannot contain `/`, and may be 1024 characters long. A longer one returns `regex pattern too long (N chars, max 1024)`, and an unparseable one returns `invalid regex: <detail>`.

## Comments

`#` starts a comment that runs to the end of the line. `#` is the only comment character.

A comment opens at the start of the input, or directly after a space, tab, carriage return, or newline. Other Unicode whitespace separates two tokens without opening a comment, so `foo`, a no-break space and `# note` is one token carrying a `#`.

```
# find the noisy services
_severity>=error last=1h    # only the last hour
| stats count() by service
| where count > 10
```

To carry a `#` in a value or a name, quote it. A double-quoted value, a backtick-quoted name and a regex body all carry it verbatim. `//` is not a comment opener, so a value carries it unquoted.

| Query | Result |
| --- | --- |
| `a=1 # note` | The filter `a=1`, plus a comment. |
| `a=1# note` | Parse error: `'#' inside an unquoted value` |
| `color=#ff0000` | Parse error: `'#' inside an unquoted value` |
| `foo#bar` | Parse error: `'#' inside an unquoted token` |
| `count(),# x` | Parse error: `'#' inside an unquoted token` |
| `-foo#bar` | Parse error: `'#' inside a negated search term` |
| `co#unt()` as a stage name | Parse error: `'#' inside a pipe stage name` |
| `//cdn.example.com` | Parse error: ``'//' does not start a comment — '#' is the comment character`` |
| `-//cdn.example.com` | An ordinary negated search term. |
| `host="a#b"` | The value `a#b`. |
| `` `a#b`=1 `` | A filter on the field `a#b`. |
| `message=/a#b/` | A regex containing `#`. |
| `url=https://example.com/x` | The value `https://example.com/x`. |
| `path=/api//v1` | The value `/api//v1`. |

Each error carries a hint, and the hint spells a concrete rewrite only where the production that failed held the exact text. Every hint ends with `put whitespace before the '#' to start a comment`.

- Unquoted search term: `quote it ("foo#bar") to search for it, or ...`
- Unquoted value: `quote the value ("#ff0000"), or ...`. A value holding `*` or `?` adds `to carry the '#' (a quoted value is never a pattern)`.
- Negated search term: `write it as NOT "foo#bar", or ...`
- Any other position, including a stage name: the whitespace advice alone, or ``put whitespace before the '#' to start a comment, or carry the '#' inside a quoted value ("…") or a backticked field name (`…`)``
- A bare term starting with `//`: `write '#' to start a comment, or quote the term to search for the slashes`

## Search stage

The search stage filters events before they enter the pipeline. Tokens separated by whitespace are AND-joined.

### Field filters

```text
<field><operator><value>
```

| Operator | Meaning |
| --- | --- |
| `=` | Equal, or membership when the value is a comma list. |
| `!=` | Not equal, or non-membership when the value is a comma list. |
| `>` `>=` `<` `<=` | Ordered comparison. |
| `=<value with * or ?>` | Glob, detected from an unquoted value. |
| `=/pattern/` | Regex, detected from the value. |

```
# exact match
service=nginx
# IN list
status=200,301,404
# ordered comparison
status>=400
# glob pattern
path=/api/*
# regex pattern, slash-delimited
message=/error.*/
# quoted value, for spaces and special characters
host="db host"
```

An unquoted value holding `*` or `?` becomes a glob whatever operator you typed, and a quoted value is never a glob. A `/pattern/` value becomes a regex, and its closing `/` must be followed by whitespace, a stage separator, or the end of the query. Globs and regexes are case-sensitive.

Comma lists take `=` and `!=` only, so an ordered spelling such as `f>=a,b` is a parse error. `f=a,b` is membership, and `f!=a,b` is `f!=a AND f!=b` with the search-stage null rule on each term, so an event without `f` matches it.

A bare `*` term matches everything and emits no filter. A bare field name is `[A-Za-z_][A-Za-z0-9_]*` with `.` separating segments; `@name` and a backtick-quoted name are field names too.

#### Quoted list elements

Any element of a comma list may be quoted, in any position. The quotes are quoting, not data.

```
# the three values 200, 301, 404
status=200,"301",404
# a value with a space, beside a bare one
host="db host",web-01
# a value carrying a comment opener
tag="a#b",plain
```

Quoting is how a value the bare production cannot spell reaches a list: a space, a backtick, or a `#`. A list whose first element is quoted and whose rest is bare stays one list, so `a="x",y` is `a IN ("x","y")`.

#### Pinned comparison semantics

On a server, every stored field carries a type pin in the field catalog. The envelope columns pin on install, and a custom field pins at first typed sight. `host`, `service`, `env`, `message` and `_raw` hold VARCHAR pins. `_severity` holds a SEVERITY pin.

What is stored is the value's reading under the pin, when that reading is value-preserving. A cast that would alter the value shelves it instead: the column holds NULL, `trawl schema conflicts` records the disagreement, and the original text stays in `_raw`. Spelling drift is not alteration, so `"0404"` under a BIGINT pin stores as `404` while `"1.5"` shelves rather than rounding to `2`.

What matches is decided by the pin, not by the shape of the query literal. Batch SQL, live tail and the kv batch tail answer identically. The hot buffer conforms through the same expression compaction writes with. Embedded `--data` has no catalog and no pins, so every comparison there is literal-driven.

##### The numeric reading

Both VARCHAR-pinned numeric rules read the column and the literal through one `DECIMAL(38,6)` cast, which is exact for every 64-bit integer and out to 10^32.

| Text | Reading |
| --- | --- |
| `" 200"` | 200. ASCII whitespace at both ends is ignored. |
| `"200_000"` | 200000. `_` separates two digits. |
| `"0404"` | 404. |
| `"+5"`, `"1."`, `".5"`, `"1e3"`, `"1E-3"` | The number they spell. |
| `"nan"`, `"inf"`, `"infinity"` | No reading. |
| `"0x10"`, and any radix prefix | No reading. |
| `"1,000"`, and any grouped digits | No reading. |
| Empty or blank text | No reading. |
| A magnitude at or above 10^32 | No reading. |
| A fraction below 10^-6 | Quantized, rounded half away from zero, so two values a nanosecond apart compare equal. |

No reading means unknown, never a false match, and `NOT` cannot invert an unknown into a match.

##### The rules, per pin

The pattern target is what a glob or regex matches against.

| Pin | `=`, `!=`, `in` | Ordered comparison | Pattern target |
| --- | --- | --- | --- |
| None | Literal-driven. | Literal-driven. | The column. |
| VARCHAR | The exact text, or any spelling of the same number when the literal is numeric. | Numeric for a numeric literal, lexical otherwise. | The column. |
| BIGINT | The stored integer. | The stored integer. | The integer's text, such as `404`. |
| DOUBLE | The stored double. | The stored double. | DuckDB's double text, which always carries a fraction and takes a signed two-digit exponent outside 1e-4 to 1e16: `200.0`, `0.0`, `-3.0`, `1e-07`, `1.2345678901234568e+17`. |
| BOOLEAN | The stored boolean. | Not applicable. | `true` or `false`, lowercase. |
| TIMESTAMP | The stored instant. | The stored instant. | RFC 3339 UTC microseconds, with a `T` separator, six fractional digits and a trailing `Z`: `2026-01-15T09:00:00.000000Z`. |
| SEVERITY | The band containing the token. | The token's exact number. | The canonical OTel short name, such as `error2`. |

- On a VARCHAR pin, `status=200` finds `"200"`, `"0200"` and `"200.0"`, and not `"accepted"` or `"404"`; `status!=200` returns `"accepted"` and every other non-200 value. A literal the numeric space cannot read, such as `status=nan` or `id=1e40`, falls back to the text comparison alone.
- On a VARCHAR pin, `status>=400` matches `"404"` and `"500"`, while `"accepted"` is unknown, so `NOT status>=400` does not match it either. A literal with no reading matches nothing on either side.
- A BIGINT pin stores `"0404"`, `"4.0"`, `" 200"`, `"200_000"` and `"1e3"` as their integer, so `status=4*` finds 404 and `status=0*` does not. `"1.5"`, `"0x10"` and `"accepted"` shelve.
- A BOOLEAN pin keeps only the values the wire spelled `true` or `false`. `"TRUE"`, `"t"`, `"yes"` and `"1"` are inside DuckDB's cast but do not survive the round trip, so they shelve. `flag=/^true$/` matches a stored `true`, and `flag=TRUE*` matches nothing.
- A DOUBLE pin stores anything its cast reads, so it is the one pin with no round-trip guard. `dur=/^200$/` matches nothing while `dur=/^200\.0$/` matches.
- A TIMESTAMP pin applies an offset in the wire text, so `2026-01-15T09:00:00+05:30` stores and matches as `2026-01-15T03:30:00.000000Z`. Text without an offset reads as UTC, a bare date is midnight, and fractions truncate at six digits. The envelope's `_time` and `_ingested` are canonicalized at ingest, so this rule reaches only a custom timestamp-pinned field.
- A typed pin compares the conformed value, so a wire `1.5` under a BIGINT pin is shelved: `duration>1` does not match it, `NOT duration>1` does not either, and `duration!=2` does.
- A literal under a typed pin binds as it does on an unpinned field, and DuckDB reads it against the column's type. `flag=TRUE` and `flag=yes` both match a stored `true`, and an offset inside a timestamp literal is ignored where the same offset in a stored value is applied.
- A shelved value is a NULL column, which is unknown, not false.

#### Missing fields and nulls

A field an event does not carry is a NULL column, and a comparison against NULL is unknown: neither true nor false. Only a true row is returned. This holds in every lane, on pinned and unpinned fields alike.

| Spelling | Events missing the field | Emitted shape |
| --- | --- | --- |
| `f!=x` in the search stage | Match. | `("f" != ? OR "f" IS NULL)` |
| `NOT f=x` in the search stage | Do not match. | `NOT ("f" = ?)` |
| `where f != x` in a pipe stage | Do not match. | `("f" != ?)` |

On live tail, `f!=x` over a sparse custom field streams nearly everything, so pair it with `f=*` to require the field. To match events missing `f` plus events whose `f` is not `x`, write `f!=x`. The same holds for `NOT _severity=...` when an event has no derived severity. A repin does not change missing-field semantics.

Numeric-literal detection reads content, not quoting: `status>"400"` and `status>400` are the same query, and `where status == "400"` binds as `where status == 400`.

#### Pin-aware `| where` and `| let`

A bare field-vs-literal comparison inside `| where` or `| let` reads the same catalog pin the search stage reads, and applies the same rule table.

```
# DECIMAL(38,6) comparison on a VARCHAR pin
| where status > 400
# the stored text or its numeric reading
| where status == 200
# each element routes through the equality rule
| where status in (200, "accepted")
```

Three shapes are pin-aware: `where field <op> literal`, the reversed `where literal <op> field` (`400 < status` is `status > 400`), and `where field matches|like|ilike <pattern>`, where only the left operand is a subject.

Everything else stays literal-driven: field against field (`where a == b`), a function around the field (`where lower(status) == "a"`), arithmetic on the field (`where status * 2 > 400`), `where f == null`, a pattern with the field on the right (`where "x" matches f`), and every comparison on an unpinned field.

Which pin applies follows the pipeline, not a flat name lookup.

| Stage | Effect on pins |
| --- | --- |
| `rename status as st` | `st` carries the pin, and `status` no longer resolves it. |
| `let s2 = status` | A bare alias copies the pin. |
| `let status = <expr>` | A computed value has no pin. |
| `stats ... by status` | Group keys keep their pins. Aggregate outputs, aliases and the `timechart` bucket are never pinned. |
| `extract <regex>` | Capture-group names lose their pins. |
| `extract kv` | Passes the scope through. A kv key that shadows a pinned field name is read under that pin. |
| `from saved` | No pins apply. |

Inside one `let`, a reference to a sibling target resolves the way the SQL does: an existing column of that name wins, so `let a = 1, b = a` gives `b` the original `a`, and only a name the row does not carry reads the sibling's fresh value, so `let ms = 1000, total = ms * 2` gives `total = 2000`. Pins stay pre-stage, so `let a = status, b = a` gives `b` no pin.

Timestamps in the kv batch tail compare as stored instants and shift to your display timezone last. The shift follows the tail's lineage, so `rename _time as t` and the bare alias `let t2 = _time` render in your zone, while a computed value such as `let t = coalesce(_time, x)` and every aggregate output render as UTC text.

### The two namespaces

Bare names identify sender fields, with ASCII case folding and catalog conformance applied at ingest and storage. `env`, `service`, `host` and `message` have declared envelope roles. Other bare names, such as `level` and `timestamp`, are ordinary sender fields, and there are no field-name aliases: use `_time` for the event instant and `_severity` for derived severity.

The underscore namespace belongs to trawl. Its declared slots are `_time`, `_ingested`, `_raw`, `_repairs`, `_severity` and `_producer`, with presence rules in the [event reference](/reference/events/#declared-fields).

- An incoming reserved name loses its leading underscore run. See [name handling](/reference/events/#names-and-original-values) for proposal exceptions and collisions.
- `let _foo = 1` and `rename x as _foo` are parse errors naming the reserved namespace, and an `_foo` regex capture in `extract` is refused before the query runs.
- A kv key starting with `_` is dropped. Its text stays findable in the source field and `_raw`.
- The stage aliases `head`, `fields`, `eval` and `rex` remain valid.

### Backtick-quoted names

Any field name can be written between backticks, and a backticked name is always a field reference.

```
# a name the bare form cannot spell
`http-status`=500
# the field last, beside the time keyword
`last`=5 last=2h
# a doubled backtick escapes one: the name a`b
| table `a``b`
```

Backticks change how a name is lexed, never what a name may be. Content is any character except a backtick, and a doubled backtick escapes one. A dot inside the quotes is a literal character, not a segment separator. The ASCII fold still applies, so `` `Dur` `` is `dur`, and the `_` namespace is still sealed, so ``let `_foo` = 1`` is the same error as the bare spelling.

- An empty name is a parse error: ``` empty field name: `` names no column ```
- A control, bidi or zero-width format character, or the soft hyphen, is a parse error: `field name contains a control or invisible format character (U+XXXX); such characters are not part of any column name`

Backticks are accepted in every field position: search-stage filters, expressions, aggregation arguments, `by` keys, `table`, `fields`, `sort`, `drop`, `dedup`, both sides of `rename`, `let` targets and `as` aliases. They are not accepted for a function name, a stage name, or a saved-query name, so `` `lower`(x) `` is a field reference rather than a call.

A backtick also ends an unquoted value and an unquoted word, so text containing one is written double-quoted, as in `` host="a`b" `` and `` "er`ror" ``. Backticks are not value quotes, so `` service=`nginx` `` is a parse error, and `-` cannot negate a quoted name, so write ``NOT `http-status`=500``.

The search stage reads `last=`, `earliest=` and `latest=` before anything else, and backticks reach fields with those names. Inside `| where`, `| let` and aggregation arguments, the words an expression is made of are read before a field reference is tried: `true`, `false` and `null` are literals, and `and`, `or`, `not`, `in`, `matches`, `like` and `ilike` are operators. ``| where `true` == 1`` filters on the column named `true`.

A `#` inside backticks is part of the name. See [Comments](#comments).

### Severity: `_severity`

`_severity` is the derived OTel SeverityNumber, 1 to 24, pinned to the `SEVERITY` type on every install.

```
# the ERROR band: BETWEEN 17 AND 20
_severity=error
# NOT BETWEEN 9 AND 12, or _severity IS NULL
_severity!=info
# two adjacent bands merge: BETWEEN 13 AND 20
_severity=warn,error
# disjoint, so two ranges: 13-16 OR 21-24
_severity=warn,fatal
# the token's exact number: >= 13
_severity>=warn
# the OTel exact short name: exactly 18
_severity=error2
# glob over the canonical token text: 13-16
_severity=warn*
```

- Equality and `in` match the whole band containing the token, so `notice` falls inside the INFO band. An ordered comparison uses the token's exact number.
- A comma list is one set over the ladder, emitted as the minimal contiguous ranges covering it: `warn,17` is 13 to 17, and all six base bands are one `BETWEEN 1 AND 24`. The merge changes speed, not which rows match.
- A negated comma list composes scalar `!=`, so `_severity!=warn,error` matches severities outside 13 to 20, and events with no `_severity`.
- The exact OTel short names, such as `trace2`, `warn3` and `error2`, name one number under every operator.
- Globs and regexes match the canonical OTel short name of the stored number, which is what results display: a `_severity` cell reads `error`, not `17`. `-f json`, `-f csv` and SSE carry the number.
- An unrecognized value is a query error: ``unknown severity value 'x' — a severity field takes a band token (trace, debug, info, notice, warn, error, fatal, alert, emerg), an exact OTel short name (error2, warn3), or a number on the 1-24 ladder``
- Embedded `--data` has no `SEVERITY` pin, so compare the ladder number there, as in `_severity>=17`.

Band tokens are case-insensitive.

| Token and aliases | Number |
| --- | --- |
| `trace`, `t` | 1 |
| `debug`, `d` | 5 |
| `info`, `i` | 9 |
| `notice` | 10 |
| `warn`, `warning`, `w` | 13 |
| `error`, `err`, `e` | 17 |
| `fatal`, `critical`, `crit`, `f` | 21 |
| `alert` | 23 |
| `emerg`, `panic` | 24 |

trawl derives `_severity` at ingest and never accepts it from a sender. The packaged `[ingest] severity_from` reads `severity`, then `severity_text`, then `level`, and the first mappable source wins. trawl retains those source fields under their own names. A word maps through the token table or the exact names, and a number maps as OTel 1 to 24, so `3` is `trace` unless the source sets `dialect = "syslog"`. An event with no mappable source has no `_severity`.

`level` is an ordinary sender field, so `level=error` compares its stored value. An event carrying `{"service":"game","level":"gold"}` has a queryable `level` column holding `gold`.

#### Reading any field as a severity: `sev()`

`sev(x)` applies the same reading at query time, to any field you name.

```
# the ordered rule: >= 17
| where sev(level) >= "error"
# the equality rule: the 17-20 band
| where sev(level) == "error"
# a foreign syslog numeral
| where sev(syslog_severity, "syslog") == "error"
```

- The vocabulary is the band tokens with their aliases, the OTel exact short names, then a strict integer. Anything else, including `1.5`, `1e1`, `0x10`, `gold` and a boolean, has no reading, which is `null` rather than an error.
- `sev()` declares its result as `SEVERITY`, so comparisons against it bind through the severity rules, the pin travels through `| let s = sev(level) | stats count() by s`, and the column displays tokens in the CLI table, the TUI and the web UI.
- Literals must be quoted. A bare `error` in a pipe stage is a field reference.
- The subject must be `sev(<field>)` directly. `sev(lower(x))` still computes the reading, but the comparison around it falls back to the literal-driven path, so `sev(lower(x)) == "error"` compares against the string: a `Conversion Error` in batch, and a filter that never matches on live tail. Bind it first with `| let s = sev(lower(level)) | where s == "error"`.
- `dialect` governs numbers only. Words always read the one token table. `sev(x, "syslog")` inverts 0 to 7, so `3` is `err`, which is 17. An unknown dialect is a query error naming `otel` and `syslog`.
- `sev()` works in embedded `--data`, where nothing else is pin-aware. The function declares the type.

#### Putting the field itself on the ladder

To make a field be a severity rather than read one at query time, an operator repins the column once with `trawl schema repin level --to severity`. See the [CLI reference](/reference/cli/#repin) for the command and its dry run.

- After the repin, `level` behaves as `_severity` does in every lane, for stored history and for events ingested after the cutover.
- The repin dialect applies to the historical rewrite only, and new values conform under the OTel interpretation. With `--dialect syslog`, a historical raw `3` becomes 17 while a new raw `3` reads as OTel 3. Text severity tokens carry no such ambiguity.
- `severity_from` with a syslog dialect derives `_severity` without rewriting the bare source field. For a continuing syslog-number sender, query `_severity` or `sev(field, "syslog")`.
- `_severity` cannot be repinned. Its type is part of the event contract.

### `timestamp` and `@timestamp`

Ordinary sender fields. trawl reads them as sources for the `_time` derivation, in the packaged `[ingest] time_from` order `_time`, then `timestamp`, then `@timestamp`, first present wins. trawl retains them under their own names and their own catalog pins, and the original representation stays in `_raw`, subject to its cap. Only `_time` is consumed and canonicalized.

`| sort -timestamp` sorts the sender's column. Sort, filter and project `_time` when you mean the event's instant.

### Text search

```
# substring match, case-insensitive
error
# negated, excludes matches
-debug
# exact phrase
"connection refused"
```

Bare words and phrases match `message` and `_raw`, case-insensitively. Negation excludes an event when either column matches. Containment is two-valued, so a missing, null or non-text `message` or `_raw` does not contain the term, and `-debug`, `NOT debug` and `NOT "debug"` agree even on foreign data that lacks one or both columns.

Searching `_raw` is whole-event search. `_raw` holds the most original form of the event, so what a term reaches depends on who filled it.

- A collector that sent its own string `_raw`: the pre-parse line, matched as text.
- Everything else: the server's JSON serialization of the event as it arrived. A term matches another field's value, so `nginx` finds an event with `service=nginx` even when `message` never says it, and a term matches a field name, so `debug` finds an event carrying `debug_mode` and `-debug` excludes it.

A field filter never consults `_raw`.

```
# regex, message only
message=/debug/
# glob, message only, case-sensitive
message=*debug*
```

### Time filters

```
# a relative window; units are s, m, h, d, w
last=2h
# an absolute lower bound, quoted
earliest="2026-03-14T03:00:00Z"
# an absolute upper bound, quoted
latest="2026-03-14T03:15:00Z"
```

| Keyword | Argument | Bound |
| --- | --- | --- |
| `last=` | An unquoted duration: a positive integer and one of `s`, `m`, `h`, `d`, `w`. | `_time >= now() - duration`, with no upper bound. |
| `earliest=` | A quoted timestamp. | `_time >=` the bound, inclusive. |
| `latest=` | A quoted timestamp. | `_time <` the bound, exclusive. |

The three keywords are read before any field filter, wherever they appear, and they apply to the query globally. Fields of those names are reachable with backticks, as in `` `last`=5 ``.

- `last=` with `earliest=` or `latest=` is refused: `cannot combine 'last=' with 'earliest='/'latest='`
- A repeated keyword takes its last spelling.
- A zero duration returns `duration must be greater than zero`, and an overflowing one returns `duration too large`.
- The pair is half-open, `[earliest, latest)`, so an event at `2026-03-14T03:00:00Z` matches `earliest="2026-03-14T03:00:00Z"` and does not match `latest="2026-03-14T03:00:00Z"`. Consecutive windows tile.

### Scheduled windows

A saved query with a windowed schedule does not spell its own interval. The schedule owns the window, and each run gets absolute bounds spliced onto the front of the saved text before it executes.

```
# the saved DSL
_severity>=error | stats count() by service
# what the 03:00 run executed, for window = "since_last" and lag = 5m
earliest="2026-03-14T01:55:00.000000Z" latest="2026-03-14T02:55:00.000000Z" _severity>=error | stats count() by service
```

- The stored `query` on a run row is that resolved text, so pasting it into `trawl query` reproduces the report.
- The splice is valid ahead of a field filter, a bare word, a comment or a leading stage separator, and leaves your text byte for byte.
- `last=`, `earliest=` or `latest=` in a windowed saved query is a 400 naming both, in both directions. A backticked `` `last`=5 `` is an ordinary field filter and is unaffected.
- `| from saved` may not have a window. It reads stored report rows, not ingest events.
- An interactive run of a scheduled saved query carries no window. The bounds exist only on the runs the scheduler made.

Setup and mechanism are in [scheduled reports](/architecture/reports-telemetry/#scheduled-reports).

### OR grouping

```
# either filter matches
service=nginx OR service=apache
# implicit AND within groups: (a AND b) OR (c AND d)
a b OR c d
# parentheses group explicitly
(service=nginx OR service=apache) status>=400
```

`OR` may be spelled `OR` or `or`. `NOT` is uppercase only, and a lowercase `not` in the search stage is a bare search term. Whitespace binds tighter than `OR`. Parentheses group tokens and may nest. A time keyword inside a group is hoisted out and applied globally.

## Pipe stages

The parser defines 18 stage variants. `head`, `fields`, `eval` and `rex` are alternate spellings, not additional variants.

| Stage | Purpose |
| --- | --- |
| [stats](#stats) | Aggregate rows, optionally by fields. |
| [where](#where) | Filter rows by an expression. |
| [sort](#sort) | Order rows by fields. |
| [limit / head](#limit--head) | Keep the first rows. |
| [tail](#tail) | Keep the last rows. |
| [table / fields](#table--fields) | Select columns. |
| [top](#top) | Count the most common values. |
| [rare](#rare) | Count the least common values. |
| [drop](#drop) | Remove columns. |
| [let / eval](#let--eval) | Compute or replace values. |
| [extract / rex](#extract--rex) | Extract regex captures or key-value fields. |
| [rename](#rename) | Rename fields. |
| [dedup](#dedup) | Remove duplicate rows or keys. |
| [timechart](#timechart) | Aggregate into time buckets. |
| [pivot](#pivot) | Turn distinct values into aggregate columns. |
| [sample](#sample) | Sample a percentage or a row count. |
| [eventstats](#eventstats) | Add grouped aggregate values to each row. |
| [from saved](#from-saved) | Read stored report runs. |

### stats

```text
stats <agg>[ as <name>][, <agg> ...] [by <field>[, <field> ...]]
```

| Argument | Required | Meaning |
| --- | --- | --- |
| `<agg>` | Yes | An [aggregation function](#aggregation-functions) call. |
| `as <name>` | No | The output column name. |
| `by <field>` | No | Group keys. |

```
# one row, one column
stats count()
# one row per host
stats count() by host
# several aggregates, several keys
stats count(), avg(duration) by service, host
```

`stats` reduces each group to one output row, and projects the group keys plus one column per aggregate. Group keys keep their catalog pins; aggregate outputs do not.

### where

```text
where <expression>
```

```
# combine conditions
where status == 200 and count > 5
# regex and membership
where host matches /prod-.*/ and x in (1, 2, 3)
```

The condition is an [expression](#expressions). A bare field-vs-literal comparison is pin-aware. `!=` carries no null widening here, so a row whose field is missing is filtered out.

### sort

```text
sort [-]<field>[, [-]<field> ...]
```

```
# ascending
sort count
# descending, then a second key
sort -count, status
```

A leading `-` sorts descending. There is no `+` prefix.

### limit / head

```text
limit <n>
head <n>
```

```
# keep the first 20 rows
limit 20
# the same stage
head 20
```

`<n>` is a non-negative integer.

### tail

```text
tail <n>
```

```
# the last 5 rows
tail 5
```

Without an earlier `sort`, `tail` orders by `_time` descending first. With one, it keeps that order and limits.

### table / fields

```text
table <field>[, <field> ...]
fields <field>[, <field> ...]
```

```
# select two columns
table host, status
# the same stage
fields host, status
```

Columns appear in the order you name them.

### top

```text
top <n> <field> [by <field>[, <field> ...]]
```

| Argument | Required | Meaning |
| --- | --- | --- |
| `<n>` | Yes | How many values to keep. |
| `<field>` | Yes | The field whose values are counted. |
| `by <field>` | No | Group keys. |

```
# the 10 most frequent hosts
top 10 host
# the 5 most frequent statuses per service
top 5 status by service
```

`top` mints its own `count` column and cannot spell `as`.

### rare

```text
rare <n> <field> [by <field>[, <field> ...]]
```

```
# the 5 rarest status codes
rare 5 status
# the 3 rarest hosts per service
rare 3 host by service
```

The arguments and the minted `count` column match [top](#top).

### drop

```text
drop <field>[, <field> ...]
```

```
# remove one column
drop message
# remove several
drop host, raw
```

### let / eval

```text
let <name> = <expression>[, <name> = <expression> ...]
eval <name> = <expression>[, <name> = <expression> ...]
```

```
# a computed column
let duration_ms = duration * 1000
# the same stage
eval status_class = floor(status / 100)
```

A target naming an existing column replaces it. A target in the `_` namespace is a parse error, and two targets that fold to one name in a single stage are a parse error. See [Pin-aware where and let](#pin-aware--where-and--let) for how a sibling reference resolves.

#### Arithmetic

`/` is true division, so `status / 100` over a `404` is `4.04`, and the result is a DOUBLE whatever the operands were. Wrap it in `floor()` for the integer part: `floor(status / 100)` answers `4.0`, also a DOUBLE.

`+`, `-` and `*` stay integral over two integers. An integer overflow raises an error in batch SQL and yields `null` in the streaming lane.

#### Infinities and NaN

Division never fails: `1.0 / 0` is `inf`, `-1.0 / 0` is `-inf`, and `0.0 / 0` is `NaN`. `%` follows `/` whenever either side is a float, so `5 % 0.0` is `NaN`, while integer `5 % 0` is `null`.

These values flow between stages like any other number and compare in DuckDB's total order, not IEEE's.

- Every NaN equals every other NaN and outranks every finite value, so `| let x = 0.0 / 0 | where x == x` keeps the row and `max(x)` over a column holding one answers `NaN`.
- The two zeros compare equal, so `-0.0` groups with `0.0`.
- JSON has no spelling for any of them, so the API wire, `-f json` and the SSE stream render `null`. The table and CSV renderers print `inf`, `-inf` and `NaN` under embedded `--data`, the one path that does not cross the JSON wire.

### extract / rex

```text
extract "<regex>" [from <field>]
extract kv [sep="<char>"] [from <field>]
```

| Argument | Required | Meaning |
| --- | --- | --- |
| `"<regex>"` | One of these two | A pattern whose named capture groups become columns. Backslashes need no doubling. |
| `kv` | One of these two | Key-value parsing of the source text. |
| `sep="<char>"` | No | The key-value separator. Default `=`. A value longer than one character falls back to `=`. |
| `from <field>` | No | The source field. Default `message`. |

```
# named capture groups become columns
extract "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message
# the same stage, spelled rex
rex "(?P<code>[A-Z]+)" from raw
# key-value pairs, with a different separator and source
extract kv sep=":" from raw
```

- A capture group in the `_` namespace is refused before the query runs, in every lane, and so are two capture groups that fold to one name.
- A kv key is alphanumerics, `_` and `.`, starting with an alphanumeric or `_`. A kv value is a double-quoted string, or a run ending at whitespace or a comma.
- A kv key starting with `_` is dropped, and its text stays findable in the source field and `_raw`.
- `extract kv` moves the rest of the pipeline into the kv batch tail. See [Batch and streaming differences](#batch-and-streaming-differences).

### rename

```text
rename <field> as <name>[, <field> as <name> ...]
```

```
# one column
rename service as svc
# several columns
rename service as svc, host as hostname
```

The new name carries the old name's catalog pin. A new name in the `_` namespace is a parse error, and two new names that fold to one name in a single stage are a parse error.

### dedup

```text
dedup [<field>[, <field> ...]]
```

```
# exact duplicate rows
dedup
# one row per host and service
dedup host, service
```

Bare `dedup` keeps distinct whole rows. With fields, `dedup` keeps the row with the latest `_time` in each key group.

### timechart

```text
timechart [on <field>] [span=<duration>] <agg>[ as <name>][, <agg> ...] [by <field>[, <field> ...]]
```

```
# five-minute buckets
timechart span=5m count()
# one series per service
timechart span=1h count() by service
# buckets cut from each run's start time
| from saved daily_errors run=all | timechart on _run_time span=1h sum(count)
```

The bucket column is always named `_time`, and rows come back in bucket order. The buckets are cut from `_time` unless `on` names another column, which must already hold timestamps: trawl buckets it as stored and refuses a column of any other type. Without `span=`, trawl derives the bucket from the `last=` window.

| `last=` window | Bucket |
| --- | --- |
| Up to 1 hour, or no `last=` | 1 minute |
| Up to 6 hours | 5 minutes |
| Up to 24 hours | 15 minutes |
| Up to 7 days | 1 hour |
| Up to 30 days | 6 hours |
| Longer | 1 day |

### pivot

```text
pivot <agg> on <field> [by <field>[, <field> ...]]
```

```
# one column per distinct status
pivot count() on status
# one column per service, one row per host
pivot avg(duration) on service by host
```

`pivot` takes exactly one aggregate.

### sample

```text
sample <n>%
sample <n>
```

| Form | Range | Method |
| --- | --- | --- |
| `sample <n>%` | 1 to 100 | Bernoulli. Each row is independently eligible, so the row count is approximate. |
| `sample <n>` | 1 or more | Reservoir, for a fixed-size sample, limited by the available rows. |

```
# about a tenth of the rows
sample 10%
# up to 1000 rows
sample 1000
```

There is no seed argument and no stable ordering, so a repeated query can return different rows. Stage order matters: sampling a filtered relation and filtering a sample answer different questions.

### eventstats

```text
eventstats <agg> as <name>[, <agg> as <name> ...] [by <field>[, <field> ...]]
```

```
# a total on every row
* | eventstats count() as total
# compare a row against its group
* | eventstats max(duration) as host_max by host | where duration > host_max / 2
```

`eventstats` returns the input rows and adds the aggregate values. Without `by`, each aggregate covers all input rows. With `by`, each row receives its group's aggregate.

- Every aggregate needs an explicit `as`.
- Aliases must not collide with each other or with the grouping keys, under ASCII case folding. An alias naming an existing column replaces it, unless that column is a grouping key.
- `dc`, `distinct_count`, `values` and `list` are refused. Use an accepted aggregate such as `count`, `avg` or `max`.

### from saved

```text
from saved <name> [run=latest|all|<id>]
```

| Argument | Required | Meaning |
| --- | --- | --- |
| `<name>` | Yes | A bare name or a quoted string, never a backtick-quoted name. |
| `run=latest` | No | The newest successful run. The default. |
| `run=all` | No | Every successful run that has a Parquet result. |
| `run=<id>` | No | One successful run ID owned by the caller. |

```
# the newest run
| from saved daily_errors
# every run, tagged by run
| from saved daily_errors run=all | stats sum(count) by _run_id
```

`from saved` must be the query's first pipe stage, and the rest of the pipeline follows it. The source is a materialized report result, so trawl does not execute the saved query again. The name must resolve among the caller's saved queries.

- `run=latest` never substitutes an older run when the latest has no Parquet result.
- `run=all` adds `_run_id` and `_run_time`, the run's start time. It omits runs without a Parquet result, including zero-row runs, and resolution returns not found when no run qualifies.
- `run=<id>` checks ownership of that run, and does not require the run to belong to the named saved query, so use an ID from that query's run list. A missing, foreign-owned or unsuccessful run has no readable result.
- A successful empty result without Parquet becomes an empty relation with the recorded column names, so `stats count()` returns zero.
- A successful nonempty blob-only result returns a conflict. Fetch that run through the report-result API.
- Corrupt success metadata is an error, not an empty result.
- The remaining pipeline starts with no pins. Report columns can be computed values.

Read [scheduled reports](/architecture/reports-telemetry/#scheduled-reports) for window and stored-result behavior.

### Output names must be unique

Every aggregating stage projects a fixed set of columns: its group keys, `timechart`'s `_time` bucket, the `count` column `top` and `rare` mint, and one column per aggregate. An aggregate takes its `as` alias, otherwise a default name such as `count`, `avg_duration` or `dc_host`. An un-aliased aggregate over a computed argument takes its innermost field name, so `avg(tonumber(rssi) * -1)` is `avg_rssi`.

Two producers naming one column is refused before the query runs, in every lane, with both producers named.

```
# the group key `count` and the aggregate count()
| stats count() by count
# `n` and `N` are one name: names fold
| stats count() as n, sum(x) as N
# the bucket is always `_time`
| timechart span=1h count() by _time
# `top` mints its own `count` column
| top 5 count
```

The remedy is usually `as`, as in `| stats count() as hits by count`. `top` and `rare` cannot spell `as`, so write them out: `| stats count() as hits by count | sort -hits | head 5`. The in-memory lanes refuse an un-aliased aggregate over a computed argument outright.

### Query limits

Two fixed limits, checked before anything runs and identical in every lane. Neither is configurable.

| Limit | Value | Counted over |
| --- | --- | --- |
| Pipeline stages | 128 | Every stage, including `from saved` and the stages after `extract kv`. The search stage is not a pipeline stage. |
| Alias expansion | 512 | The whole query. |

Alias expansion scores one thing: an output of a stage naming an earlier output of the same stage. The database writes that earlier expression into the new one, once for every place the name appears.

- An expression that names no earlier output of its own stage scores zero, however large it is. Independent assignments, a 500-element `in (...)` list and a long pipeline of separate stages all score zero.
- Naming the same earlier output many times is charged for the whole expression copied at each use, minus the one reference it replaces. `base = status + 1` has three nodes, so each later `xN = base + N` adds two, and 256 such outputs score 512, exactly the admitted limit.
- trawl's generated SQL multiplies too. `sev(x) in (1,3,5,7,9,11,13,15,17,19,21,23)` writes its subject twelve times, once per contiguous range of the ladder.

Over either limit, trawl refuses the query before it reaches the database, naming the stage and the output that crossed the line.

```text
pipeline stage 1 (`let`) goes over this query's alias-expansion budget of
512 at the output `a7`: an output naming an earlier output of the same
stage is written into it once for every place it is named, so the work
multiplies — split the dependent assignments across separate `| let`
stages, so each one reads a finished column

this query has 129 pipeline stages, over the limit of 128; shorten the
pipeline, or save part of it and read it back with `from saved`
```

One `| let` per dependent step carries no alias expansion, however deep it goes.

```text
| let a0 = status + status | let a1 = a0 + a0 | let a2 = a1 + a1
```

`stats`, `timechart` and `eventstats` take the same fix from the other end: give each output an expression of its own, then derive combined values in a later `| let`. Splitting carries no materialization guarantee, so a stage's expression may still be evaluated more than once.

## Expressions

Expressions appear in `where`, `let` and aggregation arguments.

| Kind | Examples |
| --- | --- |
| Literals | `42`, `1.5`, `"string"`, `true`, `false`, `null` |
| Field references | `host`, `host.name`, `@timestamp`, `` `request id` `` |
| Arithmetic | `+`, `-`, `*`, `/`, `%`, unary `-` |
| Comparison | `==`, `!=`, `>`, `>=`, `<`, `<=` |
| Logical | `and`, `or`, `not` |
| Pattern | `matches`, `like`, `ilike` |
| Membership | `x in (1, 2, 3)` |
| Grouping | `(count + 1) * 2` |

Precedence runs from lowest to highest: `or`, `and`, `not`, the comparison and pattern operators, `+` and `-`, `*` and `/` and `%`, then unary `-`.

`matches` takes a `/regex/` literal or a string on its right. `like` and `ilike` take a SQL pattern, where `%` matches any run and `_` matches one character.

Expressions may nest 16 levels deep. A nesting level is a parenthesised sub-expression, a function call's arguments, or an `in (...)` list, so `abs((a + b) * 2)` is two levels. Deeper is a parse error naming the limit.

## Aggregation functions

| Function | Result |
| --- | --- |
| `count()` | Row count. |
| `count(field)` | Non-null count. |
| `avg(field)` | Mean. |
| `sum(field)` | Total. |
| `min(field)`, `max(field)` | Minimum, maximum. |
| `dc(field)`, `distinct_count(field)` | Distinct count. |
| `p50(field)`, `p90(field)`, `p95(field)`, `p99(field)` | Percentiles. |
| `median(field)` | Median value. |
| `stddev(field)` | Standard deviation. |
| `first(field)`, `last(field)` | First, last value. |
| `values(field)`, `list(field)` | List of distinct values. |

## Scalar functions

Scalar functions are available in `let`, `eval` and `where` expressions.

### String functions

| Function | Result |
| --- | --- |
| `lower(x)`, `upper(x)` | Lowercase, uppercase. |
| `length(x)`, `len(x)` | String length. |
| `trim(x)`, `ltrim(x)`, `rtrim(x)` | Whitespace trimming. |
| `replace(x, old, new)` | String replacement. |
| `substr(x, start[, len])` | Substring, 1-based and character-based. A negative `start` counts from the end, and a negative `len` is a leftward window. |
| `concat(a, b, ...)` | Concatenation of one or more arguments. |
| `contains(x, part)` | Whether `part` occurs in `x`. |
| `startswith(x, prefix)`, `endswith(x, suffix)` | Prefix and suffix tests. |
| `split(x, delimiter, index)` | One part of a split, 0-indexed. `index` must be an integer literal. |

### Numeric functions

| Function | Result |
| --- | --- |
| `abs(x)` | Absolute value. |
| `ceil(x)`, `ceiling(x)` | Round up. Always a DOUBLE, even over an integer, so `ceil(5)` is `5.0`. |
| `floor(x)` | Round down. Always a DOUBLE, like `ceil`. |
| `round(x[, n])` | Round to `n` decimal places. An integer argument stays integral, so `round(5)` is `5`, and a float stays a float, so `round(1.5)` is `2.0`. `n` must be an integer literal. |

### Conditional and type functions

| Function | Result |
| --- | --- |
| `if(cond, then, else)` | Ternary conditional. See [Conditions](#conditions). |
| `case(cond, then[, cond, then ...][, else])` | The first true arm's value. A trailing odd argument is the else value. |
| `isnull(x)`, `isnotnull(x)` | Null checks. |
| `coalesce(a, b, ...)` | First non-null value. |
| `typeof(x)` | The type name of the value the expression read: `"BIGINT"`, `"DOUBLE"`, `"VARCHAR"`, `"TIMESTAMP"` and the rest. |
| `now()` | The query's instant, timezone-naive UTC at microsecond resolution. See [now() and the unit of output](#now-and-the-unit-of-output). |
| `tonumber(x)` | Cast to float, mirroring `TRY_CAST AS DOUBLE`, and `null` on a parse failure. A boolean reads as `1.0` or `0.0`. |
| `tostring(x)` | Cast to string, and `null` for null or array input. |
| `sev(x[, dialect])` | The value's OTel SeverityNumber, or `null` when it has no reading. `dialect` is `"otel"` or `"syslog"`. See [Reading any field as a severity](#reading-any-field-as-a-severity-sev). |

#### Conditions

`if()` and `case()` read their condition the way DuckDB casts one to BOOLEAN, not as truthiness.

| Condition value | Reading |
| --- | --- |
| A boolean | Itself. |
| SQL `null` | The else branch. |
| A number | True when non-zero. Both zeros are false, and `NaN` is true. |
| A string | DuckDB's closed, case-insensitive vocabulary: `true`, `t`, `yes`, `y`, `1`, `false`, `f`, `no`, `n`, `0`. |
| Any other string | No reading, for `"nonempty"` and for `" true "` alike. The cast does not trim. |
| A timestamp or a list | No reading. Neither has a boolean cast. |

A condition with no reading has no answer, and the lanes part as they do for an [arithmetic overflow](#arithmetic). Batch SQL raises a conversion error and the query returns no rows, reporting `Could not convert string 'nonempty' to BOOL`, or `Unimplemented type for cast` for a timestamp or a list. The streaming lane yields `null` for the whole call, so `if("nonempty", 1, 2)` is `null` there.

`case()` reads its arms in order and stops at the first true one, so an unreadable condition after a match is never looked at. An unreadable condition reached before any match takes down the whole call in its lane's way, rather than skipping that arm.

`and`, `or`, `not` and the streaming `where` gate use a permissive predicate instead: `null`, `false`, zero, an empty string and an empty list are false, and every other value is true.

#### typeof and large integers

`typeof` returns the type name of the value the expression read. A wire integer above `i64::MAX` reads as a `DOUBLE`, so `typeof(request_id)` says `"DOUBLE"` for one. The value keeps its digits where identity matters: on the wire, in a `dedup` key, and in a `stats ... by` group.

#### now() and the unit of output

`now()` is read once per unit of output, not once per call site. Two `now()` reads in one statement are always equal, and `typeof(now())` is `"TIMESTAMP"`, never `"TIMESTAMP WITH TIME ZONE"`.

| Lane | Unit of output |
| --- | --- |
| Batch: `trawl query`, `/api/v1/query`, `/api/v1/export` | One instant per logical query invocation. A retried statement, the hot-only cold-start fallback and the kv batch tail all read the instant the first attempt read. |
| Live pass-through: SSE with no aggregation | Processing time, sampled once per event. Each row is frozen internally, and successive rows advance. `last=`, `earliest=` and `latest=` read that same per-event instant. |
| Aggregate streams: SSE over `stats`, `timechart`, `top` or `rare` | Stages before the aggregation read the source event's per-event instant. Every row of one emitted snapshot shares one instant, and the next snapshot advances. |

- The `last=` window in a batch query evaluates on DuckDB's statement clock, a separate clock from `now()`, and the gap between the two reads is unbounded.
- Wall-clock sampling carries no monotonic guarantee, so an NTP step backward can put a later event's instant before an earlier one's.
- An SSE reconnect is a new subscription with a fresh clock, and nothing carries across it.
- `strftime(now(), ...)` and `tostring(now())` return text. The display timezone offset applies to TIMESTAMP result cells only, and a value the kv batch tail computed renders as UTC text.

### JSON functions

Nested objects and arrays are stringified at ingest, so a field such as `k8s: {"pod": "x", "ns": "default"}` is stored as JSON text in a `VARCHAR` column, never as a `STRUCT`. Bare text search matches inside that text, which is part of both the column and `_raw`.

| Function | Result |
| --- | --- |
| `json_extract_string(x, path)`, `json(x, path)` | The contents of a JSON string, unquoted: `"x"` becomes `x`. |
| `json_extract(x, path)` | The value's JSON text. A string keeps its quotes, a number, boolean or `null` is its own text, and an array or object is compact JSON. |
| `json_valid(x)` | Whether `x` parses as JSON. |
| `json_keys(x)` | The object's keys. |
| `json_array_length(x)` | The array's length. |

```
# reach into a stringified object
service=kubelet | eval pod = json_extract_string(k8s, "$.pod") | where isnotnull(pod)
```

On numbers of exotic magnitude, the streaming lane re-renders a number from its `f64` where the query engine renders the source spelling. A value written `1e16` through `1e20`, or an integer with more digits than a 64-bit one holds, can come back spelled differently, as `1e20` against `100000000000000000000`. Values inside those bounds render identically in both lanes.

## Date and time functions

Date and time functions operate on timestamp values. `_time` is canonicalized to UTC at ingest and stored as a timezone-naive `TIMESTAMP`, so an input `12:00:00+05:30` becomes `06:30:00Z`. The bare sender field `timestamp` is a separate field with its own catalog pin. See [time derivation](/reference/events/#time-derivation) for accepted event-time encodings and [catalog conformance](/architecture/catalog/#write-time-conformance) for custom TIMESTAMP fields.

| Function | Result |
| --- | --- |
| `date_part(unit, ts)` | A calendar component, as an integer, or a float for `epoch`. |
| `date_trunc(unit, ts)` | The start of the period, as a timestamp. |
| `date_diff(unit, start, end)` | Calendar-unit boundaries crossed, as `end - start`. |
| `strftime(ts, fmt)` | The timestamp formatted as a string. |
| `strptime(str, fmt)` | The string parsed as a timestamp, or `null` on failure. |

The argument order for `strftime` is `(timestamp, format)`, the opposite of C `strftime`. `strptime` returns `null` for a value that does not parse against the format, in both lanes, so one unparseable value is not a query error.

### The unit allowlist

The `unit` argument must be a string literal from the allowed set. A non-literal expression and an unlisted unit are refused before execution, in both lanes.

| Function | Allowed units |
| --- | --- |
| `date_part` | `year`, `quarter`, `month`, `week`, `day`, `hour`, `minute`, `second`, `dow`, `doy`, `epoch` |
| `date_trunc` | `year`, `quarter`, `month`, `week`, `day`, `hour`, `minute`, `second` |
| `date_diff` | `year`, `quarter`, `month`, `week`, `day`, `hour`, `minute`, `second` |

`dow` is the day of the week, Sunday 0 through Saturday 6. `doy` is the day of the year, 1 through 366. `epoch` is seconds since the Unix epoch, as a float. `date_trunc("week", ts)` truncates to Monday midnight, the ISO 8601 week start.

`date_diff` counts boundary crossings for every unit except `week`. For `week`, DuckDB computes the whole number of days between the dates divided by 7, with integer division toward zero.

### Comparing a timestamp against text

A timestamp compares against a string by reading the string as a timestamp. That cast takes the wall-clock components and discards any offset, so `'...T09:00:00+05:30'` is 09:00.

Text with no reading makes the comparison `null`. There is no fallback to string ordering, so `t < "zzz"` is unknown rather than true, and a malformed offset such as `+ab:cd` matches nothing. The streaming lane reads less than the query engine, never more: it declines zone names beyond `UTC`, and years outside chrono's calendar.

### Infinity timestamps

`infinity` and `-infinity` are values a timestamp can hold, and they read from text like any other instant. They order below and above every date, and render as their own words through `tostring()`.

| Function | Answer for an infinity |
| --- | --- |
| `date_part(unit, ts)` | `null`, for every unit including `epoch`. |
| `date_trunc(unit, ts)` | The infinity, unchanged, for every unit. |
| `date_diff(unit, a, b)` | `null` whenever either side is infinite. |
| `strftime(ts, fmt)` | `infinity` or `-infinity`, whatever the format asks for. |

### Format codes

Standard C `strftime` codes such as `%Y`, `%m`, `%d`, `%H`, `%M` and `%S` produce identical output in both lanes. An invalid code, such as `%Q` or a trailing `%`, is refused before execution in both lanes when the format is a string literal.

- `%Z`, `%z`, `%c`, `%x` and `%X` depend on timezone or locale. The streaming lane operates on a timezone-naive timestamp, so these render empty or fixed and can differ from batch SQL. Use explicit numeric codes for portable output.
- `%f` is a six-digit microsecond field in both lanes, so `strftime(ts, "%f")` over `...09:00:00.5` gives `500000`, and a zero fraction gives `000000`. The streaming lane translates a bare `%f` to chrono's `%6f` and leaves an escaped `%%f` as the literal it is.
- On input, batch SQL reads a fraction of any length, so `.5` is half a second, while the streaming lane reads exactly six digits and yields `null` for anything else.

`strptime` fills the components a format omits from a `1900-01-01 00:00:00` base, identically in both lanes. A date-only format such as `%Y-%m-%d` yields midnight, and a time-only format such as `%H:%M:%S` yields the base date. A partial date fills the rest: `%Y` gives `2023-01-01 00:00:00`, `%Y-%m` gives `2023-11-01 00:00:00`, `%m-%d` gives `1900-11-07 00:00:00`, and `%Y-%m-%d %H` gives the named hour with zero minutes and seconds.

Two codes follow chrono in the streaming lane and can differ from batch SQL. A bare two-digit year, `%y`, yields `null` where batch SQL fills the base year, though `%y-%m-%d` resolves through chrono's pivot and matches. `%Z` and `%z` keep chrono's wall-clock time rather than normalizing to UTC.

## Batch and streaming differences

Four lanes answer a query: batch SQL, the live stream (SSE), the kv batch tail behind `extract kv`, and embedded `--data`. The comparison rules are identical in all of them. Stage support is not.

| Stage | Live stream | kv batch tail |
| --- | --- | --- |
| `where`, `let`, `table`, `fields`, `drop`, `rename`, `limit`, `head`, `tail`, `dedup`, `extract` | Supported | Supported |
| `stats`, `timechart`, `top`, `rare` | One aggregation stage per query | Supported |
| `sort` | Refused: `contradicts real-time arrival order` | Supported |
| `pivot` | Refused: `dynamic column structure breaks progressive rendering` | Refused |
| `sample` | Refused: `statistical sampling requires the full dataset` | Refused |
| `eventstats` | Refused: `window functions require the full dataset` | Refused |
| `from saved` | Refused: `saved query sources are not supported in streaming mode` | Refused |

- A live stream admits one aggregation stage. A second one is refused with `only one aggregation stage is supported in streaming mode`.
- Where batch SQL raises a per-value error, the streaming lane yields `null` instead. This covers an integer overflow and an unreadable `if()` or `case()` condition. A live tail cannot raise a per-event error without ending the subscription.
- Embedded `--data` has no field catalog, so every comparison there is literal-driven. `sev()` still declares its own type.

## Examples

```
# errors in the last hour, by service
_severity>=error last=1h | stats count() by service | sort -count
# slow requests by endpoint
status=200 last=24h | where duration > 1000 | stats avg(duration) by uri | sort -avg_duration | head 10
# 4xx and 5xx rates by host
status>=400 last=2h | stats count() by host, status | where count > 10
# extract IP addresses and count them
"connection from" | rex "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message | stats count() by ip | sort -count
# a time series of the error rate
_severity>=error | timechart span=5m count() by service
# collapse flapping alerts
service=monitoring | dedup host, alert_name
# status codes as columns, hosts as rows
last=1h | pivot count() on status by host
# a computed field with null handling
* | eval msg_len = if(isnotnull(message), length(message), 0) | fields host, msg_len | head 10
# distinct values per group
* | stats values(_severity), first(message) by service | head 10
```
