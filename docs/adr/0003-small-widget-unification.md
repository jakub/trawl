# small-widget unification: slice C relaxes zero-visual-change

**Status:** Accepted

Slices A (#27/PR #29) and B (#28/PR #30) of the fleet-ui migration were gated
as strictly zero-visual-change refactors: a pixel-diff parity harness
([retired visual-parity script](https://github.com/jakub/trawl/blob/8625503c3e3a75436d906fc093bb72a94dbd3939/scripts/web-visual-parity)) had to report 0 unexplained deltas. That
discipline fit those slices — they moved big structural chrome whose look was
already settled, and pixel identity was the cheapest proof the refactor was
pure.

Slice C graduates the remaining generic-by-nature small widgets (ADR-0002
admission): sparkline, toggle switch, status dot, badge, segmented control,
pager footer, search input, tri-state loader, kbd chip, actions menu. Unlike
the slice-B chrome, these exist today as *divergent* hand-rolled variants:

- three segmented controls with different sizes and active treatments
  (`.fmt-btn` 12px amber-active, `.seg-mini` 11px neutral-active, plus the
  date-range tabs);
- two pager footers differing in panel tone and padding (`.tbl-foot` is
  literally commented "mirror of `.results-footer`");
- 22 badge call sites setting colors via per-call inline `style=` strings;
- 22 hand-written loading/error tri-state matches, each with bespoke copy.

Preserving pixels here would force the shared components to *parameterize
accidental divergence* — axes whose only purpose is to reproduce drift.

## Decision

- **Slice C unifies instead of preserving.** One canonical look per widget:
  segmented control keeps a size axis but gets a single amber-wash active
  treatment (amber is the fleet accent — toggle, tabs, and primary `Btn`
  already key on it); the pager standardizes on the `.results-footer` look;
  loader copy normalizes to standard loading/error presentation.
- **Badge color is a `Tone` enum over fleet tokens** (neutral / info /
  success / warn / danger family). Apps map domain kinds → tones; inline
  color passthrough is not offered. A mis-fitting palette is a cheap in-place
  API change under the lockstep model (ADR-0002), whereas an escape hatch is
  permanent palette drift.
- **Slice C is not pixel-gated.** A holistic cross-app design pass (trawl +
  coastwatch, with dedicated design tooling) is planned after extraction;
  today's pixel layout is therefore not a contract worth freezing. PR
  evidence is targeted before/after captures of the changed surfaces, not a
  parity ledger.
- **The actions menu graduates with a behavior improvement**: wiring it to
  fleet-ui's overlay arbitration stack gives it Escape and outside-click
  dismissal it currently lacks. Sanctioned as part of unification, not scope
  creep.

Zero-visual-change remains the default discipline for future structural
moves; this ADR sanctions deviation for the slice-C widget set specifically,
in anticipation of the design pass.
