---
title: Query Language (DSL)
description: Complete reference for trawl's pipeline query language.
---

trawl uses a pipeline-oriented query language inspired by Splunk's SPL. Queries flow through stages separated by `|`, with each stage transforming the data for the next.

## Query structure

```
[search stage] | [pipe stage] | [pipe stage] ...
```

The search stage is optional. Pipelines can start with `|` for raw log access.

## Search stage

The search stage filters events before they enter the pipeline. Multiple conditions within a group are AND-joined.

### Field filters

```
service=nginx                    # exact match
status=200,301,404              # IN list (comma-separated)
status>=400                     # comparison
path=/api/*                     # glob pattern
message=/error.*/               # regex pattern (slash-delimited)
host="db host"                  # quoted values for spaces/special chars
env=prod                        # environment (path-pruned)
```

**Operators:** `=`, `!=`, `>`, `>=`, `<`, `<=`

#### Pinned comparison semantics

On a server, every stored field carries a type pin in the field catalog
(the envelope columns are pinned on install; custom fields pin at first
typed sight). Two rules follow, and they are one rule seen from either
end:

- **What is stored** is the value's reading under the pin — but only when
  that reading is *value-preserving*. A cast that would silently alter the
  value is refused: the column holds NULL, the disagreement is recorded as
  a conflict (`trawl schema conflicts`), and the original text stays
  findable in `_raw`. Spelling drift is not alteration, so `"0404"` under
  a BIGINT pin stores as `404`, but `"1.5"` stores as NULL rather than
  rounding to `2`.
- **What matches** is decided by the pin, not by the shape of the query
  literal, so a comparison means the same thing however you spell it.

An event answers the same way the moment it lands as it will hours later:
the hot buffer conforms through the identical expression compaction writes
with. Live tail (SSE) applies the same rules again, so a streamed query,
its batch form, and the same query after the compactor runs all agree
event for event.

##### The numeric reading

Both VARCHAR-pinned numeric rungs below read the column *and* the literal
through the same `DECIMAL(38,6)` cast, so the two sides can never disagree
about what a string means. That space is **exact for every 64-bit
integer** and out to 10^32 — `id=1737000000123456789` matches that id and
no neighbour.

What has a reading: surrounding whitespace is ignored (`" 200"` is 200),
`_` between digits is a separator (`"200_000"` is 200000), leading zeros
are fine (`"0404"` is 404), and `"+5"`, `"1."`, `".5"`, `"1e3"` and
`"1E-3"` all read.

What has **no** reading — and no reading means *unknown*, never a false
match, and `NOT` cannot invert it into one: `"nan"`, `"inf"`,
`"infinity"`, radix prefixes like `"0x10"`, grouped digits like
`"1,000"`, empty or blank text, and any magnitude at or above 10^32.
Fractions quantize at 10^-6 (rounded half away from zero), so two values a
nanosecond apart compare equal — sub-microsecond ordering is not something
a VARCHAR-pinned field can express.

##### The rules, per pin

- **VARCHAR-pinned field, `=` / `!=` / IN list** — compares **as text**:
  `status=accepted` matches the stored string `"accepted"`, exactly. A
  *numeric* literal matches the exact text **or** any spelling of the
  same number: `status=200` finds `"200"`, `"0200"` and `"200.0"`, but
  not `"accepted"` or `"404"`. The numeric half is not optional — the
  text a number is stored under depends on the batch it arrived in (one
  fractional value anywhere in the batch stores `200` as `"200.0"`), and
  live tail must answer the same as a batch query. `!=` is the exact
  complement: `status!=200` returns `"accepted"` and every other
  non-200 value. A literal the numeric space can't read (`status=nan`,
  `id=1e40`) falls back to the text comparison alone.
- **VARCHAR-pinned field, ordered comparison with a numeric literal** —
  compares **numerically** in that same space: `status>=400` matches
  `"404"`/`"500"`, and values with no numeric reading like `"accepted"`
  simply don't match (they never error the query). Such a value is
  *unknown*, not *false*, exactly as in SQL — so `NOT status>=400`
  doesn't match those rows either. A *literal* with no reading
  (`dur>1e40`) matches nothing at all, on either side.
- **VARCHAR-pinned field, ordered comparison with a non-numeric
  literal** — lexical string comparison, unchanged.
