---
title: CLI and TUI workflows
description: Run one-shot queries from scripts and investigate interactively in the terminal UI.
---

Both read `~/.config/trawl/config.toml`. Set it up with
[Connect to a server](/start/connect/) first, and name a profile when you use
more than one server.

## Run a one-shot query

```bash
trawl --profile lab query 'service=nginx last=15m | head 20'
```

Expect up to 20 rows and a footer with the number returned, such as
`3 row(s)`. The `lab` profile must exist.
Replace `nginx` with your service.

## Choose the output format

With no `--format`, a terminal gets a table and a pipe gets one JSON object
per line. Set it in scripts:

```bash
trawl --profile lab query --format json \
  'service=nginx last=15m | stats count() as events by host' | jq .
```

Expect one object per host. A failed command exits non-zero with the error on
stderr, so check the exit status and keep stderr apart from the data. Parquet
needs `--output`. See [output formats](/reference/cli/#output-formats).

## Validate a query and list fields

```bash
trawl --profile lab validate 'service=nginx last=15m | stats count() by host'
trawl --profile lab schema fields --service nginx
```

Expect `valid`, then a table with one row per field including `type` and
`last_seen`. `validate` checks syntax and semantics without scanning events,
so the server can still reject the run for a missing field, a permission, or a
resource limit. `schema` needs the `schema_read` permission.

## Investigate in the TUI

```bash
trawl --profile lab
```

With no subcommand, `trawl` opens the terminal UI. From its help screen:

| Key | Action |
| --- | --- |
| F1 | Toggle help |
| Shift+Enter, Ctrl+Enter, or F5 | Run the query |
| Alt+1 to Alt+4 | Query, History, Schema, and Saved tabs |
| Ctrl+S | Save the query |
| F9 | Toggle live tail |
| Ctrl+Q | Quit |

A key acts on whatever has focus, so press Esc to close a panel before a
shortcut reaches the results. Mouse, theme, and timezone are `[ui]` settings
in [client configuration](/reference/configuration/).

## Tell a result from the editor

Editing the query text does not change the rows on screen. Run the query and
wait for it to finish. A live view updates as events arrive and keeps at most
`tail.max_events` rows, 1000 by default. Use a normal query for a repeatable
interval, [exports](/use/sharing-export/) for a file, and [saved queries](/use/saved-reports/)
for a query you run again.

## Drive the TUI from a script

The TUI can listen on a Unix socket for automated checks. Start it with a
socket path in a private directory:

```bash
trawl --profile lab --driver /path/to/private/session.sock
```

A bare `--driver` uses `~/.config/trawl/driver.sock`, and startup deletes any
existing file at that path, so never reuse another user's socket.
`trawl driver --socket <path>` then takes `status`, `get-results`, `query`,
`set-query`, `key`, `keys`, `capture`, and `quit`. `capture` renders the
screen as text. A `query` timeout means the driver stopped waiting, not that
the query stopped, so check `status` before you retry. The
[driver section](/reference/cli/#driver-mode) lists every option.
