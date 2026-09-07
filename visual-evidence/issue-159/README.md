# Issue #159: native controls and the shared menu contract (ADR-0028)

Browser evidence for the fleet-ui half of slice D: the topbar account
menu rebuilt on the shared menu contract, both tab strips as named
tablists, and the modal close / toast dismiss / bare copy button as
native buttons.

Everything here was captured at commit
`c5ba6a4468e134fc7d289199deb16b9d8dec664b`, the head of the mutation
checkpoint. This commit is that one's child, so the pinned SHA is not
the branch tip and is not meant to be.

Environment: chromium 151.0.7922.34 (the browser
`@playwright/test` 1.62.1 installs), viewport 1440x900, locale en-US,
timezone UTC. The SPA was built with `trunk build` (dev profile) and
served by the suite's own zero-dependency stub, `e2e/harness/server.mjs`.

## Mutation transcripts

`mutation-check.sh` prints `PASS` when the MUTANT WAS KILLED. It is the
runner's verdict on itself, not a test result, so a table of `PASS`
lines is a table of dead mutants. A kill needs both halves in the same
run: the target spec EXECUTED and FAILED, and a control spec the
mutation does not touch PASSED. Exit codes cannot make that call on
their own, because Playwright records a browser launch failure as
executed-and-failed tests rather than as no tests.

| file | mutation | runs | killed |
|---|---|---|---|
| `mutation-08-transcript.txt` | `roving::next_index` answers `current`, so no navigation moves | 5 | 5 |
| `mutation-09-transcript.txt` | every menu item renders `tabindex="0"` | 5 | 5 |
| `mutation-10-transcript.txt` | the menu's Escape listener loses its `is_topmost` guard | 5 | 5 |
| `mutation-11-transcript.txt` | activation closes and runs the callback without restoring the trigger | 5 | 5 |
| `mutation-12-transcript.txt` | the toast dismiss reverts to a `<span class="x">` | 1 | 1 |

Each run block opens with its date, the HEAD sha, the command, and ends
with the runner's table plus the kill count so far.

08 through 11 got five runs each because what they break is where
`document.activeElement` ends up after a keypress, and focus can be lost
to a race that a network assertion would never see. Five separate
invocations, not one invocation repeated internally, so each run
rebuilds and re-launches.

Which assertion caught what is worth reading, since two of these
mutations fail the same spec:

- 08 fails `topbar-menu.spec.ts` "keyboard open focuses the first item
  and arrows walk with wrap" on `toBeFocused` after the first ArrowDown.
- 10 fails the same file's "first Escape closes only the modal above the
  menu" on the menu still being visible after the first Escape. Different
  test, different observable.
- 09 fails `actions-menu.spec.ts` on the tabindex VECTOR
  (`['0','-1','-1']` versus three zeroes). Asserting only the focused
  item would have let it live.
- 11 fails the same file on the last line: after Escaping the confirm
  dialog the trigger is not focused, because the dialog captured
  `<body>` as its opener.

`e2e-suite-transcript.txt` is the full suite on a pristine rebuild after
all 21 mutation runs: 39 tests, all green, `npm run test` in `e2e/`.

## Captures

Ten PNGs, five surfaces in light and dark. Taken by
`crates/trawl-web-ui/e2e/scripts/capture-native-controls.mjs` — under
`e2e/scripts/` rather than here because it is worth re-running whenever
the tab chrome changes.

The dark set is not `colorScheme` emulation. fleet-ui reads its theme
from localStorage through `theme::runtime` and never from
`prefers-color-scheme`, so the script reaches dark by activating the
account menu's own theme item, which makes the dark set evidence that
the item works.

- `account-menu-{light,dark}.png` — the account menu open from the
  keyboard, focus ring on the first item ("Switch to dark theme" /
  "Switch to light theme"). The identity header sits above the items,
  outside `role="menu"`. No bell, no Profile row, no API tokens row, no
  ⌘⇧L chip; the ⌘K box is still in frame, untouched.
