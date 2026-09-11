---
title: Build a query
description: Filter events, pick columns, sort rows, and summarize counts with the query language.
---

A query selects events, then passes them through pipe stages. Run each example
in browser Search or pass it, quoted, to `trawl query`. The examples use the
three `service=tutorial` events from [Your first query](/getting-started/first-query/).
With your own logs, replace the service and look at a few rows first.

## Find your events

```text
service=tutorial last=1h
```

Expect three rows. Both filters apply: `service` equals `tutorial` and the
event time falls in the last hour. Start with a small range and widen it when
you have a reason. In the browser, the range control and the **Filters**
sidebar add to the editor text, and a [shared link](/use/sharing-export/)
carries all three.

## Pick the columns to show

```text
service=tutorial last=1h | table _time, level, _severity, message, duration
```

Expect three rows with those five columns. `_time` is the event time and
`_severity` is the severity Trawl derived. `level` and `duration` came from
the sender. The sample durations 12, 1500, and 700 have no unit. Ask what a
real source means before you set a threshold.

Open **Schema** to find your fields. Quote a name with spaces or punctuation
in backticks, such as `` `http.status` ``, rather than renaming source fields.

## Select errors

```text
service=tutorial _severity>=error last=1h | table message, duration
```

Expect the one `connection refused` row. `_severity` compares severity bands.
`level=error` compares the sender's own `level` text, which can use any
vocabulary.

## Search raw text

```text
service=tutorial "connection refused" last=1h
```

Expect the same row. A quoted term matches the raw event text, field names
included, so name the field when you mean one. See [text search](/reference/dsl/#text-search).

## Filter and sort rows

```text
service=tutorial last=1h | where duration > 500 | sort -duration | table message, duration
```

Expect the 1500 row, then the 700 row. `where` keeps rows where the expression
is true. A minus sign sorts descending. Add `| head 10` after `sort` for the
ten largest.

A missing field is not an empty string. An expression on it is unknown, and
`where` drops unknown rows. See [missing fields and nulls](/reference/dsl/#missing-fields-and-nulls)
when a negated filter returns fewer rows than you expect.

## Count and summarize

```text
service=tutorial last=1h | stats count() by service
```

Expect one row: `tutorial`, `3`. `stats` replaces event rows with summary
rows, and fields you did not group by or aggregate are gone. Name calculated
columns for later use:

```text
service=tutorial last=1h | stats avg(duration) as mean_duration, count() as events by service
```

Expect `mean_duration` close to 737.33 and `events` equal to 3. Filter after
`stats` with the output names:

```text
service=tutorial last=1h | stats count() as events by service | where events >= 3
```

Expect the same row.

## Chart counts over time

```text
service=tutorial last=1h | timechart span=5m count()
```

In the browser, open **Visualization**. Expect one bar, because all three
events fall in one five-minute bucket. For events still arriving, use
[live tail](/use/live-tail/). To run a query again later, [save it](/use/saved-reports/).
The [DSL reference](/reference/dsl/) lists every stage and function, time
bounds, quoting, pinned types, and what runs in a stream.
