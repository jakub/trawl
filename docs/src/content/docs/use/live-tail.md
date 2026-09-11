---
title: Watch live events
description: Use live tail for incoming events and switch to a snapshot for a complete historical query.
---

Live tail uses a server-sent event connection to show events as they arrive.
Use it while reproducing a problem or checking a newly connected log source.
Your key needs `stream` permission. A quiet stream can mean no matching events
are arriving; it does not establish that historical storage is empty.

## Start in the browser

1. Open Search and enter a service filter, such as `service=nginx`.
2. Open the time-range control and choose **Live Tail**.
3. Watch **Events** while you generate a request or another known source event.
4. Check the connection state if the expected event does not arrive.

Begin with a plain event query. Add filters only after you have seen a known
event, for example:

```text
service=nginx _severity>=error
```

For a live count chart, use:

```text
service=nginx | timechart span=1m count()
```

Open **Visualization** for the chart. Raw event queries belong in Events;
a live visualization needs a supported aggregation.

## Use the terminal UI

Start `trawl` with the intended profile, enter your filter, and use the live
mode shortcut shown in Help. The regular `trawl query` command runs a snapshot. Use the TUI or browser
for interactive live tail.

## Keep the query streamable

The stream compiler supports a subset of batch queries. A stage that needs
a final ordering or a complete dataset may not work in live mode. If Trawl
reports an unsupported stage, remove that stage or switch to Snapshot.
Do not repeatedly reconnect with an unchanged rejected query.

Simple field filters and projections are good starting points. Consult the
[DSL reference](/reference/dsl/) for the supported streaming behavior of a
specific stage or aggregation.

## Understand gaps and limits

The browser and TUI keep bounded live buffers. Older displayed rows can leave
the buffer as new rows arrive. Slow consumers can also lag the server's event
bus. Treat a lag or disconnect notice as a gap in your observation, even if
new events later resume.

Live tail does not carry the batch query's degraded-field notice. Its absence
is not evidence that every field is healthy. Check Schema and use a snapshot
query when investigating catalog conflicts or missing values.

## Return to a historical answer

Switch to **Snapshot**, select a time range, and run the query. Use explicit
UTC start and end times when you need to compare observations with someone
else. A snapshot reads hot and stored events for that interval, subject to
retention, query limits, and any reported storage or field degradation.

[Export the snapshot](/use/sharing-export/) if you need to preserve the rows.
Do not use the current live buffer as proof of all events during an incident.
