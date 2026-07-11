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
on sqlite at `[auth] db_path`, holding postgres key ids as plain i64s.

## Consequences

- Deploying trawld now requires a reachable postgres (CNPG in the homelab;
  documented external requirement for the .deb channel — apt install alone no
  longer yields a working server).
- Breaking config change: `[auth]` gains `database_url`; `db_path` survives
  only until slice 3. `auth_cache_ttl_secs` dies with trawld's AuthCache
  (fleet-auth's KDF cache has no TTL and revocation is checked per-request).
- All existing `flt_` tokens stop working at cutover; the token format itself
  is unchanged.
- fleet-auth gains a small substrate addition: a key-liveness check by id for
  trawld's scheduler.
