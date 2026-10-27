---
title: Work an investigation from the browser
description: Follow an agent's investigation from your phone, keep a row as evidence, send a search into it, and close it.
---

An agent such as Claude works a question in an
[investigation](/reference/api/#investigations) through
[`trawl mcp`](/use/agents/). Each search it attaches is a step, and its rows
are kept as evidence. The board in the browser shows that work while it
happens, and lets you add to it under your own key. The agent reads what you
add in its next attached result.

You need a browser sign-in on the same server, with a key of its own. To
follow and read, the key needs `query`, `schema_read`, and
`investigation_read`. To keep rows, send searches, and close the
investigation, it also needs `investigation_write`.
[Create a key for an agent](/operate/access/#create-a-key-for-an-agent) has
a reader role and a writer role for a person. Without `investigation_write`, the controls on this
page are absent and the rest works the same.

## Follow an agent's investigation

1. Open the link from the agent's answer, or open **Investigations** in the
   sidebar and select the investigation. Open ones are listed first.
2. Read the top of the board: the question and scope, then
   **Latest finding**, then **Activity**.
3. Leave the tab open. Expect **Live** in the header, and **checked** with a
   time that advances every few seconds.

New entries appear within a few seconds while the tab is visible, and what you
are reading stays where it is on screen. When you switch away from the tab,
the board reads nothing and shows **Paused**. When you switch back, it reads
at once. **Retrying** means a check failed. The board keeps what it shows and
tries again.

To see what a step ran, open it under **Steps**. To see the rows it captured,
select **View rows**.

## Look at the evidence

1. Under **Findings**, find the row a finding cites, and select
   **Open result**. Under **Steps**, select **View rows** instead.
2. Read the rows in the viewer. A cited row opens at its page, marked.
3. To run the same search over the current data, select **Open in Search**
   below the rows.

The viewer shows the rows as the agent captured them, even after the source
logs have left retention. Search runs the query again, so its rows can differ.
A result that reads **Pruned** has lost its rows to the evidence retention
clock. The finding still cites them.

## Keep a row

1. In the viewer, select **Keep row N** on the row.
2. Enter a reason the agent can act on, such as
   `this is the first 502 after the deploy`. The limit is 1024 bytes.
3. Press Enter. Expect `Kept row N. The board shows it in the activity.`

The keep appears in **Activity** under your key's name. The agent sees it in
the result of its next attached call. If the viewer says the outcome is
unknown, select **Refresh the board** and look for the keep in **Activity**
before you keep the row again.

## Send a search to the investigation

Send a search when you found something the agent did not look at.

1. Open Search, run the query, and check the rows.
2. Select **Send to investigation** beside **Export**.
3. Choose the investigation. The dialog has no default choice.
4. Optionally, enter a purpose, such as `errors on the other node`.
5. Send.

If the range moves, such as **15m**, the dialog offers to pin it to the
instant your search started on the server. Select **Pin and re-run**. Search runs again
over that fixed range, and the dialog sends that search once the run is
accepted, so the rows on screen are the rows sent. A fixed range sends at
once.

If the query sets its own moving range, such as `last=1h`, the dialog sends
nothing. Remove the time clause from the query, pick the range in the range
control, run the search, and send it again. A range longer than one capture
may cover, 7 days by default, is not sent either: pick a shorter range.

Expect a toast that names the new step, such as **Step 7 in Disk pressure on
node-2**, with an **Open the board** link. The search is a new step
under your key, and its rows are kept evidence. The control is absent in live
mode and for a search that reads `from saved`.

## Close the investigation

Close the investigation when the question has its answer.

1. In the board's header, select **Close** beside the status.
2. Expect `You closed the investigation.` and the status **Closed**.

A closed investigation takes no new steps, keeps, findings, or notes.
**Reopen** opens it again. If someone changed the investigation first, the
board says nothing changed and reads it again. Check the status, then press
the control again if you still mean to.

The [web UI reference](/reference/web-ui/#investigations) describes every
control and state.
