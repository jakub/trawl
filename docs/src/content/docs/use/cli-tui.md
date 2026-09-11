---
title: CLI and TUI workflows
description: Use one-shot commands for scripts and the terminal UI for interactive investigation.
---

Both interfaces use the same server and key configuration. Start with
[Connect to a server](/start/connect/) if you have not configured a client.
Use an explicit profile or config when you work with more than one server.

## Run a one-shot query

```bash
trawl --profile lab query 'service=nginx last=15m | head 20'
```

The `lab` profile must already exist. Replace the service with one in your
installation. The CLI prints a table to a terminal and newline-delimited JSON
to a pipe. Select a format explicitly in scripts:

```bash
trawl --profile lab query --format json \
  'service=nginx last=15m | stats count() as events by host' | jq .
```

Each JSON object is a result row. A failed command is an error, not an empty
result. Check its exit status in automation, and keep stderr separate from
the data you are collecting.

## Check syntax and discover fields

```bash
trawl --profile lab validate 'service=nginx last=15m | stats count() by host'
trawl --profile lab schema fields --service nginx
```

Validation checks the query without running the event scan. The server may
still reject execution for a missing field, a permission, or resource limits.
Schema commands require schema-read permission. The
[CLI reference](/reference/cli/) covers their options.

## Investigate in the TUI

```bash
trawl --profile lab
```

Run with no subcommand to enter the interactive terminal UI. Start by opening
Help for the current keyboard shortcuts. Enter a bounded query in the editor,
execute it, then inspect the result table or visualization. Use Schema to
inspect source fields, History to revisit previous queries, and Saved to
return to named investigations.

The editor, focus, selection, and active popup affect how a key is handled.
Close a popup before assuming a global shortcut will act on the results.
The terminal UI also supports configured mouse input and themes; see
[client configuration](/reference/configuration/) for those settings.

## Keep output and query state distinct

Changing editor text does not make existing rows a result of that new text.
Execute the changed query and check its completion state before drawing a
conclusion. A live view is different again: it updates as events arrive and
is bounded by the client's live buffer. Switch back to a snapshot query when
you need a repeatable time interval.

Use [exports](/use/sharing-export/) for a file to hand to another tool.
Use [saved queries](/use/saved-reports/) for a query you want to execute again.

## Programmatic TUI inspection

For agent-driven checks, the TUI has an optional Unix socket driver:

```bash
trawl driver --help
trawl driver query --help
```

These help commands do not launch the TUI. A driver-enabled TUI must be
started separately with `--driver /path/to/private/session.sock`.
Always name the socket when controlling a session. `status` and `get-results`
read its existing state; `query` replaces the editor and executes;
`key`, `keys`, and `quit` change the session.

Use a new socket inside a private directory for automated tests. Do not reuse
a user's socket: startup removes an existing file at that path. Driver
`capture` renders a text view, not a terminal screenshot. A query timeout
means the driver stopped waiting, not that execution was cancelled.
These details matter when deciding whether a retry is safe.

The repository's `trawl-tui` skill describes the agent workflow. For normal
interactive use, current Help and the [CLI reference](/reference/cli/) are
the command and keyboard authorities.