- **Integer-pinned field, glob or regex** — matches the **stored
  integer's** text form, which is not always how the event spelled it:
  `status=4*` finds 404 in a BIGINT column, and a wire `"0404"` is stored
  as 404, so it matches `status=4*` and *not* `status=0*`. The same holds
  for `"4.0"`, `" 200"`, `"200_000"` and `"1e3"` — value-preserving
  spellings, stored as the integer. A value the cast would *rewrite* —
  `"1.5"` (rounded to 2), or `"0x10"` (read as 16, but a spelling DuckDB
  would never write back) — and one it can't read at all (`"accepted"`)
  are stored as NULL and are *unknown*, not false.
- **Boolean-pinned field, glob or regex** — matches `true` or `false`,
  lowercase, and only genuine ones. DuckDB's boolean vocabulary is wider
  than what it writes back (`"TRUE"`, `"t"`, `"yes"`, `"1"` are all inside
  the cast), but none of those survive the round trip, so they are stored
  as NULL and counted as conflicts — the stored column holds exactly the
  values the wire spelled `true` or `false`. `flag=/^true$/` matches
  those; `flag=TRUE*` matches nothing. A NULL is *unknown*, not false.
- **Double-pinned field, glob or regex** — matches the value's text form
  too, but a double's text form is not what the event's JSON looked like:
  it always carries a fraction, and switches to a signed, two-digit
  exponent outside `1e-4 … 1e16` (`200.0`, `0.0`, `-3.0`, `1e-07`,
  `1.2345678901234568e+17`). So `dur=/^200$/` matches nothing while
  `dur=/^200\.0$/` matches — the same on both sides, batch and live.
  Pinning DOUBLE *means* accepting DOUBLE's precision, so this is the one
  rung with no round-trip guard: anything the cast reads is stored. A
  value with no numeric reading is stored as NULL and is *unknown*, not
  false.
- **Timestamp-pinned field, glob or regex** — matches the **RFC 3339
  UTC-microsecond form** of the instant, always with a `T` separator, six
  fractional digits and a trailing `Z`
  (`2026-01-15T09:00:00.000000Z`). The parse is **zone-aware**: an offset
  in the wire text is *applied*, so `2026-01-15T09:00:00+05:30` is stored
  and matched as `2026-01-15T03:30:00.000000Z` — `_time=/T03:30/`, not
  `/T09:00/`. Text without an offset is read as UTC, a bare date is
  midnight, and fractions truncate at six digits. So `_time=2026-01-15*`,
  `_time=/T09:/` and `_time=/\.123456Z$/` all mean the same thing in a
  batch query and in live tail. A value with no timestamp reading is
  stored as NULL and is *unknown*, not false — `NOT _time=/T09:/` doesn't
  match it either. (The envelope's own `_time`/`_ingested` are already
  canonicalized to UTC at ingest, so this only changes how a *custom*
  timestamp-pinned field reads.)
- **Typed-pinned field (BIGINT / DOUBLE / BOOLEAN / TIMESTAMP),
  comparison** — compares what the column **stores**, which is the
  conformed value: a wire `1.5` under a BIGINT pin is NULL there, so
  `duration>1` does not match it, `NOT duration>1` does not either
  (*unknown*, not false), and `duration!=2` does — a NULL column matches
  `!=` by design. The literal binds exactly as on an unpinned field
  (the column already has the pinned type) and DuckDB reads it against
  that type, so `flag=TRUE` and `flag=yes` both match a stored `true`
  even though those same texts *stored* conform to NULL, and an offset
  spelled in a timestamp literal is ignored where the same offset in a
  stored value is applied. Live tail answers the same way, event for
  event.
- Every comparison on an **unpinned** field keeps plain literal-driven
  behavior.

:::caution[Changed in the ADR-0011 release]
The zone-aware timestamp parse applies to values conformed **from this
release onward**. A custom timestamp field that received offset-bearing
values before it may hold wall-clock instants in already-compacted
partitions; those are not rewritten, so a glob over such a field can span
both readings until the old partitions age out.
:::

#### Missing fields and nulls

A field an event doesn't carry is a NULL column, and a comparison against
NULL is **unknown** — neither true nor false. This is SQL's rule and it
holds everywhere: batch queries, exports, and live tail, on pinned and
unpinned fields alike. Only a *true* row is returned, so an unknown one is
filtered out. Two consequences are worth knowing before you write an alert:

