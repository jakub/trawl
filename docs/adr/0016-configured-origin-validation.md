# Fleet origin validation: configured public origins, no forwarded trust

status: accepted (2026-08-18) — prep ruling record for #92

`fleet_session` is one cookie shared across fleet apps, so state-changing
cookie-authenticated endpoints (login/logout, and every mutation the
trawl-web proxy forwards) are CSRF targets. The shipped guard
(`fleet-auth/src/session.rs::origin_allowed`, ADR-0004 slice 2) is
present-only and **host-only**: scheme ignored, ports stripped. That blocks
the principal attack (a different or sibling host auto-submitting a form
POST) but admits same-host cross-scheme and cross-port forgery — an active
attacker serving `http://trawl.example` or another port of the same name
passes. Naive scheme comparison cannot fix it: behind the packaged
TLS-terminating proxy the backend sees `http` while the browser origin says
`https`.

## Decision

**A present `Origin` is compared — scheme, host, and effective port,
normalized — against an explicitly configured list of the deployment's
browser-visible origins. No forwarding header is ever consulted.**

- `[web] public_origins = ["https://trawl.example.com"]` — parsed and
  validated at startup. `trawl-web` with the web surface enabled and an
  empty list **fails startup** with an actionable error; there is no
  host-only legacy mode (silent retention of the defect) and no
  empty-means-reject default (silent browser breakage). Helm/Debian
  templates gain the knob; Helm refuses to render `web.enabled=true`
  without an entry and never derives it from ingress hosts.
- Normalization: lowercase scheme/host, effective default ports
  (`https://x` ≡ `https://x:443`), IPv6/IPv4 canonical forms, every
  non-default port distinct. Exactly one serialized `http`/`https` origin
  is accepted; `null`, lists, multiple `Origin` fields, wildcards,
  credentials/path/query are rejected. One parser for config and header.
- `Forwarded`/`X-Forwarded-*` are ignored **even from loopback peers** —
  the decision never depends on anything a client can type or a proxy must
  sanitize. If forwarding-derived origin is ever added later, peer
  allowlisting AND documented delete-then-set sanitization are
  prerequisites, not hardening.
- **Absent `Origin` stays allowed.** This is a browser CSRF control, not
  client authentication: browsers always stamp cross-site POSTs;
  curl/scripted clients send none and keep working.
- **Sibling fleet apps stay rejected.** Sharing the SSO cookie's domain
  governs cookie reach, never mutation authority; each app configures only
  its own origins.
- The check applies to fleet-auth's own login/logout handlers AND every
  cookie-authenticated trawl-web route — including the SSE streaming
  routes, which today bypass the generic forwarder's guard.
- The policy lives in fleet-auth (`SessionConfig`), consumed by trawl-web
  and by coastwatch in its own PR — one implementation, per-app origin
  lists.

Deliberately loud upgrade: an existing `trawl-web` install must state its
public origin once before restart. Release notes and template errors carry
the migration line.
