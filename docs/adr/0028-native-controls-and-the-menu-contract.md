# Native controls and the menu contract: a fleet-ui control is a `<button>`, a menu is a `None` layer with one tab stop, and tabs are a named tablist that owns no panes

status: accepted (2026-09-07) — prep ruling record for #100 slice D

fleet-ui shipped nine clickable elements that were not buttons: the topbar
user menu's trigger and its two live rows, both tab-strip families, the
modal close, the toast dismiss and the copy button's bare mode. None
reached the keyboard, none carried a role, none earned the ADR-0007 focus
ring, and the user menu closed through its own full-viewport scrim rather
than the overlay stack, so Escape did nothing and a modal above it had no
arbitration relationship. ADR-0002 reserved their conversion for deliberate
a11y work; PR #34 left `ActionsMenu`, the one menu that did register with
the stack, "untouched" as a scope fence. This is that work. The blind
design pair (one leg per model family) and the critique round settled the
rules below; the rulings the human made are marked.

## Decision

**Every control fleet-ui renders is a native `<button type="button">`. A
menu registers as a `FocusPolicy::None` layer and manages its own focus
under one contract shared by both menus. A tab strip is a WAI-ARIA
tablist with a required name that owns no panes.**

- **Menus keep `FocusPolicy::None`.** `Capture` would make an open menu
  the topmost focus owner, and a menu inside a modal (a table's
  `ActionsMenu` under a dialog is an ordinary composition for a library)
  would then switch the modal's Tab trap off while trapping nothing
  itself, letting focus leave an `aria-modal="true"` dialog. The menu
  panel is a separate component mounted only while open, registers
  through `use_overlay_layer()`, and gates Escape AND outside-mousedown
  on `is_topmost()`. Initial focus goes through the shared
  `overlay::focus_initial` scan (made crate-visible, still wasm-only)
  under a once-per-mount latch the menu owns, because the overlay hook's
  latch belongs to the focus-owning policies. Both panels carry
  `tabindex="-1"` for the empty-panel fallback.
- **One tab stop per menu.** Items are `<button role="menuitem">` with true
  roving tabindex: exactly one at `0`, the rest `-1`. ArrowUp/Down wrap,
  Home/End reach the ends, the walk indexes `[role="menuitem"]` under the
  `role="menu"` node rather than element siblings, so a header or a
  separator can never take focus. Tab and Shift+Tab close the menu and
  return focus to the trigger; a deterministic extra keypress beats a
  document-wide scan for the trigger's successor across a panel that
  unmounts on a microtask. No typeahead: the two menus carry two and three
  items. The identity header sits before and outside `role="menu"`; the
  separator is `role="separator"` inside it.
- **Restore is cause-specific.** Escape restores the trigger. Item
  activation restores the trigger BEFORE running the callback, so a
  callback that opens a dialog captures the trigger as that dialog's
  opener. Outside pointer dismissal restores nothing: focus belongs to
  what was clicked, and the alternative (restore in `on_cleanup`) races
  the browser's own focus move across the disposal microtask.
- **`FOCUSABLE` excludes explicit negative tabindex on every arm.** The
  selector's `button:not([disabled])` matched a `tabindex="-1"` button, so
  a modal's Tab cycle would have walked every roving item. This is a
  standing overlay bug the roving contract depends on; it is fixed here
  with its own native test.
- **Tabs are a tablist, not toggle buttons.** (Human ruling.) `aria-pressed`
  announces an exclusive view switch as independent toggles.
  `role="tablist"` wraps ONLY the tab buttons; the spacer, the trailing
  action slot and the drawer's metadata stay siblings outside it. Each tab
  is `<button role="tab" aria-selected>` with roving tabindex;
  Left/Right/Home/End move focus; activation is MANUAL (Enter/Space)
  because trawl's tabs write `?ntab=` to the URL and automatic activation
  would push a history entry per arrow press. No `aria-controls`: `Tabs`
  does not render panes and ARIA does not require the relationship. The
  accessible name is a REQUIRED `label` prop (human ruling), forwarded by
  `Drawer` as a required `tabs_label`, so an unnamed tablist cannot
  compile.
- **⌘⇧L is not bound.** (Human ruling, amending ADR-0025.) Ctrl/Cmd+Shift+L
  is Bitwarden's default autofill chord and Safari's ⇧⌘L; a page keydown
  cannot beat an extension command, so the chord would autofill for some
  operators and toggle the theme for others. The hint chip leaves; the
  theme item stays a plain menu command; no replacement chord and no
  platform-glyph helper in this slice.
- **The ⌘K stub is untouched.** (Human ruling, against both design legs.)
  Its title and box wait for the palette slice, G4.
- **CSS resets are per rule, never global.** The sheet's only button rule
  is `font: inherit`; a global reset would restyle every consumer's own
  button recipe. Each converted rule gains its reset (the `.sd-x` /
  `.btn-icon` / `.actions-menu .item` precedent), preserving its declared
  weights. The two tab rules are byte-pinned in the premigration fixture;
  those two bodies are re-captured with a dated note and a new parity test
  pins the reset declarations so the re-capture is not a blank cheque. The
  bell's `.iconbtn` rule and the `.user-wrap > .overlay` scrim rule are
  deleted with their markup; `Icon::Bell` stays a library glyph.
- **Copy bare mode is a native button that the caller's class styles
  entirely.** The `class` prop and the propagation stop are unchanged; the
  contract now says the caller owns the button reset. trawl's
  `.editor-tools button.tool` already does; coastwatch's `.copy-inline`
  gains it in its `TRAWL_REV` bump PR.
- **Icon-only controls carry names.** Modal close is "Close dialog", toast
  dismiss is "Dismiss notification" with the glyph hidden, the `ActionsMenu`
  trigger is "Actions", the topbar trigger's name is the visible user name
  and its menu is labelled "Account".

## Consequences

- Public Rust call surfaces of `Shell`, `TopBar`, `UserInfo`, `ActionsMenu`,
  `ActionItem`, `Modal`, `Toasts`, `ToastBus` and `CopyButton` are
  unchanged. `Tabs` gains a required `label`; `Drawer` gains a required
  `tabs_label`. trawl-web-ui's three tab call sites change; coastwatch
  uses neither.
- coastwatch's chrome loses the bell, the two disabled rows and the ⌘⇧L
  chip on its next bump, and its `.copy-inline` must style a button. The
  bump PR carries both.
- Out of scope and named: toast `aria-live` (an announcement policy, its
  own decision), `Btn`'s missing `type` attribute (a form-submission change
  for unrelated consumers), a `prefers-reduced-motion` rule (nothing here
  touches the fade), and a browser proof of a menu INSIDE a modal (no such
  composition exists in trawl-web-ui and a harness-only route would make
  the e2e build differ from the shipped SPA). The last is proven natively
  and declared a residual.
- Automated proof is Chromium-only through the trawl-web-ui harness; other
  engines and assistive technology are not claimed.