- `f!=x` **matches events that carry no `f`** (and events whose `f` is
  null). Its emitted form is `("f" != ? OR "f" IS NULL)` — the one total
  comparison. On live tail that is a wide net: bus events carry the
  envelope plus whatever their sender sent, so `f!=x` over a sparse custom
  field streams nearly everything. Pair it with `f=*` to require the field.
- `NOT f=x` **does not match events that carry no `f`** — `NOT (NULL)` is
  NULL, which is unknown, which is filtered out. If you want "events
  missing `f`, plus events where it isn't `x`", write `f!=x`, not
  `NOT f=x`. The same holds for `NOT _severity=...` when an event has no
  derived severity, and for `NOT <bare term>` when it has no
  `message`/`_raw`.

:::caution[Changed in the ADR-0011 release]
Live tail previously treated a missing field as *false* rather than
unknown, so `f!=x` matched nothing on such events and `NOT f=x` matched
all of them — the opposite of what the same query returned from
`/api/v1/query`. Live tail now agrees with the batch answer. Alerts built
on `NOT f=x` to catch events missing a field need rewriting as `f!=x`.
:::

Two deliberate boundaries:

- **Numeric-literal detection is by content, not quoting**: the parser
  discards quote provenance, so `status>"400"` and `status>400` are the
  same query — in the search stage and in `| where` alike (`where
  status == "400"` binds exactly as `where status == 400`).
- **Embedded mode (`--data`) stays literal-driven** — there is no
  catalog behind `--data`, so every comparison keeps its pre-catalog
  behavior there.

#### Pin-aware `| where` and `| let` (ADR-0011 slice A′)

A **bare field-vs-literal comparison** inside `| where` or `| let`
consults the same catalog pin the search stage does, and adopts exactly
the same rule table: on a VARCHAR pin `where status > 400` compares in
`DECIMAL(38,6)` (a value with no reading is unknown, never an error),
`where status == 200` matches the stored text *or* its numeric reading,
`where status in (200, "accepted")` routes each element through the
equality rule; pattern operators (`matches`, `like`, `ilike`) against a
typed pin target the same canonical text form globs do. Both operand
orders bind (`400 < status` is `status > 400`); for pattern operators
only the *left* operand is a subject — the right operand is the pattern.
Live tail (SSE) and the batch tail behind `extract kv` evaluate the same
rules, so a pipeline means one thing in every lane.

**Excluded shapes** stay literal-driven, structurally: field-vs-field
(`where a == b`), function-wrapped fields (`where lower(status) == "a"`),
arithmetic on the field (`where status * 2 > 400`), `== null`, and a
pattern with the field on the right (`where "x" matches f`). Unpinned
fields are unchanged everywhere.

**Which pin applies follows the pipeline**, not a flat name lookup:

- `rename status as st | where st > 400` is pin-aware under the new name
  (and `status` no longer resolves the pin).
- `let status = <expr> | where status > 400` is literal-driven — a
  computed value has no pin. A **bare alias** copies the pin:
  `let s2 = status | where s2 > 400` is pin-aware. Within one `let`, a
  reference to a sibling target resolves the way the SQL does — an
  existing *column* of that name wins (so `let a = 1, b = a` gives `b`
  the original `a`, not `1`), and only a name the row doesn't carry reads
  the sibling's freshly computed value (`let ms = 1000, total = ms * 2`
  gives `total = 2000`). *Pins* stay strictly pre-stage either way, so
  `let a = status, b = a` gives `b` no pin.
- Aggregations keep their group-by keys and nothing else:
  `stats count() by status | where status == 200` stays pin-aware, while
  aggregate outputs (`count`, aliases, the `timechart` time bucket) are
  never pinned.
- `extract <regex>` unpins its capture-group names; `extract kv` passes
  the scope through — with one caveat: a kv key that *shadows* a pinned
  field name is read under that pin. Confine kv extraction to fields
  that don't collide with pinned names if that matters.
- `from saved` reads another query's output: no pins apply.

**Timestamps in the `extract kv` tail** are compared as stored instants
and shifted to your display timezone *last*, like every other lane. The
shift follows the tail's own lineage: `rename _time as t` and a bare
alias `let t2 = _time` still render in your zone, while a *computed*
value (`let t = coalesce(_time, x)`, aggregate outputs) is a value the
tail derived and renders as UTC text.

