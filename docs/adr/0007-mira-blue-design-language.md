# ADR-0007: Mira Blue design language

Status: accepted (2026-07-27)

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
  lines as white-alpha overlays (10% / 6%) rather than solid greys.
- **Accent**: fleet's blues stay — `#2a5c8a` light, `#5a9fd4` dark — with a
  new `--on-accent` token replacing hard-coded `#fff` button text. Dark mode
  keeps Mira's inversion: light-blue accent surfaces carry near-black
  (`oklch(16% 0 0)`) text.
- **Focus**: 2px solid ring (`--ring`, accent-tinted) replaces the 3px soft
  glow, via the existing `--shadow-glow` token and `:focus-visible` rule.
- **Radius**: tokenized — `--radius-ctl: 8px` (buttons, inputs, editor),
  `--radius-sm: 6px` (chips, ghost buttons, facet rows), `--radius-panel:
  10px` (cards, modals). All hard-coded `border-radius` px literals convert
  to tokens.
- **Controls**: buttons weight 500 (was 600), transparent 1px border,
  `color-mix` tint hovers, `translate: 0 1px` press (replaces
  `scale(0.96)`); destructive buttons are tinted (red text on ~10% red wash),
  never solid; inputs get translucent fills (`color-mix` of the border color,
  ~20% light / ~30% dark) and ring focus.
- **Fonts**: Geist / Geist Mono replace Open Sans / Fira Code. Loaded by the
  consumer's `index.html` from Google Fonts, same mechanism as before;
  self-hosting remains deferred.
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
