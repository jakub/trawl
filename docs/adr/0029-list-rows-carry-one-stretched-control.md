# List rows carry one stretched control: links for places, buttons for commands, and a pointer-blocking popover is a trap dialog

status: accepted (2026-09-07) — prep ruling record for #100 slice D2

trawl-web-ui shipped 29 clickable elements that were not controls: five
list tables whose whole `div.tbl-row` or `<tr>` opened, expanded or reran
something, sort headers that were divs with a glyph, facet include and
exclude spans hidden until hover, a date-range trigger whose popover had
no overlay layer and no Escape, and a scattering of chips and carets.
ADR-0028 settled the fleet-ui half of this debt and named the app's
control rule (a control is `<button type="button">`), but it says nothing
about the shapes that only an application has: a row that is also a
place, a command that can refuse, a table drawn out of divs. The blind
design pair (one leg per model family) converged on those shapes; the
rulings the human made are marked.

## Decision

**A list row has exactly one control, in its primary cell, and that
control's hit area is stretched over the row. A stable destination is a
link; a command that goes through the search navigator is a button. A
div table gets no ARIA table roles until it gets all of them. An
app-owned popover behind a pointer-blocking scrim is a `Trap` dialog.**

- **One control per row, stretched.** (Human ruling: whole-row pointer
  activation stays.) The row element carries no handler. The primary
  cell's link or button is the only listener, and its `::after`
  pseudo-element covers the `position: relative` row so the pointer still
  hits anywhere on the row. Nested controls (an `ActionsMenu` trigger, a
  degraded-case badge, a quick action, "Save as Net") are positioned
  above that pseudo-element. Double activation is impossible because
  there is one listener on the path, so every `stop_propagation` on a
  nested app control is dead and deleted. The cost is that row text is
  not selectable, which it never was while the row itself was the
  handler. The alternative, a row handler filtered on `event.target`, is
  two activation paths that every future nested control must satisfy.
- **Links for places, buttons for commands.** A row whose activation is a
  complete, shareable URL today (`?svc=`, `?net=`, `?net=&ntab=runs`) is
  an `<a href>`: the router already lets modified clicks through to the
  browser (new tab, copy link) and reads `replace` as a property on the
  anchor, so the schema and nets drawers keep their replace semantics and
  the Runs page keeps its push. A control that builds a search URL
  through the navigator (history rerun, a top field's "use in search",
  the schema row's Search and Live Tail quick actions) stays a button,
  because ADR-0027 lets that navigation REFUSE an over-bound query and an
  anchor has no way to say no. The fleet-ui focus ring gains `a[href]`;
  the five app-side `outline` overrides on already-native controls are
  deleted so ADR-0007's ring is the one treatment.
- **Sort headers.** The control is a button inside the header cell, never
  the cell. `aria-sort` goes on the real `<th>` only. The div tables
  (schema, nets, the service drawer's field list) get no
  `role="columnheader"`: a lone header role without `table`/`row`/`cell`
  is invalid ARIA, and a five-table role retrofit is not a control
  conversion. Direction lives in the button's accessible name there. The
  service drawer's four hand-rolled headers move onto the one `sort_th`
  helper.
- **The range dialog.** The date-range trigger is a button that reports
  `aria-haspopup="dialog"` and `aria-expanded`. The panel is a named
  `role="dialog"` with `aria-modal="true"`, registered through fleet-ui's
  public overlay hook under `FocusPolicy::Trap`, Escape gated on topmost,
  the scrim dismissing on mousedown, initial focus on the first control,
  focus restored to the trigger. `Trap`, not `Capture`: the picker's
  scrim already blocks every pointer event, so a `Capture` layer would
  let Tab reach the Run button under an invisible sheet and Enter run the
  query with the picker open. This is a fact about THIS popover's scrim,
  not a rule that a scrim implies a trap: the Drawer is a `Capture` layer
  with a scrim by design. The dialog stays app-owned; slice E decides
  whether it is promoted.
- **Hidden-until-hover controls reveal on focus too.** Facet include and
  exclude, and the schema row's quick actions, are revealed by
  `:hover` OR `:focus-within` through opacity, never `display: none`,
  which removes a button from the tab order.
- **Names.** Icon-only and glyph-only controls carry `aria-label`s built
  from domain text (`Remove filter host = db1`, `Include status = 500`,
  `Sort by name`); the glyph is hidden. The theme control is a command
  named by its result (`Switch to dark theme`), not an `aria-pressed`
  toggle, matching ADR-0028's ruling on its menu twin. Interval presets
  and the quick-range options are exclusive choices and carry
  `aria-pressed`, the `Segmented` idiom.
- **CSS resets stay per rule** (ADR-0028). A button that replaces a
  grid or flex child gets its width back explicitly.

## Consequences

- The e2e stub gains a scenario with rows: the query route answers by DSL
  shape (`| top 10`, `| stats dc(`, otherwise event rows) and the history,
  saved-runs, run-result and runs routes gain fixtures, all decoded by
  the wire-fixture contract. `populated` is unchanged.
- The source scan that asserts no `on:click` is left on a `div`, `span`,
  `tr` or `th` outside an allow-list is a regression guard against the
  thirtieth pseudo-button. It is not the accessibility claim; the browser
  proofs, one per converted site, are.
- Out of scope and named: ARIA table roles on the div tables, the
  `Segmented` control's own semantics, toast `aria-live`, the ⌘K box (G4),
  promoting the range dialog into fleet-ui (slice E), and any claim about
  non-Chromium engines or assistive technology.

## Amendment: theme control location, 2026-09-16

For [issue #196](https://github.com/jakub/trawl/issues/196), the user chose
to remove Trawl's footer theme command and use the account menu's explicit
Light, Dark and System choices. The [ADR-0028 theme preference
amendment](0028-native-controls-and-the-menu-contract.md#amendment-explicit-theme-preference-2026-09-16)
replaces the theme-specific command and accessible-name rule above. Remove
the footer control, its handler and theme-specific label code while
preserving the footer's other content and keyboard order. Other command
names and exclusive-choice controls keep this ADR's existing rules.
