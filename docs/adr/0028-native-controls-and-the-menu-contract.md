# Native controls and the menu contract: a fleet-ui control is a `<button>`, a menu is a `None` layer with one tab stop, and tabs are a named tablist that owns no panes

status: accepted (2026-09-07) — prep ruling record for #100 slice D

> **amendment (2026-09-23, #242):** a `Drawer`'s dialog is named by a
> REQUIRED `label` prop (a reactive string), not by its title slot. The
> panel used `aria-labelledby` on `.sd-ttl`, so every control, badge and
> link a consumer put in the title became part of the dialog's name. A
> Chromium probe measured `Rename errors by host` (net drawer),
> `errors by host success` (run drawer, with the status badge appended)
> and `Back to nginx duration` (field case). The title slot is now visual
> only. The name is the bare entity name the consumer passes (human
> ruling): the net's name, `Run <id>, <net>` or `Run <id>` when the net
> name is unknown, the field's name, the service's name, and `Deleted
> net` when the net is gone. It stays fixed while the net drawer's
> inline rename edits, and it follows a successful rename. This matches
> the rule `Tabs` already follows: a fleet-ui dialog is named from a
> string the compiler requires, never from markup the consumer fills.
> `Modal` (a static title) and the command palette and range dialog
> (fixed `aria-label` strings) already comply. coastwatch's document
> drawer passes its document title in its next `TRAWL_REV` bump PR.

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

## Amendment: explicit theme preference, 2026-09-16

[Issue #196](https://github.com/jakub/trawl/issues/196) adds System to the
theme preference. The user chose one selector in the account menu and
removal of Trawl's footer theme control. The user also chose to preserve
every valid saved Light or Dark preference. A saved Light value can be
deliberate or incidental because the old runtime persists all preferences
together; the format cannot distinguish these cases. Neither is migrated.

The account menu contains a group named "Theme" with Light, Dark and
System choices, in that order. Each is a native button with
`role="menuitemradio"`, a visible selected indicator and `aria-checked`
derived from the preference. System stays checked when its resolved theme
changes. Sign Out remains a separate command. The group's label and
separators never take focus. The menu's one roving tab stop and item walk
include both `menuitem` and `menuitemradio`. Opening the account menu
focuses the checked theme choice; command-only menus keep their existing
initial-focus behavior. ArrowUp/Down and Home/End move
focus only; click, Enter or Space selects the focused choice, closes the
menu and restores focus through the existing cause-specific contract.
Escape, Tab, outside dismissal, overlay arbitration and identity-loss
behavior remain as specified above. No theme shortcut is added. This
amends the earlier plain-command theme choice, including ADR-0025's rule.

Fleet distinguishes `ThemePreference` with System, Light and Dark from
the binary `Theme` used by renderers. Consumers read the resolved theme
and change the preference through its own setter. CSS, charts and
Atmosphere continue to receive only Light or Dark. The workbench's
always-visible demonstration control uses explicit preferences so it can
still exercise a mounted login backdrop without a binary theme writer.

The existing per-consumer storage namespace and JSON `theme` field remain.
The field accepts `system`, `light` and `dark`. Missing or invalid values
default to System. Malformed storage does not crash startup or get repaired
by an initialization write. Parsing a valid theme does not depend on the
validity of unrelated fields. Initialization, operating-system changes and
reselecting the current preference write nothing. A real preference change
persists the preference snapshot, so changing another reading preference
while in System mode stores `system`, never the resolved color. Storage
failure leaves the current session usable without claiming persistence.

System resolves from `prefers-color-scheme: dark`; an unavailable media
query resolves to Light. Fixed Light and Dark ignore media changes. The
runtime owns one media-query listener per preference installation and
removes it at cleanup. It registers the listener before its final sample
of the current query, so a change during startup is not lost. Runtime
installation re-reads storage and the current query rather than trusting
the bootstrap's earlier sample.

Trawl and the Fleet workbench load one shared, same-origin, classic script
before styles and Wasm. Trunk emits a content-hashed asset; it is neither
async, deferred nor a module. Each consumer supplies its storage namespace
on the script element. The bootstrap reads only, uses the runtime's parsing
and fallback rules, and sets the binary `data-theme` on the root element.
Existing CSS owns `color-scheme`; the bootstrap adds no permanent inline
override. Equal inputs produce the same styled appearance before Wasm and
after runtime installation. Later changes to storage availability or OS
appearance can legitimately change that result. No new theme transition
is introduced. A missing script or disabled JavaScript leaves the static
Light fallback visible; correct first paint is not claimed for that case.

This is one Trawl PR, including Fleet, Trawl and the workbench. Verify
Coastwatch's existing readers against the candidate Fleet revision in a
disposable copy and record both source revisions. Do not modify Coastwatch
or its CI pin in this issue. A Trawl-owned adoption guide explains the
external script, consumer namespace and placement under the existing CSP.
Coastwatch's unchanged HTML has no first-paint guarantee from this change.

Proof must exercise the shared parsing cases in Rust and the real bootstrap,
storage write counts, live media changes and listener cleanup, menu keyboard
behavior, and existing chart/backdrop state retention. First-paint evidence
must inspect computed styles with Wasm delayed, then compare the runtime
handoff under production asset serving and CSP. A final DOM attribute alone
does not establish first-paint behavior.
