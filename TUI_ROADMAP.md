# trawl TUI — opinionated UX review

## context

the tui is already solid. ratatui 0.30, 5-tab layout, full custom editor with undo/redo and selection, syntax highlighting with schema awareness, sparkline timechart visualization, two-pane detail views for history/schema, live streaming, responsive status hints. the bones are genuinely good — this isn't a "fix it" review, it's a "what takes it from good to *impressive*" review.

the north star here is: **what would make an experienced ops/SRE person choose trawl's TUI over opening a browser tab?** the answer is always speed, keyboard fluency, and information density that a web UI can't match.

---

## tier 1 — high-impact, medium effort (the "obviously missing" stuff)

### 1. ~~autocomplete / suggestions in the editor~~ ✅ DONE

ghost-text autocomplete with context-aware triggering. see `crates/trawl-cli/src/tui/autocomplete.rs` and `SUGGESTIONS.md`.

### 2. query history in the editor (↑/↓ cycling)

right now ↑/↓ in the editor moves the cursor. but when the editor has a single line (or is empty), ↑/↓ should cycle through recent queries — exactly like a shell. this is muscle memory for anyone who's used a REPL.

**proposal:** when cursor is on line 0 and ↑ is pressed (and there's no line above), cycle backwards through history. ↓ goes forward. store the "pre-history" buffer so the user can return to what they were typing. ctrl+r for reverse-i-search would be chef's-kiss but is a separate feature.

### 3. mouse support

the config field exists (`ui.enable_mouse`), the infrastructure is in ratatui — just needs wiring. at minimum:
- click to focus a pane
- click to select a row in results
- click to select a history/schema/saved item
- scroll wheel for vertical navigation
- click in editor to place cursor

mouse support isn't about replacing keyboard workflows — it's about not punishing the user when they instinctively reach for the trackpad. every moment of "oh right, i can't click there" is a paper cut.

### 4. toast notifications / transient status messages

right now, success and error states are only shown in the status bar or via modal popups. there's no middle ground. modern TUIs (lazygit, helix, zellij) use transient toast-style messages that appear briefly and auto-dismiss.

**use cases:**
- "query saved" (currently: nothing visible happens if you're on a different tab)
- "copied to clipboard" (currently: silent)
- "clipboard unavailable" (currently: silent failure)
- "live stream lagged — N events dropped" (currently: only logged)
- "schedule set" / "schedule deleted"

**implementation:** a small notification area (1-2 lines) above or overlaying the status bar, with auto-dismiss after 3s. ratatui has no built-in for this but it's ~50 lines of state + render code.

---

## tier 2 — medium-impact, medium effort (polish that compounds)

### 5. command palette (ctrl+p or ctrl+shift+p)

a fuzzy-searchable list of all available actions. this is the single best discoverability mechanism invented — better than help screens, better than cheat sheets. every modern editor has it for a reason.

**contents:** all actions currently in the help screen, plus:
- recent queries (fuzzy searchable)
- saved queries (fuzzy searchable)
- schema fields (jump to field in schema browser)
- tab switching
- toggle actions (live mode, chart view)

this subsumes the F1 help screen for discoverability and adds fuzzy search over saved/history queries as a bonus.

### 6. result column resizing and reordering

right now column widths are auto-computed from content. this is fine as a default but for wide tables with many columns, the user should be able to:
- resize columns (shift+←/→ or drag with mouse)
- toggle column visibility (hide/show)
- pin columns (keep `timestamp` and `host` visible while scrolling)

pinned columns is the killer feature here — in log analysis you almost always want the timestamp and one or two identifiers visible while scrolling through a wide result set.

### 7. multiple query tabs

the app has a single `Tab` for query work. multiple tabs (ctrl+t to open, ctrl+w to close, alt+1-9 to switch) would let users run multiple queries side-by-side, compare results, and maintain separate working contexts.

this is the kind of feature that turns a TUI from "quick query tool" into "i live in this thing." splunk's search interface supports this, and it's one of the first things people miss in simpler tools.

### 8. export from TUI

there's no way to export results from the TUI itself — you have to switch to the CLI. add:
- ctrl+e to export current results (prompt for format: json/csv/parquet)
- pipe to clipboard as JSON (ctrl+shift+c — copies entire result set)
- save to file with a path prompt

### 9. theme system

the config has a `theme` field that's never read. implement at least:
- `dark` (current default)
- `light` (for the sunlight-terminal weirdos)
- `nord`, `dracula`, `catppuccin` (the holy trinity of terminal rice)
- custom theme file support (map semantic names to colors)

this is low-impact per user but high-impact for adoption — people *will* dismiss a tool that doesn't match their terminal aesthetic. this is irrational but true.

### 10. vim mode (optional)

not full vim emulation — that way lies madness. but a lightweight modal mode:
- `Esc` enters normal mode, `i`/`a`/`o` enters insert mode
- hjkl movement, w/b/e word motions
- dd, yy, p for line operations
- `/` search (already exists)
- `:w` to save, `:q` to quit, `:wq`

configurable via `ui.editor_mode = "readline" | "vim"`. default stays readline. the overlap with existing vim search (`/`, `n`, `N`) is already there — this is just extending the metaphor.

---

## tier 3 — "delighter" features (what makes people tweet about it)

### 11. inline field value histograms in schema browser

the schema detail pane shows sample values and cardinality. add tiny inline bar charts showing value distribution — for a field like `level`, show `error ████░░░░ 23%` / `info ██████████ 67%` / `debug █░░░░░░░ 10%`. for numeric fields, show a distribution histogram.

this turns the schema browser from "what fields exist" into "what does my data actually look like" — enormously useful for query composition.

### 12. split-pane results comparison

after running a query, let the user split the results pane horizontally or vertically and run a second query in the new pane. useful for comparing time ranges, different filters, or before/after.

### 13. query builder mode

a structured form UI where the user picks fields, operators, and values from dropdowns/lists. generates the DSL query as they build. this dramatically lowers the barrier for users who don't know the DSL yet — and the generated query teaches them the syntax.

### 14. saved query dashboard layout

let users pin 2-4 saved queries as a dashboard grid in the dashboard tab, each showing a sparkline or key metric. auto-refresh on a configurable interval. this is the "single pane of glass" that ops people crave.

### 15. contextual right-click menu (mouse mode)

when mouse is enabled, right-click on a value to:
- filter by this value (`field=value`)
- exclude this value (`field!=value`)
- copy value to clipboard
- search for this value

this is one of splunk's most copied UX patterns and it's enormously powerful for ad-hoc investigation.

---

## structural observations

### the event loop is doing too much

`mod.rs` is handling input dispatch, popup routing, query execution, mutation polling, dashboard polling, driver commands, validation debouncing, and live stream management — all in one function. this works but it's getting brittle. consider:
- extract input handling into a proper `InputRouter` that dispatches based on mode/focus
- extract async task coordination into a `TaskManager`
- use a message/command pattern (ratatui's `Action` enum pattern) instead of direct state mutation

this isn't about abstraction for its own sake — it's about making the features above (command palette, multiple tabs, vim mode) implementable without turning `handle_key` into a 2000-line match statement.

### color constants should be centralized

colors are scattered across every rendering module. a `theme.rs` module with named semantic colors (`BORDER_ACTIVE`, `BORDER_INACTIVE`, `TEXT_PRIMARY`, `TEXT_MUTED`, `ACCENT`, `ERROR`, `SUCCESS`) would:
1. make the theme system trivial to implement
2. ensure visual consistency as you add features
3. make it easy to audit the visual hierarchy

### the popup system needs composability

every popup is a separate render function with its own layout math. as you add more popups (command palette, export dialog, theme picker), this becomes repetitive. consider a generic `Dialog` widget that handles centering, clearing, border rendering, and keyboard dismissal — with content provided as a closure or widget.

---

## what NOT to do

- **don't add tabs at the top for no reason.** 5 tabs is already a lot. resist the urge to add Config, Alerts, Dashboards-plural, etc. until there's a clear need.
- **don't chase feature parity with splunk's web UI.** the TUI wins on speed and keyboard fluency. lean into that.
- **don't add mouse-only features.** mouse support is about removing friction, not replacing keyboard workflows. every mouse action should have a keyboard equivalent.
- **don't add animations beyond cursor blink.** 100ms render loops and terminal rendering don't mix well with animation, and the aesthetic goal is "fast and precise," not "smooth and delightful."

---

## recommended implementation order

if i had to pick 5 things to build next, in order:

1. ~~**autocomplete**~~ ✅ — biggest single UX improvement, makes the DSL learnable
2. **toast notifications** — fixes silent failures, small effort, big polish
3. **mouse support** — wire the existing config flag, remove paper cuts
4. **editor history cycling** — tiny change, huge muscle-memory win
5. **command palette** — subsumes help screen, unlocks discoverability

everything else is gravy on top of a solid foundation.
