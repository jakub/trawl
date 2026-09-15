# fleet-ui

Vocabulary for the shared leptos design system consumed by trawl-web-ui and coastwatch. Cross-cutting terms (event, field, pin, lane) live in the root `context.md`; this slice names only the chrome fleet-ui owns. Decisions live in `docs/adr/`; the overlay contract's own rules are the module doc of `crates/fleet-ui/src/overlay.rs`, which ADR-0003 and ADR-0028 cite rather than restate.

## Language

### Overlays and focus

**Overlay stack**:
The in-process, last-in-first-out list of open layers. It answers exactly one question per layer, whether that layer is topmost, and every layer gates its own Escape and outside-click handling on that answer. The stack never handles a key itself.
_Avoid_: modal manager, z-index stack (the stack is logical, not visual)

**Layer**:
One open overlay's registration on the stack, alive exactly as long as its panel is mounted. A closed menu is not a layer; a mounted-but-hidden panel would be, which is why panels are separate components.
_Avoid_: overlay (that is the family of components), popup

**Focus policy**:
What a layer claims about focus when it registers: `Trap` owns focus and cycles Tab inside its panel (the modal family), `Capture` owns focus without trapping (the drawer), `None` is invisible to focus ownership and manages its own (both menus). Topmost and focus owner are different questions: a `None` layer can be topmost for Escape while a modal beneath it still owns focus.
_Avoid_: focus mode, trap flag

**Focus owner**:
The topmost layer whose policy is not `None`. Only the owner traps, and only the owner restores on close.
_Avoid_: active overlay

**Opener**:
The element that had focus when a focus-owning layer mounted, captured once and restored on unmount if that layer still owns focus and the element is still in the document. Menus have no captured opener; they restore their trigger by cause (see menu).
_Avoid_: return target, previous focus

**Scrim**:
The full-viewport backdrop a modal or drawer renders behind its panel; a mousedown on the scrim itself, and nothing inside it, dismisses. Menus have no scrim: they dismiss on a mousedown outside the trigger-plus-panel wrapper.
_Avoid_: overlay, backdrop, mask

### Menus

**Menu**:
A popup of application commands: `role="menu"` holding only menu items and separators. fleet-ui has two, the command bar's account menu and the table row's actions menu, and they share one contract: a `None` layer, one tab stop, an indexed arrow walk, Tab closes and returns to the trigger, restore by cause. Not a disclosure of links and not a listbox.
_Avoid_: dropdown, popover, user-menu dropdown

