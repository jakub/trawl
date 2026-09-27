---
title: Build a query
description: Filter events, pick columns, sort rows, and summarize counts with the query language.
---

A query selects events, then passes them through pipe stages. Run each example
in browser Search or pass it, quoted, to `trawl -p trial query`. The examples
use the three `service=tutorial` events that `trawl trial up` loads with its
sample data ([Your first query](/getting-started/first-query/)). They carry the
time at which the first `trawl trial up` loaded the samples, and a later `up`
does not move them, so the examples name no time range. `trawl query` adds
none. The expected rows hold while the samples are retained (90 days by
default). In the browser, select `7d` in **Date range**, because the default 15
minutes stops covering the events soon after `up`. `7d` ends at the current
time, so it covers the events for about seven days. After that, open
**Date range** and select **Absolute**. In the `Samples` section that
`trawl trial status` prints, copy the timestamp after `from` into **From**,
not the `created` timestamp above it. The section looks like this, and here
the value is `2026-09-24T12:01:36.453Z`:

```text
Samples
  2000 events from 2026-09-24T12:01:36.453Z to 2026-09-25T12:00:00.000Z
```

**From** accepts the timestamp with its milliseconds and drops them, which
moves the start a fraction of a second earlier. Leave **To** at `now`, and
select **Apply**. With your own logs, replace the
service and look at a few rows first.

## Find your events

```text
service=tutorial
```

Expect three rows: `service` equals `tutorial`. On real logs, add a time
bound such as `last=1h`, so the event time must also fall in the last hour.
Start with a small range and widen it when you have a reason. In the browser,
the range control and the **Filters** sidebar add to the editor text, and a
[shared link](/use/sharing-export/) carries all three.

## Pick the columns to show

```text
service=tutorial | table _time, level, _severity, message, duration
```

Expect three rows with those five columns. `_time` is the event time and
`_severity` is the severity Trawl derived. `level` and `duration` came from
the sender. The sample durations 12, 1500, and 700 have no unit. Ask what a
real source means before you set a threshold.

Open **Schema** to find your fields. Quote a name with spaces or punctuation
in backticks, such as `` `http.status` ``, rather than renaming source fields.

## Select errors

```text
service=tutorial _severity>=error | table message, duration
```

Expect the one `connection refused` row. `_severity` compares severity bands.
`level=error` compares the sender's own `level` text, which can use any
vocabulary.

## Search raw text

```text
service=tutorial "connection refused"
```

Expect the same row. A quoted term matches the raw event text, field names
included, so name the field when you mean one. See [text search](/reference/dsl/#text-search).

## Filter and sort rows

```text
service=tutorial | where duration > 500 | sort -duration | table message, duration
```

Expect the 1500 row, then the 700 row. `where` keeps rows where the expression
is true. A minus sign sorts descending. Add `| head 10` after `sort` for the
ten largest.

A missing field is not an empty string. An expression on it is unknown, and
`where` drops unknown rows. See [missing fields and nulls](/reference/dsl/#missing-fields-and-nulls)
when a negated filter returns fewer rows than you expect.

## Count and summarize

```text
service=tutorial | stats count() by service
```

Expect one row: `tutorial`, `3`. `stats` replaces event rows with summary
rows, and fields you did not group by or aggregate are gone. Name calculated
columns for later use:

```text
service=tutorial | stats avg(duration) as mean_duration, count() as events by service
```

Expect `mean_duration` close to 737.33 and `events` equal to 3. Filter after
`stats` with the output names:

```text
service=tutorial | stats count() as events by service | where events >= 3
```

Expect the same row.

## Chart counts over time

```text
service=tutorial | timechart span=5m count()
```

In the browser, open **Visualization**. Expect a line with one point at
count 3, because all three events fall in one five-minute bucket. For events still arriving, use
[live tail](/use/live-tail/). To run a query again later, [save it](/use/saved-reports/).
The [DSL reference](/reference/dsl/) lists every stage and function, time
bounds, quoting, pinned types, and what runs in a stream.
