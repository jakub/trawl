# Planes, one labelled sidebar, and docked drawers

status: accepted (2026-09-13), ruling record for the UI redesign of
2026-09-13

The design study compared two directions against the shipped UI. The
ruling takes Direction A as the visual base (a small set of surface
planes, a re-derived ink ramp, an outline focus ring) and Direction B's
navigation and query console. This record covers the chrome half: the
plane system, the sidebar that replaces the icon rail and the mode tabs,
and the docked presentation the detail panels get. The search console,
the list pages and the reading modes are the same ruling, recorded in
the study bundle rather than here, because they change no shared API.

Three findings from the accessibility audit are fixed by the work this
ADR rules, not by a separate pass. A01 is text contrast below 4.5:1 on
the composited floors, A05 is `role="dialog"` on an `<aside>`, and the
missing landmarks around the status bar and the 404 card.

## Decision

**Five planes, named by token, applied in both themes.** Floor (`--bg`),
sheet (`--panel` with a hairline, `--edge-hi` and `--elev-1`), support
(`--panel-2`), well (`--well` with `--inset`), and raised or overlay
(`--elev-2`, `--elev-3`). `--shadow-1/2/3` become aliases of the new
ramp so nothing that reads them breaks. The ink ramp moves to Direction
A's measured values, and every pair in the audit clears 4.5:1 on the
surface it actually sits on.

**`:focus-visible` is an outline, not a box shadow.** `outline: 2px
solid var(--ring); outline-offset: 2px` on one rule. Elevation is a box
shadow on most controls now, so a box-shadow ring would collide with it.
`--shadow-glow` is deleted rather than kept for the few unelevated
elements: two focus idioms in one system is one too many.

**One labelled sidebar replaces the 84px icon rail and the topbar mode
tabs.** `Sidebar` takes `Vec<SidebarGroup { label: Option<String>, items:
Vec<RailItem> }>`, so each consumer names its own runs and fleet-ui owns
no taxonomy. A group with no label renders its items with no heading; a
labelled group renders a small-caps heading and carries `role="group"`
with that name. Width is 200px expanded and 56px collapsed. The class
name `rail` stays on the `<nav>`, and every link keeps its `title`, so
the pinned nav selectors move by one path segment instead of being
rewritten.

**The topbar becomes a command bar.** `<header class="topbar">` holding
the nav toggle, the active page's label as a crumb, the command palette
trigger and the account menu. It renders no navigation of its own. The
theme toggle stays in the account menu and in the status bar.

**Below 900px the sidebar is an overlay.** The command bar's toggle
opens it as a `FocusPolicy::Capture` layer over a scrim, the posture the
drawer already has. Escape closes it while it is topmost, a press on the
scrim closes it, a route change closes it, and focus returns to the
toggle through the layer's opener restore. The palette chord is inert
while it is open, because `has_layers()` is true.

**The palette's inventory is the sidebar's inventory.** `commands_from`
takes the groups, flattens them in source order, dedupes by exact path
with the first occurrence winning, and keeps each command's group label
and group position. An unlabelled group renders its options directly
under the listbox with no wrapper and no heading, because a
`role="group"` with no accessible name is a defect and the options
belong to the listbox either way. Trawl's inventory is Search, History,
Schema, then "Scheduled work" (Nets, Runs), then "Operations" (Health).
`/settings` leaves the inventory; it is a redirect, not a destination.

**Collapse is a `UiPrefs` pref, applied as a class.** `sidebar:
Expanded | Collapsed` round-trips through the same localStorage JSON as
theme and rowstyle. It writes no `<html data-*>` attribute: the only
rule that reads it is `nav.rail.collapsed`, and `runtime` writes
attributes only for CSS that keys off them. Without
`fleet_ui::install()` there is nowhere to persist the state, so no
collapse control renders.