**One NULL-policy difference from the search stage, kept on purpose**:
the pipeline `!=` does *not* carry the search stage's `OR field IS NULL`
widening. `| where f != x` over an event without `f` is unknown and
filtered — exactly what the pin-blind `| where` always answered — so a
repin never changes missing-field semantics. Use the search-stage `f!=x`
when you want the missing-field net.

:::caution[Changed in the ADR-0011 slice A′ release]
`| where` and `| let` comparisons on pinned fields change answers:
`| where status > 400` over a VARCHAR pin stops raising a Conversion
error and starts filtering; `== 200` gains the numeric arm (it now
matches a stored `"200.0"`); on TIMESTAMP pins live-tail ordered
comparisons become the instant comparison batch always performed instead
of lexical text. The envelope seed pins `host`/`service`/`env`/`message`/
`_raw` as VARCHAR (and `_severity` as SEVERITY) on every install, so this
is live on day one.
:::

### The two namespaces

One sentence, learned once (ADR-0013):

- **Bare names are your data.** `service`, `host`, `status`, `level`,
  `timestamp` — whatever your senders emit, stored verbatim under the
  name they sent. trawl never assigns meaning to a bare name.
- **Underscore names are trawl's.** `_time`, `_ingested`, `_raw`,
  `_repairs`, `_severity` are contract slots whose semantics trawl
  guarantees on every corpus. The whole `_` prefix is reserved: an
  incoming `_x` that is not a slot you may propose has its leading
  underscores stripped and lands under the bare remainder (`_HOSTNAME` →
  `hostname`), and the DSL cannot mint one either — `let _foo = 1`,
  `rename x as _foo` and `extract "(?P<_foo>…)"` are errors.

There are **no aliases**. The name you type is the column in `DESCRIBE`
is the identifier in the SQL, in every lane.

### Backtick-quoted names

Any field name can be written between backticks, and a backticked name is
**always** a field reference:

```
`http-status`=500                # a name the bare form cannot spell
`last`=5                         # the FIELD last, beside last=2h
| table `request id`, `x-request-id`
| stats count() as `total count` by `where`
| table `a``b`                   # a doubled backtick escapes one: the name a`b
```

Backticks change how a name is **lexed**, never what a name may be:

- content is any character except a backtick, and a doubled backtick
  escapes one (last line above);
- an empty name is a parse error, as is one carrying a character that
  cannot render as itself — a control character, a bidi or zero-width
  format character, or the soft hyphen;
- the ASCII fold still applies: `` `Dur` `` **is** `dur`;
- trawl's `_` namespace is still sealed: ``let `_foo` = 1`` and
  ``rename x as `_foo` `` are the same errors as the bare spellings.

They are accepted in every field position — search-stage filters,
expressions, aggregation arguments and `by` keys, `table`/`fields`,
`sort`, `drop`, `dedup`, both sides of `rename`, `let` targets and `as`
aliases — so a name that exists is a name you can reach.

**Not fields, so no backticks:** function names (`` `lower`(x) `` is a
field reference, never a call), stage names, and saved-query names.

The search stage reads exactly three words before anything else —
`last=`, `earliest=`, `latest=`. Backticks are how you reach fields with
those names; everywhere else they are ordinary names already.

Inside an expression (`| where`, `| let`, aggregation arguments) the
words an expression is made of are read before a field reference is
tried: `true`, `false` and `null` are literals, and `and`, `or`, `not`,
`in`, `matches`, `like`, `ilike` are operators. Backticks reach the
fields — ``| where `true` == 1`` filters on the column named `true`,
while `| where true == 1` compares the boolean.

A `#` or `//` inside backticks is part of the name, not a comment. The
comment stripper opens a name only where one could start and only when
the region would really lex as one, so a backtick inside a value — a
regex literal or a glob — hides nothing. (A stray backtick that does sit
at a token start and finds a partner on the same line still has the same
shape of consequence as a `#` inside a regex literal: it can hide a later
comment from the stripper.)

### Severity: `_severity`

`_severity` is the derived OTel SeverityNumber (1-24), pinned to the
`SEVERITY` type on every install. Because the pin types the comparison,
the token vocabulary works identically in the SQL emitter, the live
filter, the stream compiler and the post-SQL tail:

