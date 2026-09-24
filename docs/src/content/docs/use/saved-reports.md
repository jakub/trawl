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
2. Select **Save as Net** in the console. The dialog shows the editor text under **Query**.
3. Enter a **Name** that states the question, such as `Nginx errors by host`, and select **Save as Net**.
4. Open **Nets** and select the net to inspect or change it.

Save captures the editor text only, not the **Filters** sidebar or the range
control, so put every filter in the editor before you save. To keep the whole
browser state, [share a link](/use/sharing-export/) instead. **Open in search**
runs a net with a new range. **Rename** and **Delete** change the stored net
for everyone.

## Add a schedule

Open the net's **Query + Schedule** tab. A new net shows `No schedule
attached.`, so select **+ Add Schedule** first. Set **Run every** to the interval.
In **Keep schedule running for**, enter a run count or leave it blank for unlimited
runs. Switch on **Schedule enabled**. Select **Save schedule**. The interval says
how often the query runs, not how wide the data window is. For an hourly
error report:

```text
service=nginx _severity>=error last=1h | stats count() as errors by host
```

**Each run covers** decides what each run reads. Under **Query text**, the run executes
the saved text as written, so the `last=1h` above measures the trailing hour on
each run. The other two modes give the schedule its own bounds, and a query that
already sets a time range cannot take one. Remove `last=1h` from the text
before you save a window, or the server refuses the save and names both sides.

## Choose the reporting window

Pick a mode in the **Each run covers** control:

- **Query text** leaves the bounds to the query. Without a time clause in the
  text, the schedule imposes no time limit.
- **Since last run** covers consecutive periods with no gap. Each run starts
  where the previous one stopped covering.
- **Fixed span** covers the **Trailing span**, measured again from every run. A
  two-hour span on an hourly schedule overlaps between runs.

**Late-arrival lag** applies to both windowed modes. It moves both window bounds
back by that much, so late events land before the window that
owes them closes. Leave it blank for none.

When the scheduler misses boundaries, it runs one bounded catch-up window
instead of a backlog.

## Run a scheduled net now

To run a net without waiting for its next boundary, select **Run now** in its
row or in its drawer header. Any net with a saved schedule offers it, including
a paused one. A net with no schedule does not.

**Run now** fires the schedule's next window early. The server picks the window
when it accepts the run, and the toast shows it:

- **Since last run** reads from where the last successful run stopped to the
  current time, less the lag. The same catch-up limit applies. On success, the
  next scheduled run starts where this one stopped.
- **Fixed span** reads the trailing span ending at the current time, less the
  lag. Its results can overlap earlier runs.
- **Query text** runs the saved text as written.

A successful manual run also counts as any scheduled boundary that is already
due, so the scheduler does not repeat an older window after it. A failed manual
run changes nothing, and the due run still fires. Manual runs count toward
**Keep schedule running for**.

The server refuses a manual run when a run is already in progress, when the
run count is used up, or when a **Since last run** schedule has nothing new to
read. The toast shows the server's reason.

Switching a windowed schedule back to **Query text** removes the window and the
lag. The form says so before you save and, when the schedule reports one, names the point coverage stops at.
Changing the window or the interval can make the next run due immediately.

## Inspect a run

Open the net's **Runs** tab or the **Runs** page. Check the status, the
execution time, and the result before you trust it. A failed run and a
successful run with zero rows mean different things.

Each run keeps the query text it ran. A run under a schedule window stores
the absolute bounds it covered, which explains why a report included an
event. A run without a window, including every run of the schedule above,
stores the saved text as written, so its `last=1h` stays relative and a rerun
of that text reads a different hour. The stored result is the record of the run
either way. History marks a run started with **Run now** as **Manual**. Select
**Run now** only when you want a new run. It changes the server, not only your
screen.

Expanding a run shows its stored result 20 rows to a page. Paging reads the
rows the browser already has and sends no further request. A run that stored
more rows than the server returned says how many of each under the pager, so a
short preview reads as a capped fetch rather than a short run.

## Reuse a report in a query

The `from saved` stage reads a stored run's output, which has its own columns
rather than the live event fields. See the [DSL reference](/reference/dsl/)
for run selection and limits, or [export](/use/sharing-export/) a file to
share outside Trawl.
