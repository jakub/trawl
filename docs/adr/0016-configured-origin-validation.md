# Fleet origin validation: configured public origins, no forwarded trust

status: accepted (2026-08-18) — prep ruling record for #92; amended (2026-11-23): rulings moved from code comments, see the Amendment

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

## Amendment (2026-11-23): rulings moved from code comments

A comment audit on 2026-11-23 removed these rulings from the code, where they were the only record. They were already in force. None is a new decision.

**Where the guard runs**

- **The guard runs before any step that reads a key or a cookie.** `fleet_auth::login`, `fleet_auth::logout` and trawl-web's `Session` extractor call `check_origin` before they verify a key, look up the cookie, decrypt it, or check its expiry. Steps outside the handler can still answer first: trawl-web's `envelope_login_body_limit` layer and the `Json<LoginRequest>` extractor can refuse a login request before its handler starts. A refused request is a 403 with no `Set-Cookie` of any kind. If logout built its clear directive first, or the extractor answered an expired cookie first, any page could clear the shared `fleet_session` and sign the user out of every sibling app.
- **In trawl-web, the `Session` extractor is the guard for every cookie-authenticated route.** A handler cannot get a `Session` or a cookie-backed `Auth` without passing the guard, so a new route, an SSE route included, cannot forget it. The login and logout routes in `routes/auth.rs` call `check_origin` themselves because they have no session to extract: login has no cookie yet, and logout is discarding one.

**What the guard reads and logs**

- **`check_origin` takes the whole `HeaderMap` and reads only the `Origin` field.** Two `Origin` fields are a refusal even when they are byte-identical. A browser sends at most one, so a second field means something between the browser and the server appended it, and the next request through that intermediary may not carry a copy. Picking one value, as `HeaderMap::get` does, answers for a request nobody sent. The map, not a string, is the parameter so that no caller decides the duplicate and non-UTF-8 cases on its own.
- **The `warn` line that `check_origin` logs for a refusal carries three bounded fields.** The fields are `handler`, a `&'static str` label from the caller; `reason`, from the closed vocabulary in `fleet-auth/src/origin.rs`; and `origin`, present only when the header parsed. The raw header text never reaches the log, because an attacker chooses it and could use it to bury other evidence, fill log storage, or put terminal escapes in front of whoever reads the log. A parsed origin is logged in the form `Origin`'s `Display` prints, which parses back to the same value, so an operator can paste it into `public_origins`. In trawl-web, `ProxyError::OriginMismatch` also logs a fixed `proxy origin mismatch` warning that carries no header data.

**The origin parser**

- **`Origin::parse` is hand-written and refuses input it does not recognize.** The `url` crate was rejected. It repairs malformed input so that a browser can navigate to it, which is wrong for a security check, and it brings in IDNA tables that a wasm-clean, dependency-light feature does not need. There is no path, userinfo or IDNA handling, because an origin has no path or userinfo and the config file states ASCII. A non-ASCII host is refused with a hint to write the punycode `xn--` form.

**Configuration**

- **A `FLEET_SESSION_PUBLIC_ORIGINS` value replaces the `[web] public_origins` list and never merges with it.** A merged list would let a stale config entry keep authorizing an origin the operator believes they moved away from. trawl-web records how many file entries the environment replaced, so the displacement is visible.
- **The startup error for a missing list names both `[web] public_origins` and `FLEET_SESSION_PUBLIC_ORIGINS`.** A packaged install can own the config file, and then the environment is the only setting the operator controls.

**The no-forwarded-trust scan**

- **`crates/fleet-auth/tests/no_forwarded_trust.rs` scans the source of `crates/fleet-auth/src`, because a behavioural test cannot prove an absence.** A `Host` fallback or an `X-Forwarded-Proto` tiebreak would pass every row of `tests/origin_guard_truth_table.rs` and still make the verdict depend on the network. The scan reads every `.rs` file in full and skips comment lines, so the docs that explain why these headers are ignored can name them. A unit test under `src` therefore cannot plant a `Host` or `X-Forwarded-*` header. Those tests live in `tests/origin_guard_truth_table.rs`, which the scan does not enter.
- **The scan covers fleet-auth only.** trawl-web calls `check_origin` in three places: the `Session` extractor in `middleware/session_extractor.rs`, and the login and logout handlers in `routes/auth.rs`. Review reads all three whole. `routes/proxy.rs` must name `host` in its hop-by-hop strip list so that the browser's `Host` does not reach trawld, and a scan of trawl-web would fail on that correct code.
- **The scan stops honest reintroductions, not deliberate evasion.** An honest reintroduction spells the header the ordinary way, such as `header::HOST` or `"x-forwarded-proto"`, and the scan fails it at `cargo test`. A split string literal, `concat!`, or a header name built at runtime passes the scan. `DOCUMENTED_NON_GOALS` lists those shapes, and a test asserts that the scan does not detect them, so the limit is on record. No token or syntax-tree scanner is added, because none can catch a header name computed at runtime. Code review is the control against a hostile contributor. Review stays small because `check_origin` receives only a `HeaderMap`, a `PublicOrigins` and a `&'static str` label, and the one question is which fields it reads from that map. The answer must stay `Origin` alone.