```
_severity=error                 # BETWEEN 17 AND 20 (the ERROR band)
_severity!=info                 # NOT BETWEEN 9 AND 12, or _severity IS NULL
_severity=warn,error            # either band
_severity>=warn                 # >= 13 (the token's exact number)
_severity=error2                # exactly 18 (the OTel exact short name)
_severity=17                    # exactly 17
_severity=warn*                 # glob over the canonical token text: 13-16
| where _severity == "error"    # the same rule, in a pipe stage
```

- Equality and IN match the whole **band** containing the token
  (`notice` falls inside the INFO band); ordered comparisons use the
  token's **exact** number.
- OTel's exact short names (`trace2`, `warn3`, `error2`, …) name one
  number, under every operator.
- Glob and regex match the **canonical token text** — the injective OTel
  short name of the stored number — which is also what results display:
  a `_severity` cell reads `error`, not `17`. The wire keeps the number:
  `-f json`, `-f csv` and SSE carry it for arithmetic consumers.
- An unrecognized value is a **query error naming the vocabulary**, never
  a filter that quietly matches nothing.
- Valid band tokens: `trace`/`t`, `debug`/`d`, `info`/`i`, `notice`,
  `warn`/`warning`/`w`, `error`/`err`/`e`, `fatal`/`critical`/`crit`/`f`,
  `alert`, `emerg`/`panic`.
- Embedded `--data` mode has no catalog, so it has no `SEVERITY` pin:
  compare the ladder number there (`_severity>=17`).

`_severity` is **derived, never proposed**: ingest reads `severity` →
`severity_text` → `level` (first mappable wins) and stores every one of
them verbatim as your own columns. A word maps through the token table
or the exact names; a number maps strictly as OTel 1-24, so `3` is
`trace` — the syslog inversion happens only in the syslog listener,
where the transport proves the dialect. An event with no mappable source
simply has no `_severity`.

:::caution[`level=error` is not a severity filter]
`level` is an ordinary field now, so `level=error` compares the sender's
own value. A game server emitting `{"service":"game","level":"gold"}`
keeps a fully queryable `level` column — that is the point — but if you
meant severity, you want `_severity>=error`. And a field no sender writes
is not an error: it simply matches nothing, so a query written against
the old alias comes back empty rather than failing. trawl says nothing
about it — `level` is your vocabulary, not trawl's, and a notice keyed on
the name would be trawl assigning it a meaning again.
:::

### `timestamp` and `@timestamp`

Ordinary sender fields. They are **read** as sources for the `_time`
derivation (`_time` → `timestamp` → `@timestamp`, first present wins)
and **stored verbatim** under their own names, so both the canonical
instant and what the sender actually sent stay queryable. Only `_time`
itself is consumed and canonicalized — it is the proposal slot.

They are no longer aliases for `_time`, so `| sort -timestamp` sorts the
sender's column and finds nothing where no sender sends one — silently,
exactly as `level` does. Sort, filter and project `_time` when you mean
the event's instant.

### Text search

```
error                           # bare word — substring match
-debug                          # negated — exclude matches
"connection refused"            # exact phrase
```

Bare-word and phrase search match the `message` column **and** `_raw`,
so content that was parsed away is still findable. Negation excludes an
event when either column matches.

Because `_raw` holds the most original form of the event, searching it is
**whole-event search**, and what "whole event" means depends on who filled
it:

- a collector that sent its own string `_raw` — the pre-parse line, matched
  as text;
- everything else — the server's JSON serialization of the event as it
  arrived, so a term matches anywhere in that object: another field's
  **value** (`nginx` finds an event with `service=nginx`, even when
  `message` never says it) and a field **name** (`debug` finds an event
  carrying `debug_mode`, and `-debug` therefore excludes it).

That is the point of a bare word — find the event without knowing which
field holds the term. When you do know, filter the field and `_raw` is
never consulted:

```
message=/debug/                 # regex, message only
message=*debug*                 # glob, message only (case-sensitive)
service=nginx                   # exact field match
```

### Time filters

```
last=2h                         # units: s, m, h, d, w
last=7d
last=30m
earliest="2026-03-14T03:00:00Z" # absolute lower bound (quoted)
latest="2026-03-14T03:15:00Z"   # absolute upper bound (quoted)
```

