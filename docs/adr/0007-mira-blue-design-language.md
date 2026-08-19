# ADR-0007: Mira Blue design language

Status: accepted (2026-07-27)

> **amendment (2026-08-19, #119):** Geist 1.8.0 and Geist Mono 1.8.0
> are now self-hosted from two committed variable WOFF2 files owned by
> `fleet-ui`, with the advertised axis constrained to the standing
> 400/500/600/700 range. Both Trunk distributions copy the same font
> directory, and generated design cards inline those committed bytes as
> data URLs to preserve their single-file contract, including the OFL text
> alongside the embedded font software. The Google Fonts runtime dependency
> and its CSP allowances are deleted; source, SHA-256 checksums and OFL-1.1
> attribution live with the assets in `crates/fleet-ui/fonts/`.

## Context

ADR-0005 established the design-workbench process and the slate/blue palette
(cool grey surfaces, Open Sans / Fira Code, `--fs-*` type rhythm). A research
spike into shadcn-derived component systems (Basecoat v1.0.2, rust-ui)
concluded that neither should be adopted as a dependency, but that Basecoat's
**Mira** style pack captures the control quality fleet-ui's buttons and inputs
lack. A live-capture prototype — the running trawl search page restyled by a
~200-line token/override skin, published as a claude.ai artifact with an
Original / Mira / Mira Blue toggle — validated that the look ports cleanly
onto fleet-ui's existing token vocabulary with zero markup changes. The
**Mira Blue** variant (Mira geometry and neutrals, fleet's blue accent) was
selected.

## Decision

Adopt Mira Blue as fleet-ui's design language, folded into `fleet-ui.css` and
`trawl-web-ui/styles/main.css` per ADR-0005's rule-by-rule discipline (never
an override layer, never a verbatim dump). Supersedes ADR-0005's palette and
font choices; its process (workbench, golden-fixture re-capture), type
rhythm, sentence-case label treatment, and density system are unchanged.

Key points:

- **Surfaces**: neutral zero-chroma OKLCH ramp. Light: `--bg` 98.5%, panels
  100%/97%/93.5%, lines 92.2%. Dark: `--bg` 14.5%, panels 18%/22%/26.9%,
  lines as white-alpha overlays (10% / 6%) rather than solid greys. The ink
  ramp follows the skin except at light `--ink-4`, which is contrast-bound
  the way `--red` is: it is a TEXT colour (the `--fs-micro` DEBUG level pill,
  the DSL editor's gutter numbers, `.editor-hd .dim`, `.divider`), and the
  skin's `oklch(70.8%)` measures 2.59:1 on `--panel` and 2.48:1 on the
  editor's `--fill` wash — under even the 3:1 non-text floor, and a
  regression on ADR-0005's `#82868e` (3.56:1). Light `--ink-4` therefore
  holds that tone's luminance as a neutral, `oklch(62%)` (3.64:1 / 3.48:1,
  and >=3.01:1 on every other light surface). Dark `--ink-4` is bound the
  same way: the skin's step (`oklch(50%)`) measures 3.12:1 on `--panel`,
  but the gutter digits actually sit on the `--fill`-composited editor
  surface, where it drops to 2.72:1 — under the floor. Dark `--ink-4`
  therefore holds `oklch(56%)` (3.51:1 on the composited fill, higher on
  every plain panel).
- **Accent**: fleet's blues stay — `#2a5c8a` light, `#5a9fd4` dark — with a
  new `--on-accent` token replacing hard-coded `#fff` button text. Dark mode
  keeps Mira's inversion: light-blue accent surfaces carry near-black
  (`oklch(16% 0 0)`) text. `--accent-2` flips role from a darker shade to a
  lighter tint (`color-mix(accent 85%, white)`), so the nine sites that used
  it as TEXT move to `--accent` — on the field-type pill wash the new tint
  measures 3.67:1, under the AA floor, against `--accent`'s 5.19:1.
  `--accent-soft` deliberately keeps its pre-Mira value (`#7ea6cc` light):
  the skin's companion move would land it .031 from the new `--accent-2` in
  oklab, and those two are the INT and NUM arcs of the schema drawer's
  field-type donut, where colour is the only encoding. For the same reason
  the tint mixes toward **white in light and black in dark**: the dark
  accent is already light-valued, so a white mix compresses the arc scale to
  .050 / .045 gaps — one flat light blue — where the black mix restores a
  monotonic `--accent-2` / `--blue` / `--accent-soft` ramp with no pair
  closer than .095 (light's tightest is .081). The direction inverts with
  `--on-accent`, not against it: dark steps the accent away from a dark
  surface.
- **Focus**: 2px solid ring (`--ring`, accent-tinted) replaces the 3px soft
  glow, via the existing `--shadow-glow` token and `:focus-visible` rule.
- **Radius**: tokenized — `--radius-ctl: 8px` (buttons, inputs, editor),
  `--radius-sm: 6px` (chips, ghost buttons, facet rows), `--radius-panel:
  10px` (cards, modals). All hard-coded `border-radius` px literals convert
  to tokens.
- **Controls**: buttons weight 500 (was 600), transparent 1px border,
  `color-mix` tint hovers, `translate: 0 1px` press (replaces
  `scale(0.96)`); destructive buttons are tinted (red text on ~10% red wash),
  never solid; inputs get translucent fills and ring focus. The fill is a
  per-theme `--fill` token: light mixes the border color at ~20%, dark states
  the equivalent white overlay (~6%) directly, because the dark border color
  is *itself* a 10%-alpha overlay and `color-mix(…, transparent)` multiplies
  alphas — mixing it would collapse the fill to ~2%. Tinting also makes
  `--red` a *foreground* over its own wash, so the light tone is
  contrast-bound rather than free: Mira's `oklch(57.7% .245)` measures
  3.97:1 on the resting wash and 3.31:1 on the hover wash, under the 4.5:1
  AA floor, so light `--red` is deepened to `oklch(48% .177)` (6.03:1 /
  5.06:1). Dark `--red` sits on dark panels and keeps Mira's lighter tone.
- **Fonts**: Geist / Geist Mono replace Open Sans / Fira Code. Since the #119
  amendment, fleet-ui owns pinned self-hosted WOFF2 assets and consumers copy
  that directory into their Trunk distributions; no runtime font request
  leaves the application origin.
- **Native widgets**: `color-scheme` is declared per theme and
  `scrollbar-color` set, so Firefox scrollbars and form controls follow the
  theme.
- Semantic hues (`--teal/--yellow/--green/--blue`, log-level colors) and the
  `--fs-*` scale are retained.

The contract tests remain the guard rail: `css_chrome_parity`'s golden
fixture is re-captured for this pass (the ADR-0005-sanctioned mechanism), and
its targeted `rule_body` expectations are updated to pin the new recipes so
the Mira Blue values become the enforced baseline.

## Consequences

- coastwatch consumes fleet-ui by pinned SHA (its ADR-0029/0030); it is
  unaffected until its next `TRAWL_REV` bump, at which point it runs its own
  adoption pass (visual QA + `coastwatch.css` reconciliation) as a follow-up
  in that repo.
- The golden fixture's byte-parity history resets at this pass; future moves
  are measured against the Mira Blue baseline.
- Any straggler CSS reading the retired assumptions (solid `#fff` on accent,
  2–3px radii) must be caught in the port's visual QA.