**Drawers gain a docked presentation, and the host becomes a `<div>`.**
`Drawer` takes `docked: Signal<bool>` (default false) rather than a
second component: the docked panel is the same header, tabs and body
contract with a different host, and one component keeps one `sd-*`
family. Docked, the drawer renders in flow beside its list with no
scrim, registers no overlay layer, and closes on Escape only while focus
is inside it and no layer is open. Undocked, it behaves exactly as
today. The panel element is `<div role="dialog">` in both modes, because
`<aside>` is not an allowed host for that role (A05). The search
inspector, the schema service panel and the nets and runs detail panels
use the docked mode at wide widths and the scrim at narrow ones.

**Removed public items.** `Rail`, `ModeTab`, `AppLink`,
`Shell::{rail_items, rail_active, modes, app_links, rail_bottom}`,
`TopBar::{brand, brand_accent, modes, app_links}`, the token `--rail-w`,
and the token `--shadow-glow`. None is kept as a dead optional prop:
ADR-0025 bans an affordance that does nothing, and a prop the shell
ignores is the same lie one level down.

## Consequences

- Coastwatch consumes fleet-ui by path at a pinned `TRAWL_REV`. Its next
  bump changes exactly this, and nothing else:
  - `crates/web-ui/src/components/shell.rs:11` drops the `AppLink`
    import.
  - `crates/web-ui/src/components/shell.rs:162-163` deletes the `modes`
    and `app_links` lines. Both are already `Signal::derive(Vec::new)`,
    so no rendered output changes.
  - `crates/web-ui/src/components/shell.rs:212-224` replaces
    `rail_items=rail_items rail_active=rail_active modes=modes
    app_links=app_links` with `sidebar_groups=sidebar_groups
    sidebar_active=rail_active`, where `let sidebar_groups =
    Signal::derive(move || vec![fleet_ui::SidebarGroup { label: None,
    items: coastwatch_rail_items(&permissions,
    awaiting_review_count.get()) }]);`. One unlabelled group is parity;
    splitting it into named groups is optional later work.
    `components/rail.rs:39-53` and `rail_gate.rs` are untouched, so the
    permission gate and its tests stand.
  - `fleet_ui::install("coastwatch.ui")` is unchanged. The `"sidebar"`
    key is missing from its stored blob and takes the default silently,
    the same path every added pref has taken.
  - `.main` keeps `display: flex`, `overflow: hidden` and `min-height:
    0`, so `coastwatch.css:9-23`'s `.page` keeps scrolling.
    `--shadow-2` and `--shadow-3` (coastwatch.css:809, 1086) still
    resolve through the ramp aliases.
  - `fleet_ui::Login` gains optional props only, so `pages/login.rs`
    compiles unchanged, and `Drawer`'s `docked` prop is optional, so its
    drawer call sites compile unchanged.
  - Coastwatch's stylesheet references none of the removed tokens or
    classes.
- `sidebar_bottom` on `Shell` and `bottom` on `Sidebar` are `ViewFn`,
  not `Children`. The shell mounts the sidebar docked or as the compact
  overlay, and a `FnOnce` slot cannot render in both.
- `css_chrome_parity::mode_tabs_suppress_anchor_underline` is deleted
  with its selector. `shell_grid_has_auto_footer_row` now pins the two
  grids and the two `.main` declarations coastwatch depends on.
  `component_class_contract` gains the sidebar group wrapper and the nav
  overlay layer as pins.
- `e2e/selectors.ts` loses `paletteModeLink` and gains `navToggle`,
  `sidebarCollapse` and `topbarCrumb`; `paletteRailLink` becomes
  `nav.rail .grp > a[title]`. The settings, palette and responsive specs
  assert the six destinations in sidebar order.
- The `/settings` redirect keeps its test through an injected
  same-document anchor, since no rendered link points there any more.
- Trawl's status bar is a `<footer>` and the 404 card sits in a
  `<main>`, so neither falls outside every landmark.