`last=`, `earliest=` and `latest=` are the DSL's whole keyword set: they
are read before any field filter, wherever they appear, and they apply to
the query globally. Fields of those three names are reachable with
backticks (`` `last`=5 ``).

### OR grouping

```
service=nginx OR service=apache
a b OR c d                      # implicit AND within groups: (a AND b) OR (c AND d)
```

## Pipe stages

### stats

Aggregate with optional grouping.

```
stats count()
stats count() by host
stats avg(duration) by status
stats count(), avg(duration) by service, host
stats avg(duration) as avg_duration
```

### where

Filter on computed values.

```
where count > 10
where avg_duration > 100
where status == 200 and count > 5
where host matches /prod-.*/
where x in (1, 2, 3)
```

### sort

Order results. Prefix with `-` for descending.

```
sort count                      # ascending
sort -count                     # descending
sort status, -count             # multi-field
```

### limit / head

Cap the number of results.

```
limit 20
head 20                         # alias for limit
```

### tail

Last N rows. Defaults to timestamp descending if no prior sort.

```
tail 5
```

### table / fields

Select output columns.

```
table host, status
fields host, status             # alias for table
```

### top

Most frequent values.

```
top 10 host                     # top 10 hosts by frequency
top 5 status by service         # top 5 statuses per service
```

### rare

Least frequent values.

```
rare 5 status                   # 5 rarest status codes
rare 3 host by service          # 3 rarest hosts per service
```

### drop

Exclude columns from output.

```
drop message
drop host, raw
```

### let / eval

Create computed fields.

```
let duration_ms = duration * 1000
eval status_class = status / 100    # eval is an alias for let
let is_error = status >= 400
```

### extract / rex

Extract fields from text using regex or key-value parsing.

**Named capture groups:**

```
extract "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message
rex "(?P<code>[A-Z]+)" from raw    # rex is an alias for extract
```

**Key-value extraction:**

```
extract kv                      # from 'message' field
extract kv from raw             # from a specific field
```

A kv key starting with `_` is dropped rather than extracted: the `_`
namespace is trawl's, and a key parsed out of a log line is sender-controlled
text, so `_severity=17` in a message body would otherwise forge trawl's own
verdict slot. The text stays findable in the source field and `_raw`.

### rename

Rename output columns.

```
rename service as svc
rename service as svc, host as hostname
```

### dedup

Remove duplicate rows. Keeps the most recent.

```
dedup                           # entire row
dedup host                      # by single field
dedup host, service             # by multiple fields
```

### timechart

Time-bucketed aggregation.

```
timechart span=5m count()
timechart span=1h count() by service
timechart span=30s count(), avg(duration)
```

### pivot

Pivot table transformation.

```
pivot count() on status
pivot avg(duration) on service by host
```

### Output names must be unique

Every aggregating stage projects a fixed set of columns: its group keys,
`timechart`'s `_time` bucket, the `count` column `top`/`rare` mint, and
one column per aggregate — named by its `as` alias, else `count`,
`avg_duration`, `dc_host`. An un-aliased aggregate over a computed
argument names its innermost field (`avg(tonumber(rssi) * -1)` →
`avg_rssi`); the in-memory lanes — live tail, and the batch tail behind
`extract kv` — refuse that argument outright, because their accumulators
read a bare field out of the event rather than evaluating an expression.
Two producers naming one column is refused before the query runs, in
batch and in a live tail alike, with both producers named:

```
| stats count() by count        # the group key `count` and the aggregate count()
| stats count() as n, sum(x) as N   # `n` and `N` are one name (names fold)
| timechart span=1h count() by _time  # the bucket is always `_time`
| top 5 count                   # `top` mints its own `count` column
```

The remedy is usually `as`: `| stats count() as hits by count`. `top` and
`rare` cannot spell `as`, so write them out —
`| stats count() as hits by count | sort -hits | head 5`.

`eventstats` adds its column to every row and therefore **requires** an
explicit `as`: a live tail cannot know a row's schema before the rows
arrive. An alias naming a column the rows already carry overwrites it,
the way `let` does.

```
| eventstats avg(duration) as avg_dur by service
```

## Expressions

Used in `where`, `let`, and aggregation arguments.