- `results-tabs-{light,dark}.png` — the results strip after ArrowRight
  off the selected tab: the ring is on "Visualization" while "Events"
  keeps the accent underline and the selection. That is manual
  activation, in one picture.
- `drawer-tabs-{light,dark}.png` — the service drawer strip, same
  arrangement: ring on "Fields", "Overview" selected, and the
  `1.2k events · 66 KB · 2 fields` metadata outside the tablist.
- `modal-header-{light,dark}.png` — the export dialog's header with the
  close button focused and ringed.
- `toast-{light,dark}.png` — an informational toast with its dismiss
  button focused and ringed.

Both the modal and the toast are opened from the keyboard in the script.
Chromium paints `:focus-visible` only when the last interaction was a
keypress, so a mouse-opened dialog photographs as a focused close button
with no ring on it. The first capture pass did exactly that.

## The two layout questions

`tab-strip-geometry.txt` answers both with numbers, not by reading the
stylesheet. The script measures the shipped DOM, then unwraps the
`[role="tablist"]` element in place (same buttons, same classes, no
wrapper, so `.tablist`'s two rules stop matching) and measures again.
That flat DOM is exactly the pre-#159 strip under the current
stylesheet, so the two readings are a before/after on one page.

Does `.t`'s `margin-bottom: -1px` still lap the strip's bottom border
inside the nested `.tablist`? Yes. `underlineOverlap` reads 1 for every
tab, nested and flat: the negative margin pulls each tab's box 1px past
the strip's inner bottom edge, into the row the strip's 1px border
paints, so the tab's 2px underline covers that hairline instead of
floating above it. The outer bottoms coincide as a result (238.75 on the
results strip, 103.75 on the drawer strip). Zoom into `results-tabs-light.png` and you
can see it: the accent bar under "Events" sits on the same pixel row as
the hairline running away to its left and right.

Did the drawer strip's tabs or its meta text shift, now that a 4px gap
sits on both `.sd-tabs` and `.sd-tabs .tablist`? No, not by a pixel. Tab
left edges are 735 / 830 / 900 nested and flat; the meta text spans
1229 to 1426 in both. The gap count did not change. Flat, `.sd-tabs`
had five children and emitted four gaps: two between the three tabs,
one before the `flex: 1` spacer and one after it into the meta. Nested,
the tablist emits the same two between the tabs and `.sd-tabs` emits
the same two around the spacer, which is what absorbs them.

Neither claim needed a CSS fix, so the premigration fixture was not
touched.

## Limits

- Chromium only. No Firefox, no WebKit, and Chromium's `:focus-visible`
  heuristic is the reason the captures are keyboard-driven.
- The API is the stub harness, not trawld: canned fixtures, a stub
  `/api/auth/me`, no real session or keystore.
- No assistive-technology run. Roles and accessible names are asserted
  through Chromium's accessibility tree (`toHaveRole`,
  `toHaveAccessibleName`), which is not the same as a screen reader
  announcing them.
- A menu INSIDE a modal is proven natively only (the overlay stack's
  `none_layer_above_trap_keeps_the_trap` test). The browser proof here is
  the reverse arrangement: a modal above the menu.
- No visual regression diffing. These PNGs are a record of what shipped,
  not a baseline anything compares against.

## Reproducing

```sh
crates/trawl-web-ui/e2e/scripts/mutation-check.sh 08-menu-walk.patch
crates/trawl-web-ui/e2e/scripts/mutation-check.sh 09-menu-roving-tabindex.patch
crates/trawl-web-ui/e2e/scripts/mutation-check.sh 10-menu-topmost-escape.patch
crates/trawl-web-ui/e2e/scripts/mutation-check.sh 11-menu-restore-before-callback.patch
crates/trawl-web-ui/e2e/scripts/mutation-check.sh 12-toast-dismiss-span.patch
(cd crates/trawl-web-ui && trunk build) && (cd crates/trawl-web-ui/e2e && npm run test)
node crates/trawl-web-ui/e2e/scripts/capture-native-controls.mjs
```

`crates/trawl-web-ui/e2e/README.md` has the full mutation table and the
mechanism notes.
