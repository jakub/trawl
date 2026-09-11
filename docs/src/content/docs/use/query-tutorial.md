---
title: Build a query
description: Go from a few events to a filtered result and a useful summary.
---

A Trawl query starts by selecting events, then passes them through pipe stages.
Run these examples in browser Search or pass the quoted text to `trawl query`.
The [first-query tutorial](/getting-started/first-query/) supplies the three
`service=tutorial` events used below. With your own logs, replace that service
and inspect a few rows before choosing field names.

## Find your events

```text
service=tutorial last=1h
```

The search stage combines these filters: service must be `tutorial` and the
event time must fall in the last hour. Use a small range first. Expand it when
you have a reason, such as looking for an incident from yesterday.

The browser has its own range and sidebar controls. Check the visible range
and filter chips as well as the editor: they contribute to the executed query.
See [Sharing and export](/use/sharing-export/) for retaining that full state.

## Read the fields

```text
service=tutorial last=1h | table _time, level, _severity, message, duration
```

`table` chooses output columns. `_time` is the canonical event time and
`_severity` is Trawl's derived severity. `level` and `duration` are sender
fields. The sample has durations 12, 1500, and 700; it does not declare a unit,
so a real source should document whether its duration means milliseconds or
seconds before you set thresholds.

Browse Schema to discover fields in your installation. Names that contain
spaces or punctuation can be quoted with backticks, such as `` `http.status` ``.
Do not rename source fields merely to fit the query grammar.

## Select an error

```text
service=tutorial _severity>=error last=1h | table message, duration
```

This returns the `connection refused` event from the sample. Use `_severity`
for severity bands; `level=error` instead compares the sender's own `level`
value. A sender is allowed to use a completely different vocabulary there.

A bare term searches raw event text:

```text
service=tutorial "connection refused" last=1h
```

Raw JSON can include field names as well as values. If you mean a specific
field, name it in the filter rather than relying on a text match.

## Filter and order a result

```text
service=tutorial last=1h | where duration > 500 | sort -duration | table message, duration
```

`where` evaluates an expression on each row. The sample returns the 1500 row
before the 700 row. A minus sign on a sort key requests descending order.
Add `| head 10` after the sort for the ten largest matching values.

Missing fields do not behave like empty strings. Expressions can evaluate to
unknown, and `where` keeps only true rows. See the
[null and comparison rules](/reference/dsl/#missing-fields-and-nulls) when a
negated filter returns fewer rows than expected.

## Count and summarize

```text
service=tutorial last=1h | stats count() by service
```

The sample produces one row with count 3. Aggregation replaces event rows
with summary rows. Fields you did not group or aggregate no longer exist.
Give calculated columns short explicit names when you will use them later:

```text
service=tutorial last=1h | stats avg(duration) as mean_duration, count() as events by service
```

The mean duration is approximately 737.33 and the event count is 3. A
post-aggregation filter uses those output names:

```text
service=tutorial last=1h | stats count() as events by service | where events >= 3
```

## Choose the next step

Use `timechart` for counts over time:

```text
service=tutorial last=1h | timechart span=5m count()
```

In the browser, open **Visualization**. All three tutorial events belong to
the same five-minute bucket. For newly arriving events, try
[Live tail](/use/live-tail/). For repeated investigations,
[save a query or report](/use/saved-reports/).

The [DSL reference](/reference/dsl/) lists stages and functions, precise time
bounds, quoting, pinned types, and the differences between batch and streaming.