| Type | Examples |
|------|----------|
| Literals | `42`, `1.5`, `"string"`, `true`, `false`, `null` |
| Field refs | `host`, `host.name`, `@timestamp` |
| Arithmetic | `+`, `-`, `*`, `/`, `%` |
| Comparison | `==`, `!=`, `>`, `>=`, `<`, `<=` |
| Logical | `and`, `or`, `not` |
| Pattern | `matches` (regex) |
| Membership | `x in (1, 2, 3)` |
| Grouping | `(count + 1) * 2` |

## Aggregation functions

### Basic stats

| Function | Description |
|----------|-------------|
| `count()` | Row count |
| `count(field)` | Non-null count |
| `avg(field)` | Mean |
| `sum(field)` | Total |
| `min(field)` | Minimum |
| `max(field)` | Maximum |

### Cardinality

| Function | Description |
|----------|-------------|
| `dc(field)` | Distinct count |
| `distinct_count(field)` | Alias for `dc` |

### Percentiles

| Function | Description |
|----------|-------------|
| `p50(field)` | Median (50th percentile) |
| `p90(field)` | 90th percentile |
| `p95(field)` | 95th percentile |
| `p99(field)` | 99th percentile |

### Positional and collection

| Function | Description |
|----------|-------------|
| `first(field)` | First value |
| `last(field)` | Last value |
| `values(field)` | List of distinct values |
| `list(field)` | Alias for `values` |
| `median(field)` | Median value |
| `stddev(field)` | Standard deviation |

## Scalar functions

Available in `let`/`eval` and `where` expressions.

### String functions

| Function | Description |
|----------|-------------|
| `lower(field)` | Lowercase |
| `upper(field)` | Uppercase |
| `length(field)` / `len(field)` | String length |
| `trim(field)` / `ltrim(field)` / `rtrim(field)` | Whitespace trimming |
| `replace(field, old, new)` | String replacement |
| `substr(field, start[, len])` | Substring extraction |

### Numeric functions

| Function | Description |
|----------|-------------|
| `abs(x)` | Absolute value |
| `ceil(x)` / `ceiling(x)` | Round up |
| `floor(x)` | Round down |
| `round(x[, n])` | Round to n decimal places |

### Conditional and type functions

| Function | Description |
|----------|-------------|
| `if(cond, then, else)` | Ternary conditional |
| `isnull(x)` / `isnotnull(x)` | Null checks |
| `coalesce(a, b, ...)` | First non-null value |
| `typeof(x)` | Value type name (returns `"VARCHAR"`, `"DOUBLE"`, `"TIMESTAMP"`, …) |
| `now()` | Current timestamp (timezone-naive, wall-clock UTC) |
| `tonumber(x)` | Cast to float (`null` on parse failure — mirrors `TRY_CAST AS DOUBLE`) |
| `tostring(x)` | Cast to string (`null` for null/array input) |

### Nested fields (JSON)

Nested objects and arrays are stringified at ingest: a field like `k8s: {"pod": "x", "ns": "default"}` is stored as its JSON text in a `VARCHAR` column (the field catalog pins it as such), never as a `STRUCT`. Reach into it with `json_extract_string`:

```
service=kubelet | eval pod = json_extract_string(k8s, "$.pod") | where isnotnull(pod)
```

