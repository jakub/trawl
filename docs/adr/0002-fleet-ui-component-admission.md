# fleet-ui component admission: generic-by-nature, single-consumer OK

**Status:** Accepted

fleet-ui (coastwatch ADR-0030) was extracted from trawl-web-ui's "Herring"
design system with a conservative admission rule: components graduate to the
shared crate only once a second app consumes them. ADR-0030 explicitly deferred
the generic `<Modal/>` primitive on that basis ("extracting a `<Modal/>`
primitive without a second consumer risks designing for one app's quirks"),
with the trigger "revisit when coastwatch grows its own non-confirm modal."

Reality since the extraction:

- The deferral trigger fired. Coastwatch hand-rolled its own modal
  (`ConfirmDialog`, coastwatch `web-ui/src/pages/ops.rs`) rather than wait for
  the primitive — the two-consumer rule produced the exact copy-paste it was
  meant to prevent.
- The same happened with toasts: fleet-ui's toast module is wasm-gated, so
  coastwatch re-ported trawl's toast system locally to keep `ToastKind` visible
  to host-tested pure logic.
- trawl-web-ui itself (the un-migrated ADR-0030 step-4 consumer) duplicates
  scrim/panel/tab/icon patterns internally — two drawers share a copy-pasted
  `sd-*` convention, four modals each reimplement scrim + Escape + close, and
  ~20 per-file inline-SVG icon components exist.

The two-consumer rule optimizes against over-abstraction, but in a
closed, lockstep, one-author fleet (ADR-0030 "Versioning and release cadence":
no semver, consumers migrate on `TRAWL_REV` bumps) the cost of a premature
abstraction is a cheap in-place API change, while the cost of waiting is
guaranteed divergence that later needs archaeology to merge.

## Decision

A component is admitted to fleet-ui when it is **generic by nature** — it
carries no app semantics (no app-specific types, copy, routes, or layout
constants) — even if only one app consumes it today. Concretely:

- Structural primitives (modal, drawer, tab strip, toast, error banner),
  form controls, icons, and chrome are fleet-ui material on first extraction.
- App-specific content composed *on* those primitives (trawl's export modal,
  coastwatch's quarantine views) stays in the app crate.
- Pure decision logic that apps must host-test (toast kinds, theme prefs)
  lives in native-compiling fleet-ui modules (the `theme::prefs` /
  `theme::runtime` split is the template); wasm-only gating is reserved for
  DOM-touching code.
- Clickable non-button elements (nav chrome rendered as `div`/`span`) are not
  retrofitted onto `Btn` during refactors — converting them changes DOM shape
  and belongs to deliberate visual/a11y work, not extraction passes.

This supersedes ADR-0030's `<Modal/>` deferral and its implied two-consumer
threshold. The lockstep/no-semver policy is unchanged and is what makes this
rule cheap: a mis-designed shared API is corrected in place and both consumers
absorb it on their next rev bump.
