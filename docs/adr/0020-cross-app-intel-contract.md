# Cross-app intel: the resource server owns authorization, the relay carries the user

status: accepted (2026-08-18) — prep ruling record for #96; the intel
surface itself is retired until commissioned

PR #9 gave the trawl SPA a Coastwatch lineage/derivation view on two
stand-ins: derivation writes gated on **trawl's** admin permission, and
hand-vendored response types with no contract test. Prep found the ground
truth is worse: **the coastwatch API this surface targets does not exist**
on coastwatch main — no lineage/derivation routes, no matching wire types,
no lineage permission in its closed enum, and trawl forwards to a `/v1/*`
prefix nothing serves. The vendored `coastwatch-api-types` crate is
aspirational. A plausible-looking wrong-app permission gate over a
nonexistent API is worse than an absent feature.

## Decisions

1. **The trawl Intel surface is retired now** (SPA pages, intel API
   client, the `/api/intel` relay route, the vendored crate). Git history
   keeps the code; re-commissioning starts from this ADR, not from rotted
   stand-ins. Coastwatch-side commissioning (domain, routes, permissions,
   wire types) belongs to that repo's tracker when the feature is wanted.

The following rulings are **banked** for that commissioning, so it starts
from settled ground:

2. **The resource server owns authorization, exclusively.** Coastwatch
   enforces its own `coastwatch:*` permissions on every request (new
   variants in its closed enum, seeded by its boot). Trawl gates nothing:
   a hidden button is UX, never a boundary. No fleet-wide permission is
   ever minted — ADR-0006's per-app namespacing extends to the cross-app
   request path.
3. **Cross-app browser traffic is server-relayed with the user's own
   fleet key** (trawl-web's existing cookie→bearer mechanism), never the
   SPA calling the sibling's origin on the shared SSO cookie — credentialed
   CORS from a sibling would let any XSS on one app drive the other's
   whole cookie-authenticated surface, and sibling origins are not an
   allowlist (ADR-0016). The relay is **path-allowlisted and
   version-pinned** (exact methods/paths, no wildcard, redirects off,
   bounded bodies/timeouts); it never becomes a general proxy to the
   sibling's API root.
4. **Reads in trawl, writes deep-link.** Lineage views relay through
   trawl; mutations (invalidate/retract — editorial acts) happen in
   coastwatch's own UI via deep links. Smallest cross-app write surface.
5. **Wire types are producer-owned and consumed by sibling path dep**, the
   mirror of coastwatch's existing dep on the fleet crates. The producer
   commits per-endpoint JSON snapshot tests; the consumer commits decode
   fixtures over those snapshots, so serde-attribute drift fails CI where
   the compiler cannot. Hand-vendored copies are forbidden. (A full
   OpenAPI/generated-client + cross-repo CI apparatus was considered and
   rejected as ceremony for a two-repo, one-operator fleet.)
6. **Advisory capability display** comes from `/whoami` publishing the
   caller's full per-app permission map (the caller's own grants — not a
   leak); a stale badge self-corrects by re-fetching on a 403. Login never
   depends on coastwatch liveness.
7. **A sibling being down or unconfigured degrades to a named, visible
   panel** — never an empty result, and never a cleared `fleet_session`
   (a sibling 401/403 does not prove the key is dead).
8. **No automatic retry of cross-app mutations after an ambiguous
   failure** (timeout = outcome unknown): refetch state first; idempotency
   keys before any automated retry.
