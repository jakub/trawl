# Native controls, before and after (#161)

Every surface issue #161 converted, photographed at rest and with one
control focused, on both sides of the change. Same script, same stub
harness, same `corpus` fixtures, same viewport: the only variable is
which `dist/` the stub serves.

- before: `ecce7645` (ADR-0029 on main, no code change yet)
- after: `82042b23` on `feat/issue-161-trawl-web-ui-native-controls-stretched`. The
  SPA sources are unchanged from `d9b341fd`, the last commit that touched
  `src/` or `styles/`; the three commits after it are e2e patches, the
  mutation script and this evidence.
- viewport: 1440x900 CSS pixels, one device pixel each. No retina pass:
  none of this is about hairlines, and doubling 58 files buys nothing.
- theme: light only, `data-theme` untouched.

Reproduce, from `crates/trawl-web-ui/e2e/` with the SPA built:

```sh
node scripts/capture-native-controls-161.mjs --label after \
  --out ../../../visual-evidence/issue-161/captures/after

git worktree add /tmp/trawl-161-before ecce7645
(cd /tmp/trawl-161-before/crates/trawl-web-ui && trunk build)
node scripts/capture-native-controls-161.mjs --label before \
  --out ../../../visual-evidence/issue-161/captures/before \
  --dist /tmp/trawl-161-before/crates/trawl-web-ui/dist
```

`report.json` beside the PNGs in each directory is the machine-readable
half: per shot, whether the control was found, whether it took focus,
whether `:focus-visible` matched, and the computed `box-shadow` and
`outline`.

Two of those readings carry the argument.

The **converted** controls read `control absent` on the before side.
That is not a missing screenshot: the selector is a `<button>` or an
`<a href>` that the pre-#161 DOM has no element for, because the thing
that acted on the click was the `div`, `span`, `tr` or `th` around it.

The **five controls that kept an app-side outline** (`.deg-btn`,
`.fc-back`, `.fc-link`, `.url-notice-repair`, `.deg-x`) were already
buttons and already took focus. They read `ring + app outline` before and
`ring, no outline` after, because #161 deleted the five
`main.css :focus-visible { outline: … }` rules that painted a second
focus treatment over fleet-ui's ring (ADR-0007). `.fc-link` is the one
that still reports an outline after: the value is inherited colour with
`outline-style: none`, so nothing is painted. The pixels are in the PNGs.

Two shots need a word about the fixture rather than the code.
`nets-menu-open` opens the ActionsMenu over the row it belongs to,
because the corpus has exactly one net and there is no second row to
prove the overlay against. `facets-*` overwrite the first value's text
with 200 `x` characters in the DOM, exactly as `facets.spec.ts` does: the
geometry is the subject, and a 200-character hostname in the fixture
would change every other picture here.

| file (in `before/` and `after/`) | surface | before | after |
|---|---|---|---|
| `schema-table-rest.png` | Schema services table, at rest | at rest | at rest |
| `schema-row-focused.png` | Schema services table, row control focused; the quick actions reveal on `:focus-within` | control absent | ring, no outline |
| `nets-table-rest.png` | Nets table, at rest | at rest | at rest |
| `nets-menu-open.png` | Nets table with the row ActionsMenu open. The fixture has one net, so the menu sits over its own row | at rest | at rest |
| `runs-table-rest.png` | Runs table, at rest | at rest | at rest |
| `runs-row-focused.png` | Runs table, row control focused | control absent | ring, no outline |
| `history-table-rest.png` | Search history table, at rest | at rest | at rest |
| `history-row-focused.png` | Search history table, rerun control focused | control absent | ring, no outline |
| `results-table-rest.png` | Results table, at rest | at rest | at rest |
| `results-row-focused.png` | Results table, row caret control focused | control absent | ring, no outline |
| `results-row-expanded.png` | Results table, detail row open | at rest | at rest |
| `results-sort-header-focused.png` | Results table, sort header control focused | control absent | ring, no outline |
| `facets-rest.png` | Facet rail at rest, first value overwritten with a 200-character string | at rest | at rest |
| `facets-include-focused.png` | Facet rail, include control focused over that long value | control absent | ring, no outline |
| `editor-tools-rest.png` | Editor tools and the range trigger, at rest | at rest | at rest |
| `editor-tools-focused.png` | Editor tools, Format focused | ring, no outline | ring, no outline |
| `range-dialog-open.png` | Range dialog opened from the keyboard, focus on its first control | at rest | at rest |
| `status-bar-theme-focused.png` | Status bar, theme control focused | control absent | ring, no outline |
| `service-drawer-top-fields.png` | Service drawer overview, top-fields row focused | control absent | ring, no outline |
| `service-drawer-fields-rest.png` | Service drawer fields tab, at rest | at rest | at rest |
| `service-drawer-field-header-focused.png` | Service drawer fields tab, sort header control focused | control absent | ring, no outline |
| `service-drawer-degraded-badge-focused.png` | Service drawer fields tab, degraded badge focused (`.deg-btn`) | ring + app outline | ring, no outline |
| `field-case-back-focused.png` | Field case drawer, back control focused (`.fc-back`) | ring + app outline | ring, no outline |
| `field-case-link-focused.png` | Field case drawer, other-running link focused (`.fc-link`) | ring + app outline | ring, no outline |
| `url-notice-repair-focused.png` | Malformed-link notice, repair control focused (`.url-notice-repair`) | ring + app outline | ring, no outline |
| `degraded-notice-dismiss-focused.png` | Degraded results notice, dismiss focused (`.deg-x`) | ring + app outline | ring, no outline |
| `net-drawer-rename-focused.png` | Net drawer, rename control focused | control absent | ring, no outline |
| `net-drawer-presets-focused.png` | Net drawer schedule form, interval preset focused | control absent | ring, no outline |
| `net-drawer-run-row-focused.png` | Net drawer runs tab, run row control focused | control absent | ring, no outline |
