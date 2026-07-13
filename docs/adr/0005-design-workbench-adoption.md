# ADR-0005: design-workbench pass adoption (slate/blue retheme)

- Status: accepted
- Date: 2026-07-13
- Deciders: jakub

## Context

The fleet-ui design system shipped with the "Herring" warm-paper +
amber palette inherited verbatim from trawl-web-ui (ADR-0030). Visual
iteration on a leptos CSR crate is expensive — every tweak costs a
trunk/wasm rebuild — so a cheaper medium was built first:
`cargo xtask design-cards` projects the component library into static
HTML preview cards (real stylesheet, mirrored markup) pushed to a
claude.ai/design project. A full design pass ran in that workbench and
came back as a handoff (`fleet-ui.css` + `CHANGES.md`).

The handoff file itself was the *gallery copy* — comments stripped,
three app-critical rules dropped (`html, body` reset, `.topbar
.iconbtn::after` hit-area, `.user-wrap > .overlay`), and its new
font-size mappings appended as trailing cascade overrides. Adopting it
verbatim would have shipped a broken shell and destroyed the
stylesheet's documentation.

## Decision

Fold the handoff's **semantic deltas** into the existing commented
stylesheet, never replace the file. The pipeline is now: cards are
derived artifacts → workbench passes come back as handoffs → handoffs
are folded, rule by rule, into `crates/fleet-ui/styles/fleet-ui.css`.

The pass itself:

1. **Slate/blue palette.** Light theme: cool grey surfaces, neutral
   ink ramp, blue accent `#2a5c8a`. Dark theme was NOT in the handoff
   and was derived here: surfaces/lines were already cool and stayed,
   the warm cream ink ramp was neutralized, accent follows the dark
   `--blue` (`#5a9fd4`). The accent hue deliberately equals `--blue`
   in both themes; the tokens stay separate because they name
   different roles (brand accent vs info semantic) that merely share
   a value today.
2. **`--amber*` → `--accent*` hard rename** (breaking for consumers).
   A blue value living in a token named "amber" is a lie to every
   reader. No compat aliases — trawl migrated in the same branch,
   coastwatch follows in its own commit. The `class="amber"` brand
   span became `class="accent"`. Trawl's intel color-var vocabulary
   strings (`"--amber"` → `Tone::Warn`) became `"--yellow"`, since
   they always meant the warning semantic, not the accent.
3. **Type rhythm tokens.** One base size drives the scale:
   `--fs-title/section/base/control/label/small` (base 14.5px, golden
   ratio up-steps, 13px readability floor; derivation formula in the
   stylesheet comment). Folded INTO the component rules as bare
   `var()` reads — the handoff's trailing-override structure was
   rejected per its own recommendation. `--table-fs`/`--editor-fs`
   are exempt: data-dense surfaces stay compact.
4. **Label/title treatment tokens.** `--label-font/transform/variant/
   spacing` + `--title-weight/transform/spacing`, set to sentence case
   (the uppercase-mono label treatment was rejected in the workbench).
5. **Fonts:** Open Sans / Fira Code, loaded by consumer index.html
   (CSP already allowed Google Fonts).
6. **Component fixes:** `.bdg.warn` reads `--yellow` (warnings keep
   their semantic color regardless of accent hue); toast padding/
   line-height pass; and the `.toast .body` / Shell-chrome `.body`
   collision was root-caused by renaming the toast's inner element to
   `toast-body` (markup + CSS + card emitter) instead of adopting the
   handoff's display-override band-aid.

The chrome byte-parity golden fixture
(`fleet-ui/tests/fixtures/premigration-chrome.css`) was re-captured on
this baseline per its documented procedure — the deliberate deltas are
exactly the accent rename, the rhythm-token reads, and the label
treatment.

## Consequences

- Consumers referencing `--amber*` or `.amber` break until they
  migrate (coastwatch: one commit, tracked).
- The dark theme is a derived pass, not workbench-reviewed; it is
  gallery-verified but may get its own workbench session later.
- Future design passes iterate in the workbench against the same
  cards; CHANGES.md-style handoffs are folded, and the fixture is
  re-captured when deltas are deliberate.
- The TUI (ratatui) theme still carries the original amber — separate
  design surface, out of scope here.
