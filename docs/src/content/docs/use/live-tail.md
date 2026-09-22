---
title: Watch live events
description: Stream events as they arrive in the browser or the TUI, then return to a bounded query.
---

Live tail streams events over a server-sent events connection as `trawld`
accepts them. Use it while you reproduce a problem or check a new log source.
Your key needs the `stream` permission. A quiet stream means no matching event
arrived, not that nothing is stored.

## Start live tail in the browser

1. Open Search and enter a service filter such as `service=nginx`.
2. Open the range control and select **Live Tail**.
3. Trigger a known event, such as one request to the service.
4. Watch **Events**. Expect the event within seconds and **Live** in the status bar.

A live stream has no time window. The range control shows the range that
**Stop live** returns to. It does not limit the stream. A time clause you type
into the query, such as `last=1h`, still applies.

Add filters only after a known event has arrived:

```text
service=nginx _severity>=error
```

For a live count, use an aggregation that runs in a stream and open
**Visualization**:

```text
service=nginx | timechart span=1m count()
```

## Start live tail in the TUI

Open `trawl` with the profile you want, enter the filter, and press F9. F9
again leaves live mode. `trawl query` always runs a bounded query.

## Keep the query streamable

The stream runs a subset of the batch stages. A stage that needs a final
ordering or the whole result does not run live. If Trawl reports an
unsupported stage, remove it or return to a bounded query. Reconnecting with
the same query does not help. Field filters and `table` are safe starting
points. The [DSL reference](/reference/dsl/) says which stages run in a
stream.

## Read a gap or a lag

Both clients keep a bounded buffer. The TUI keeps `tail.max_events`
rows, 1000 by default, and drops the oldest first. A slow client falls behind
the server's event bus, and the status bar shows **Lagged** with a count.
Treat a lag or **Error** as a gap in what you saw, even if events resume. Live
tail carries no degraded-pin notice, so open **Schema** and run a bounded
query when you investigate a conflict or missing values.

The status bar counts the stream. **Received** is the number of events the
browser buffer accepted since the stream opened, not the number of messages
the server sent. For a query that charts an aggregation, the status bar reads
**Updates** and counts the snapshots instead. The **Events** tab counts the
rows on screen, so **Received** passes the tab count once the buffer starts
dropping the oldest rows. Both counts restart when you retry the stream or
change the query, and both survive a reconnect.

## Return to a bounded query

Select **Stop live**, above the results. Trawl closes the stream and runs the
same query once, with your filters and the range unchanged. The bounded query
applies that range. The stream did not. The browser Back button returns to the
stream.

To leave live mode on a different window, open the range control, select a
range such as **Last 15m** or an **Absolute** interval with **From** and
**To**, then select **Apply**. Any range selection leaves live mode, including
a repeat of the range already selected. Use absolute UTC times when you
compare notes with someone else.

**Haul** keeps the mode you are in. In live mode it streams the edited query,
and an unchanged query changes nothing. Adding, removing, or clearing a filter
also keeps the mode.

The bounded query reads the hot buffer and stored events for that interval,
subject to retention and query limits. [Export the result](/use/sharing-export/)
to keep the rows. A live buffer is never proof of every event during an
incident.
