---
title: Saved queries and reports
description: Save a query, schedule it, and inspect the stored runs.
---

A saved query stores a name and query text. A scheduled run stores one
execution's result. Saving does not freeze today's rows, and editing the text
does not change earlier runs. The browser calls a saved query a net. **Nets**
at `/jobs/nets` lists them and **Runs** at `/jobs/runs` lists recent runs.
Your key needs the `saved_query` permission.

## Save a query

1. Build and run the query in Search.
2. Select **Save**. The dialog shows the text under **Query**.
3. Enter a **Name** that states the question, such as `Nginx errors by host`, and select **Save as net**.
4. Open **Nets** and select the net to inspect or change it.

Save captures the editor text only, not the **Filters** sidebar or the range
control, so put every filter in the editor before you save. To keep the whole
browser state, [share a link](/use/sharing-export/) instead. **Open in search**
runs a net with a new range. **Rename** and **Delete** change the stored net
for everyone.

## Add a schedule

Open the net's **Query + Schedule** tab. A new net shows `No schedule
attached.`, so select **+ Add Schedule** first. Set **Interval** and
**Max runs**, switch on **Schedule enabled**, and select **Save schedule**. The interval says
how often the query runs, not how wide the data window is. For an hourly
error report:

```text
service=nginx _severity>=error last=1h | stats count() as errors by host
```

A schedule created in the browser runs the saved text as written, so `last=1h`
measures the trailing hour on each run. If you give the schedule its own window
through the TUI or the API, remove `last=1h` from the text. Trawl rejects a
schedule whose text and window both set a time range.

## Choose the reporting window

A `since_last` window covers consecutive periods with no gap. A fixed trailing
window, such as two hours on an hourly schedule, overlaps between runs. `lag`
ends the window before the scheduled time so late events can land first. When
the scheduler misses boundaries, it runs one bounded catch-up window instead
of a backlog.

The browser shows an existing window and lag but has no controls for them, and
it keeps them when you change the interval or enabled state. Set them from the
TUI's Saved tab with `s`, or with the [schedule API](/reference/api/#schedules).

## Inspect a run

Open the net's **Runs** tab or the **Runs** page. Check the status, the
execution time, and the result before you trust it. A failed run and a
successful run with zero rows mean different things.

Each run keeps the query text it ran. A run under a schedule window stores
the absolute bounds it covered, which explains why a report included an
event. A run without a window, including every run of the schedule above and
every manual run, stores the saved text as written, so its `last=1h` stays
relative and a rerun of that text reads a different hour. The stored result
is the record of the run either way. Select **Trigger run** only when you want a new run.
It changes the server, not only your screen.

## Reuse a report in a query

The `from saved` stage reads a stored run's output, which has its own columns
rather than the live event fields. See the [DSL reference](/reference/dsl/)
for run selection and limits, or [export](/use/sharing-export/) a file to
share outside Trawl.