Bare text search also matches inside the stringified value (it is part of `_raw` and of the column's text).

### Date and time functions

Date/time functions operate on **timestamps** — the `timestamp` field is stored as a timezone-naive `TIMESTAMP` in both the batch and streaming paths. Ingest first canonicalizes the value to UTC, so an incoming offset is *applied* (`12:00:00+05:30` becomes `06:30:00Z`) rather than dropped in favour of its wall-clock components; the naive timestamp everything downstream sees is therefore UTC. See [Data flow](/architecture/data-flow/#timestamp-canonicalization) for the accepted input grammar.

| Function | Description |
|----------|-------------|
| `date_part(unit, ts)` | Extract a calendar component (returns integer or float for `epoch`) |
| `date_trunc(unit, ts)` | Truncate to start of period (returns timestamp) |
| `date_diff(unit, start, end)` | Count calendar-unit boundaries crossed (`end - start`); `week` is the exception (see note below) |
| `strftime(ts, fmt)` | Format timestamp as string (chrono `%`-codes) |
| `strptime(str, fmt)` | Parse string to timestamp (returns `null` on failure) |

**Note:** the argument order for `strftime` is `(timestamp, format)` — the opposite of C `strftime`. DuckDB's `STRFTIME` is overloaded and accepts this order directly, so it is emitted unchanged.

**Note:** `strptime` returns `null` when a value cannot be parsed against the format — a single unparseable value yields `null`, not a query error. This holds in both the batch path (emitted as DuckDB `TRY_STRPTIME`) and the streaming path.

#### Date/time unit allowlist

The `unit` argument to `date_part`, `date_trunc`, and `date_diff` must be a **string literal** from the allowed set. Non-literal expressions (field refs, computed values) and unlisted units are rejected before execution in both batch and streaming modes (batch validates at SQL-emit time, streaming at stream-plan compile time).

| Function | Allowed units |
|----------|--------------|
| `date_part` | `year`, `quarter`, `month`, `week`, `day`, `hour`, `minute`, `second`, `dow`, `doy`, `epoch` |
| `date_trunc` | `year`, `quarter`, `month`, `week`, `day`, `hour`, `minute`, `second` |
| `date_diff` | `year`, `quarter`, `month`, `week`, `day`, `hour`, `minute`, `second` |

`dow` = day of week (Sunday = 0 … Saturday = 6). `doy` = day of year (1–366). `epoch` = seconds since Unix epoch (float).

`date_trunc("week", ts)` truncates to **Monday midnight** (ISO 8601 week start).

`date_diff` counts boundary crossings between the two timestamps for `year`, `quarter`, `month`, `day`, `hour`, `minute`, and `second`. The `week` unit is the exception: DuckDB computes it as the whole number of days between the dates divided by 7 (integer division toward zero), **not** week-boundary crossings.

#### strftime/strptime format codes

Standard C `strftime` codes (`%Y`, `%m`, `%d`, `%H`, `%M`, `%S`, etc.) produce identical output in both batch (DuckDB) and streaming (chrono) paths. chrono operates on a timezone-naive timestamp, so it is **not** locale-dependent — but codes that depend on timezone or locale (`%Z`, `%z`, `%c`, `%x`, `%X`) render empty or fixed under chrono's naive semantics and can differ from DuckDB. Stick to explicit numeric codes for portable output.

Invalid format codes (e.g. `%Q`, or a trailing `%`) are rejected before execution in both paths when the format is a string literal — they no longer error in batch while silently nulling in streaming.

**Partial formats:** `strptime` fills the components a format omits from a `1900-01-01 00:00:00` base, identically in the batch (DuckDB) and streaming paths. A **date-only** format (e.g. `%Y-%m-%d`) yields midnight (`00:00:00`); a **time-only** format (e.g. `%H:%M:%S`) yields the `1900-01-01` base date; **year-only** (`%Y` → `2023-01-01 00:00:00`), **year-month** (`%Y-%m` → `2023-11-01 00:00:00`), a bare **month-day** (`%m-%d` → `1900-11-07 00:00:00`), and a date with an *incomplete* time (`%Y-%m-%d %H` → `…14:00:00`) all fill the same way. Exotic or locale-dependent codes follow chrono in the streaming path and may differ from DuckDB: a bare two-digit year (`%y` alone) yields `null` where DuckDB fills the base year, and timezone-offset codes (`%Z`/`%z`) keep chrono's wall-clock time rather than normalizing to UTC. (A two-digit year *with* a month/day, like `%y-%m-%d`, resolves via chrono's pivot and matches DuckDB.)

## Examples

```
# Errors in the last hour by service
_severity>=error last=1h | stats count() by service | sort -count

# Slow requests by endpoint
status=200 last=24h | where duration > 1000 | stats avg(duration) by uri | sort -avg_duration | head 10

# 4xx/5xx rate by host
status>=400 last=2h | stats count() by host, status | where count > 10

# Extract IPs and count
"connection from" | rex "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message | stats count() by ip | sort -count

# Time series of error rate
_severity>=error | timechart span=5m count() by service

# Dedup flapping alerts
service=monitoring | dedup host, alert_name

# Pivot status codes by host
last=1h | pivot count() on status by host

# Conditional field with null handling
* | eval msg_len = if(isnotnull(message), length(message), 0) | fields host, msg_len | head 10

# Distinct values per group
* | stats values(_severity), first(message) by service | head 10
```
