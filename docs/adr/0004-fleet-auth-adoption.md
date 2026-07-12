# trawl adopts the fleet-auth postgres keystore; sqlite trawl-auth retires

status: accepted (2026-07-11)

Completes the auth half of ADR-0030 step 4 (canonical text in the coastwatch repo;
the UI half landed via #27/PR #29). trawld takes a **hard postgres dependency**:
API keys are verified against `fleet-auth`'s postgres keystore, the sqlite
`trawl-auth` keystore dies, and trawl gives up its "zero external services"
deployment posture in exchange for a single keystore across the fleet (SSO with
coastwatch) and the removal of the sqlite single-replica constraint documented in
the helm chart.

## Decisions

- **Hard cutover, no data migration.** Existing keys are re-minted via
  `fleet-admin`, vector ingest tokens re-issued, schedules recreated. No
  sqlite→postgres importer is built; this applies to homelab prod too.
- **The non-auth sqlite estate (history, saved queries, schedules, report runs)
  also moves to postgres**, into a separate `trawl` database on the shared CNPG
  cluster (mirroring coastwatch's per-app database; the `fleet` database stays
  substrate-only). Ends the dual-storage stack, returns `schedule.key_id`
  integrity to a single engine, drops rusqlite from the server (unblocks #12),
  and deletes the `trawl-auth` crate at the end.
- **trawl-web stays a thin proxy** (ADR-0023): it adopts `fleet_auth`'s
  `session` feature only — decrypt cookie, forward inner token as bearer, trawld
  verifies. No pg pool in the proxy. Copying coastwatch-web's full wiring
  (`require_session` + keystore) was rejected: coastwatch-web is the terminal
  authority for its requests; trawl-web sits in front of trawld, which must
  verify every bearer anyway (CLI/vector hit it directly), so proxy-side
  verification is a redundant pg round-trip per request with no security gain —
  and trusting the proxy instead would require a proxy↔daemon trust channel that
  is new attack surface. The substrate's canonical full-app wiring lands in
  trawld itself (`fleet_auth::require_bearer` replaces the hand-rolled
  middleware + AuthCache).
- **Full SSO now**: shared `fleet_session` cookie name, parent-domain scoping,
  session key shared with coastwatch-web. Session payload becomes fleet's
  app-agnostic shape (`role` leaves the cookie; the SPA gets it from whoami).
- **Audit poller ports to postgres polling** (same interval config, snapshots
  via the fleet-auth pool) — preserves trawld's key_created/key_revoked audit
  trail for out-of-process key changes. LISTEN/NOTIFY rejected for now: adds
  trigger/notify surface to the shared substrate that coastwatch doesn't need.
- **fleet-admin becomes the only key-management tool.** `revoke-grant` and
  `retype` are ported to fleet-admin first; trawl-admin sheds its `keys`
  subcommands and survives as the TLS cert generator only.
- **CI consolidates**: the postgres service container moves onto the main
  workspace nextest job (with `FLEET_TESTS_REQUIRED=1`); the dedicated
  fleet-auth-tests job dissolves into it.

## Sequencing (one PR per slice)

0. fleet-admin parity (`revoke-grant`, `retype`)
1. trawld keystore cutover: `require_bearer` adoption + policy layer moves
   into trawl-server, scheduler key-liveness via pg, audit poller port,
   `[auth] database_url` config, trawl-admin `keys` removal, CI pg service,
   minimal deb/helm `DATABASE_URL` wiring + helm init-auth rewrite (which is
   already broken against the current trawl-admin CLI).
2. trawl-web session swap + fleet_session SSO + shared-key packaging.
3. app stores → postgres (`trawl` database); `trawl-auth` crate deleted.

Transitional state between slices 1 and 3: history/saved/schedule stores remain
on sqlite, holding postgres key ids as plain i64s — but in a **fresh database
file**, not the legacy `auth.db`. Postgres and sqlite key ids are unrelated
sequences; reusing the legacy file would let a newly minted postgres key with id
*n* silently inherit sqlite key *n*'s history, saved queries, and schedules
(which would then auto-execute under the new identity) — cross-principal
disclosure, found in the codex design review. The legacy `auth.db` is
quarantined on disk at cutover and never read again; slice 3 deletes the
transitional file along with the crate.

## Slice 2 design decisions (2026-07-11 prep grill)

- **`[web] shared_domain`** is the SSO knob: optional string driving the cookie
  `Domain=` attribute, named to mirror coastwatch's `session.shared_domain` so
  operator docs can say "set the same value in both apps". Unset/empty →
  origin-scoped cookie (standalone mode). `SameSite=Lax` is hardcoded, not
  configurable — it is a correctness requirement for parent-domain SSO
  (ADR-0030), and the flip from trawl-web's current `Strict` is the explicit
  security-posture change ADR-0030 already mandates for this step.
- **Homelab SSO domain**: `{trawl,coastwatch}.fleet.lab.ktle.net` with
  `shared_domain = ".fleet.lab.ktle.net"`. coastwatch's committed prod value
  (`.fleet.home.lan`) predates this decision and must be updated in the
  coastwatch repo — a mismatched `Domain=` silently breaks SSO. `trawl-01.lab.ktle.net`
  remains the direct trawld API endpoint for CLI/vector bearer clients
  (cookie-free, unaffected).
- **Shared-key packaging is SSO-opt-in.** Both channels keep self-generating an
  app-local session key by default so standalone installs work with no
  1Password dependency. SSO = the operator provisions the shared key from
  `op://Homelab/Fleet session key/credential` (minted by
  `fleet-admin generate-session-key`): debian by overwriting
  `/var/lib/trawl/web.cookie`, helm via `web.cookieSecret.existingSecret`
  pointed at an `op inject`-provisioned Secret. The cutover runbook documents
  the procedure. Per coastwatch ADR-0038 the key stays an `op://` runtime
  reference — never committed ciphertext — because it is cross-repo shared.
- **Origin validation lands in the fleet-auth substrate, default-on.** The
  shared `fleet_session` cookie makes logout forgeable cross-site (fleet-auth's
  documented caveat: a forged POST to either app's `/api/auth/logout` clears
  the cookie for both). `fleet_auth::session` gains an origin-validation helper
  with present-only, strictly same-host semantics — Origin header present and
  its host mismatched against request Host → 403; absent → allow (browsers
  always send Origin cross-site, so the attack is blocked while curl/scripted
  logins keep working). The shared cookie's `shared_domain` is deliberately
  **not** an origin allowlist: a sibling app under the same parent domain is a
  different origin and is rejected, otherwise a compromised sibling could forge
  a fleet-wide logout. `fleet_auth::login`/`logout` enforce it by default (safe:
  absent-Origin passes), so coastwatch inherits the fix on rebuild; trawl-web's
  hand-rolled handlers call the same helper.

  **Accepted deviation from the issue #39 AC.** AC #6 as frozen required
  "subdomain-of-`shared_domain` passes" (and an approach bullet spoke of a
  `shared_domain` suffix match). That wording is self-defeating: the origin
  check exists precisely because the cookie is shared across the parent domain,
  so allowing *any* origin under that domain would re-admit the entire threat it
  closes — a compromised or attacker-hosted sibling auto-submitting a logout POST
  that clears `fleet_session` fleet-wide. We deliberately ship strictly same-host
  instead: siblings are rejected (403), the shared domain governs only the
  cookie's `Domain=` attribute, never who may hit auth endpoints. The
  `sibling_under_shared_domain_is_rejected` /
  `login_rejects_sibling_under_shared_domain` tests pin this reversal. All other
  AC #6 clauses (absent passes, same-host passes, mismatch → 403, default-on pg
  enforcement, trawl-web helper) hold as written.
- **Upstream auth mapping in the proxy**: trawld 401 (key revoked/expired
  fleet-wide) → clear the session cookie, session is dead everywhere; trawld
  403 (valid key, no trawl grant) → 403 with the cookie PRESERVED, mirroring
  coastwatch's no-grant semantics — clearing would log the user out of the
  sibling app where they do have access.
- **Login/me flow**: login keeps trawl-web's 200+JSON `{name, role}` response
  with SPA-driven redirect (fleet-auth's 302 handler needs a KeyStore and is
  unusable in the thin proxy). `/api/auth/me` stops reading `role` from the
  now-role-less payload and calls trawld `/api/v1/whoami` per request, exactly
  as login already does. `session_ttl_secs` (default 86 400) and both key
  knobs (`cookie_secret_path` raw bytes / `cookie_secret_env` base64) survive
  unchanged. Legacy `trawl_session` cookies are not cleaned up (dead name,
  expire at TTL); the app-switcher UI stays deferred per ADR-0030.

## Consequences

- Deploying trawld now requires a reachable postgres (CNPG in the homelab;
  documented external requirement for the .deb channel — apt install alone no
  longer yields a working server).
- Breaking config change: `[auth]` gains `database_url`; `db_path` survives
  only until slice 3. `auth_cache_ttl_secs` dies with trawld's AuthCache
  (fleet-auth's KDF cache has no TTL and revocation is checked per-request).
- All existing `flt_` tokens stop working at cutover; the token format itself
  is unchanged.
- fleet-auth gains a small substrate addition for trawld's scheduler: a
  live-key lookup by id returning active + unexpired state and fresh
  assignments (a bare `is_key_active` boolean is too weak — scheduled
  execution authority must also respect expiry and the current trawl grant).
- trawld's policy layer is mandatory middleware directly behind
  `require_bearer`, not per-handler convention: `require_bearer`
  deliberately authenticates keys from any fleet app, so grantless keys must
  be rejected before the rate limiter and `/whoami` see them.
- fleet-admin ships in the runtime docker image and the .deb (it is the
  migration/provisioning tool the helm init container and operators invoke;
  as of this ADR the Dockerfile does not install it).