**Trigger**:
The native button that opens a menu and reports its state through `aria-haspopup` and `aria-expanded`. It is what focus returns to on Escape, on item activation (before the item's callback runs) and on Tab; never on an outside click.
_Avoid_: opener (that is the overlay term), toggle

**Panel**:
The mounted body of an open menu, drawer or modal. For a menu it is the positioning wrapper around the identity header and the `role="menu"` node, carries `tabindex="-1"` as the empty fallback, and exists only while open.
_Avoid_: dropdown body, popup

**Menu item**:
A native button with `role="menuitem"` inside a menu. Exactly one item in an open menu is tabbable; the others carry `tabindex="-1"` and are reached by arrows, Home and End.
_Avoid_: row, entry, option (that is a listbox term)

**Command palette**:
The ⌘K surface ADR-0025 assigns to slice G4. Always the two-word term: an unqualified "palette" in fleet-ui is the atmosphere palette.
_Avoid_: palette, search box, jump

**Atmosphere palette**:
The per-theme colour stops that drive the WebGL backdrop (`atmosphere::palette`). Unrelated to the command palette.
_Avoid_: palette (unqualified), theme colours

### Chrome and navigation

**Sidebar**:
The one navigation list, `nav.rail`, rendered by `Shell` down the left of every page: a brand, then groups of destinations, then a bottom slot. It replaced the icon rail and the command bar's mode tabs (ADR-0032); the class name `rail` stayed so the pinned nav selectors did not move.
_Avoid_: rail, nav rail, menu, drawer

**Sidebar group**:
One labelled run of destinations the consumer passes as data. A group with no label renders its items with no heading; a labelled group renders a small-caps heading and names the run for assistive technology and for the command palette.
_Avoid_: section (that is the app's own term for a route), mode, category

**Collapsed sidebar**:
The icon-only presentation, persisted in `UiPrefs`. Every destination keeps its `title` and its label as screen-reader text, so nothing loses its accessible name. The collapse control lives in the bottom slot and renders only when the consumer persists the state.
_Avoid_: mini rail, icon mode, hidden sidebar

**Command bar**:
The `TopBar`: the top strip that names the current page and carries the affordances that are not destinations — the nav toggle, the command palette trigger and the account menu. It holds no navigation of its own.
_Avoid_: topbar tabs, header nav, toolbar

**Page crumb**:
The command bar's page title, the label of the sidebar item the consumer marks active. It is text, never a link, and falls back to the brand before routing state resolves.
_Avoid_: breadcrumb trail (there is one level), heading

**Nav overlay**:
The sidebar below 900px: a `Capture` layer over a scrim, opened by the command bar's nav toggle, closed by Escape, by a press on the scrim, or by a route change. Focus returns to the toggle, and the palette chord stays inert while it is open.
_Avoid_: mobile menu, hamburger, off-canvas

### Strips and controls

**Tab strip**:
The `Tabs` component: a `role="tablist"` wrapping only the tab buttons, each `role="tab"` with `aria-selected` and roving tabindex, arrows move focus, Enter or Space activates. It has a required accessible name and owns no panes; the consumer renders the selected content after it. Two visual families, the workspace strip and the drawer strip, are one component with one contract.
_Avoid_: tabs bar, segmented control (that is `Segmented`, an `aria-pressed` group), toggle buttons

**Workspace strip**:
The `TabsStyle::Workspace` family: the page-level strip with a trailing action slot outside the tablist.
_Avoid_: page tabs, results tabs

**Drawer strip**:
The `TabsStyle::Drawer` family: the strip at the top of a drawer with metadata text outside the tablist.
_Avoid_: drawer tabs, sd-tabs

**Drawer**:
The right-slide detail inspector: a `Capture` layer with a scrim, a drawer strip and a body the consumer fills. Docked presentation: the same shell rendered in flow beside its list with no scrim and no overlay layer; Escape closes it only while focus is inside it and no layer is open.
_Avoid_: side panel, sheet, inspector

**Docked panel**:
A `Drawer` rendered with `docked`: the same header, strip and body, placed in flow beside the list it details instead of over a scrim. It registers no layer, so it captures no focus and leaves the command palette's chord live; a consumer flips the prop on a media query and the panel's children stay mounted across the breakpoint.
_Avoid_: inline drawer, split view, sidebar (that is the navigation)

**Reading preference**:
A `UiPrefs` field that chooses a presentation rather than a theme: `sidebar` (expanded or collapsed), `details` (inline or inspector) and `rows` (compact or message-first). fleet-ui stores and persists them beside the theme; what each presentation looks like is the consumer's.
_Avoid_: setting, layout option, view state

**Toast host**:
The one mounted `Toasts` component that renders the queue. Its dismiss control is a named native button.
_Avoid_: toaster, notification area

**Toast bus**:
The push handle consumers use to raise a toast. Kinds are data on the bus; announcement semantics are not part of the bus.
_Avoid_: notifier, toast service

**Bare copy mode**:
`CopyButton` rendered with a caller-supplied `class`: a native button the caller's class styles entirely, reset included. The default mode renders the design system's own button.
_Avoid_: inline copy, span mode

**Native control**:
Any fleet-ui element that acts on click, rendered as `<button type="button">` so the keyboard, the ADR-0007 focus ring and assistive technology reach it. The only clickable non-buttons left are scrims.
_Avoid_: pseudo-button (that is the defect, not the term), clickable

**Range dialog**:
The whole range control: a trigger and a temporary selection panel that accepts app-supplied presets or absolute bounds. An accepted transition closes the panel and returns focus to its trigger; refusal keeps the draft and its error visible.
_Avoid_: popover, date picker, dropdown
