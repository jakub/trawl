---
name: trawl-tui
description: Inspect or exercise Trawl's terminal UI through its Unix socket driver and an owned PTY, for TUI debugging and rendering evidence.
---

# Operate the TUI

Select the binary, target config, and session. An existing driver socket
controls that TUI's existing connection; `trawl driver --url` does not retarget
it. Use an explicitly selected socket, not the user's default socket.

Discover commands with `env -u TRAWL_TOKEN "$TRAWL_BIN" driver --help` and
`driver COMMAND --help`. Unsetting the token prevents Clap from showing its
environment value. Bare `trawl` launches the TUI; it is not help discovery.

The current keyboard help is in
[ui/help.rs](../../../crates/trawl-cli/src/tui/ui/help.rs).
Read [driver.rs](../../../crates/trawl-cli/src/tui/driver.rs) for protocol and
session behavior, and [lib.rs](../../../crates/trawl-cli/src/lib.rs) for CLI
arguments. Prefer driver operations to guessed terminal escape sequences.

## Launch and inspect

For a new session, use a supervised PTY and a fresh socket inside an owned
mode-0700 directory. Startup removes any existing path at the chosen socket.
The target config must name the approved server and credential source.
Launching also truncates `~/.config/trawl/tui.log`; `--config` does not move
that log. Preserve needed diagnostics and avoid a competing user TUI session.
A strictly read-only task should inspect an existing authorized session.

Here `TRAWL_BIN` is the selected executable, `TRAWL_TEST_CONFIG` is the explicit
test config, and `TRAWL_TEST_SOCKET` is the fresh private socket path:

```bash
# Launch in the owned PTY.
"$TRAWL_BIN" --config "$TRAWL_TEST_CONFIG" --driver "$TRAWL_TEST_SOCKET"
# Inspect from a separate command session after the listener starts.
"$TRAWL_BIN" --config "$TRAWL_TEST_CONFIG" driver --socket "$TRAWL_TEST_SOCKET" status
"$TRAWL_BIN" --config "$TRAWL_TEST_CONFIG" driver --socket "$TRAWL_TEST_SOCKET" capture --width 120 --height 40
```

`status` and `get-results` inspect session data. `capture` renders text and
updates layout bookkeeping; it is not a pixel screenshot or a state-free read.
For a strictly read-only inspection, use `status` and `get-results` only.

## Exercise the selected session

`set-query` replaces the editor. `query` replaces it and executes against the
server. `key` and `keys` invoke ordinary handlers, including saves, deletes,
and schedules. Use only actions within the requested task. Run one operation
at a time and inspect state before the next action.

For an authorized fixture query, use `driver --socket "$TRAWL_TEST_SOCKET"
query 'service=agent_fixture last=5m | head 5' --timeout 15000 --format json`
with the same binary and config as above. Check against known fixture rows.

A timeout stops waiting; it does not cancel execution. Inspect state before
retrying. `query` sets the editor and executes in separate requests, and a
`keys` sequence may partly apply before an error. `get-results --tab` currently
ignores the index and returns the active tab.

Quit only the owned or explicitly authorized TUI, observe PTY exit, and check
socket cleanup. Text capture does not prove terminal colors or clipboard behavior.
