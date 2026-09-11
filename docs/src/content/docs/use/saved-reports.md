---
title: Saved queries and reports
description: Save an investigation, schedule repeated execution, and inspect stored runs.
---

A saved query stores query text and a name. A scheduled run stores an
execution result. Saving a query does not freeze today's rows, and editing
its text does not rewrite results from earlier runs.

The browser calls saved queries **Nets**. Open `/jobs/nets` for the saved
query list and `/jobs/runs` for recent report runs. These actions depend on
your key's saved-query permissions.

## Save an investigation

1. Build and run a query in Search.
2. Select **Save** and inspect the query preview in the dialog.
3. Give it a name that describes the question, such as `Nginx errors by host`.
4. Open Nets and select the saved query to inspect or change it.

Save captures the editor text when you open the dialog. It does not fold
sidebar filters and the range picker into that text. Put required filters in
the editor and check the preview before saving. See
[Sharing and export](/use/sharing-export/) for preserving a search link.

Open the saved query in Search when you want to run it interactively with a
new range. Rename or delete a saved query from its details only when you
intend to change that shared stored object.

## Add a schedule

In the saved query's details, configure its interval, enabled state, and
maximum retained runs, then save the change. An interval controls when the
scheduler runs; it is not necessarily the width of the data window.

For example, an hourly error report could use:

```text
service=nginx _severity>=error last=1h | stats count() as errors by host
```

A new browser-created schedule uses query mode: the saved text supplies its
own time bounds. The example measures the trailing hour on each execution.
If you instead configure a schedule-owned window through the TUI or API,
remove `last=1h` from the saved text. Trawl rejects conflicting time clauses
rather than choosing one silently.

## Choose the reporting window deliberately

A `since_last` window covers consecutive reporting periods. A fixed trailing
window, such as two hours on an hourly schedule, overlaps by design. `lag`
ends the window before the scheduled boundary to allow late events to arrive.
Missed boundaries coalesce into a bounded catch-up window rather than creating
an unlimited backlog of runs.

The current browser schedule editor displays an existing window and lag but
does not expose controls to change them. It preserves those settings when
you edit interval or enabled state. Use the TUI schedule editor or the
[schedule API](/reference/api/) to choose an explicit window or lag, with the
permission and review appropriate to your installation.

## Inspect a run

Open the saved query's runs or the global Runs page. Check the run's status,
execution time, and result before treating it as a successful report. A failed
run and a successful run with zero rows have different meanings.

A run retains the resolved DSL with the concrete interval it executed. Use
that text when investigating why a report included an event. Re-running it
later can still differ if retention or a catalog repin changed the corpus;
the stored result remains the evidence of that earlier execution.

Use **Trigger a scheduled run now** when you intend to create another run,
not merely to inspect the current saved result. Changing a schedule or
triggering a run is a server action, not a local display preference.

## Reuse results

The DSL's `from saved` stage reads stored report output. That output has its
own columns; it is not the current live event corpus. See
[the DSL reference](/reference/dsl/) for run selection and restrictions.
For a file to share outside Trawl, use [export](/use/sharing-export/).
