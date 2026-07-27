# fleet-auth moves to roles-as-data RBAC; static role enums retire

status: accepted (2026-07-21)

Replaces the static `(key, app, role)` grant model (ADR-0004) with data-defined
RBAC: a role becomes a named, cross-app bundle of permission strings stored in
the fleet keystore, keys hold any number of roles, and effective permissions are
the union. Motivation: the deployment model is converging on a tiered SOC shape
(tier 1/2/3 — seniority maps to capability breadth), tiers need reshuffling
without deploys, and capabilities must compose across trawl and coastwatch on a
single key. The current model blocks all three: one role per (key, app) is
DB-enforced, role→permission tables are compile-time, and coastwatch's Operator
is deliberately not a superset of Analyst.

## Decisions

- **Schema**: `roles(id, name)` + `role_permissions(role_id, app, permission)` +
  `key_roles(key_id, role_id)`; `api_key_role_assignment` dies. Permissions are
  app-namespaced strings (`trawl:ingest`, `coastwatch:stories_read`), so one
  role spans apps — a tier promotion is one grant. Flat lists, no role
  inheritance: tier2 literally contains tier1's permissions (fleet-admin may
  grow a clone-role convenience). Union semantics across a key's roles.
- **The invariant line: roles are data, permissions are code.** A permission
  string exists because a handler checks it — each app keeps its compile-time
  `Permission` enum and parses only its own namespace, unknown strings ignored
  (fail closed). New role = fleet-admin command; new permission = deploy.
- **No users table.** Keys stay the principals; `name` identifies the owner by
  convention. A person with several keys assigns roles per key.
- **Kind pairing dies.** `kind_allows_role` (Human↔Analyst/Operator,
  Service↔SiemConsumer) is deleted; `kind` survives as informational metadata
  only (inventory/audit). Rationale: agents will work coastwatch queues with
  tokens doing formerly-human tasks — the human/service capability boundary no
  longer maps to reality. Reintroduction path if it bites: an `assignable_to`
  flag on roles.
- **Resolution lives in the substrate.** `verify_key` joins key→roles→
  permissions; `VerifiedKey` carries role names (display/audit) plus per-app
  permission unions (gates). `role_for()` and `RoleAssignment` are deleted —
  apps stop knowing about roles entirely. `require_session`'s per-request
  namespace check becomes "any permission in my namespace"; the bearer
  middleware stays authn-only (unchanged contract). Session cookie payload
  (`{token, name, exp}`) is untouched — role never entered it (ADR-0030), so
  SSO is insulated from this change by construction.
- **In-place conversion, no re-minting.** A schema migration auto-creates one
  role per distinct legacy `(app, role)` pair (named `trawl-admin`,
  `coastwatch-analyst`, …) carrying the permission list the owning app hardcodes
  today, links keys via `key_roles`, then drops the old table. Live coastwatch
  prod keys work through the deploy. (Deviation from ADR-0004's hard-cutover
  precedent: that predated live keys in the shared keystore.)
- **Warn-only permission registry.** New `app_permissions` table; each app
  seeds its vocabulary at migrate-time. `fleet-admin` role mutations warn on
  unknown permission strings but persist them (a role may name a permission an
  app is about to ship). Closes the silent-no-op typo hole — today fleet-admin
  does zero vocabulary validation.
- **trawl rate limiting re-keys from role to key.** Per-role shared buckets
  have no stable identity under roles-as-data, and were poor isolation anyway
  (all analysts shared one bucket). New model: one token bucket per key id;
  RPM ceiling = max of a nullable `rate_rpm` attribute across the key's roles,
  config default otherwise. Class-of-service moves onto the role row. (The
  config default turned out to need a per-route-class split, and `rate_rpm`
  then needs a stated precedence against it — see *Slice 0 design decisions*.)
- **trawl policy layer simplifies.** The `Role` enum, its role→permission
  table, and the dead `KeyManage` variant are deleted; `require_trawl_grant`
  becomes "holds ≥1 trawl permission"; handlers' `has_permission` call sites
  are unchanged in shape. `/whoami` keeps the resolved permission list (the
  wire is already permission-shaped) and gains role names for display; the
  TUI's `server_manage` visibility check keeps working as-is.
- **Coastwatch follows in a companion arc in its own repo** (this ADR's scope
  is the trawl workspace; sibling path-dep build is red between the arcs —
  accepted, per greenfield stance no compat shim is built; `role_for`'s
  one-role-per-app semantics would be a lie under multi-role anyway). That arc
  owes: wire `SystemConfigManage` (or new ops permissions) to replace the
  `require_role(Operator)` gates, a late-revise permission to replace the
  in-handler window/role check, `MeResponse` flips role → permissions,
  `rail_gate` re-keys on permissions, `AmbiguousGrant` and `kind_allows_role`
  deleted, permission vocabulary seeding. Its three reserved-but-unwired
  permissions get wired or dropped there.

## Sequencing (one PR per slice)

0. trawl rate limiter → per-key buckets (config-default RPM only, one default
   per route class; `rate_rpm` arrives with the roles table). Independently
   landable, removes the sole role-identity consumer in trawl-server ahead of
   the cutover. See *Slice 0 design decisions*.
1. the cutover: fleet-auth schema + store + resolution, conversion migration,
   registry, fleet-admin `roles` subcommands + `keys assign-role`/
   `unassign-role`, trawl-server policy simplification, trawl-web any-perm
   check, `trawl-api`/TUI wire updates, pinning-test rewrite.

Companion coastwatch arc is prepped separately in that repo after slice 1.

## Slice 0 design decisions (2026-07-26 implementation, #43)

- **The config default is per route class, not per server.** Collapsing the
  four per-role knobs onto one number is what the slice called for, but the
  only honest value for that number does not exist: the legacy ingest ceiling
  (1000 rpm, sized for vector shipping 1 MB / 5 s per source, sources commonly
  sharing one key) is ~10–30x the legacy interactive ceilings (reader 30,
  analyst 60, admin 100), and `/query`, `/export` and `/stream` each run a
  DuckDB scan with nothing else bounding a single principal. Sized for the
  shipper it is no ceiling at all for humans; sized for humans it throttles
  ingest. So `[rate_limit]` carries two defaults — `default_rpm` (interactive
  API routes) and `ingest_rpm` (`/api/v1/ingest`) — each with its own bucket
  map, wired by the router. Buckets are still keyed solely by `VerifiedKey.id`;
  the class is a property of the *route*, so per-role bucketing stays dead.
- **Ingest-class eligibility is gated on `Permission::Ingest`.** The handler's
  own permission check runs downstream of the rate-limit middleware (axum
  resolves every extractor, including the 16 MB `body: Bytes`, before the
  handler body runs), so an ungated ingest bucket map would hand *any*
  trawl-granted key — reader included — the shipper-sized ceiling on the
  heaviest endpoint. Keys without the permission fall back to the interactive
  map, literally the same one the query routes use, so touching `/ingest`
  cannot buy extra interactive budget either. This is route eligibility, not
  the class-of-service-by-role model this ADR retires: the limiter asks a
  permission question (`has_permission`), which is the call shape the policy
  layer keeps after the cutover, and it holds no `Role`/`trawl_role()`
  reference of its own — when slice 1 makes permissions data-backed, the
  limiter's question is answered from the roles table with no edit here.
- **Precedence, once `rate_rpm` lands (slice 1).** The route class picks the
  *default*; `rate_rpm` **overrides** it. Effective ceiling for a request =
  max of `rate_rpm` across the key's roles when any role sets it, otherwise
  that route class's config default. The two are never max()'d or summed
  together — a key whose roles set `rate_rpm` ignores both config defaults.
  Since the classes hold separate bucket maps, that one effective number is
  applied independently in each map the key touches (a key with
  `rate_rpm = 2000` gets a 2000-rpm interactive bucket *and* a 2000-rpm ingest
  bucket, spent separately). Implementation shape: limiter maps indexed by
  effective rpm within each class, buckets still keyed by key id — governor
  fixes one quota per keyed limiter, so a per-key ceiling cannot come from a
  single map.
- **Accepted consequence of that rule**: a role carrying a shipper-sized
  `rate_rpm` also loosens that key's interactive routes. Operators who need
  the numbers to differ put the roles on different keys — keys remain the
  principals (see "No users table"). If that bites, the escalation path is a
  class-scoped attribute (`rate_rpm_ingest`), *not* a precedence rule layering
  attributes over route-class defaults.

## Slice 1 addendum (2026-07-26 implementation, #44)

- **Log/wire role fields become operator-defined strings; prometheus keeps
  none.** Everywhere an AUTHENTICATED surface previously carried one of four
  fixed role names (`trawl_role()`), it now carries the sorted, comma-joined
  role-NAME list (`roles_display()`, `"none"` fallback; the
  `ActiveQuerySnapshot.role` wire field keeps its name). The prometheus
  `role` label on `trawl_queries_total` / `trawl_query_duration_seconds` is
  DROPPED instead of converted: `/metrics` is unauthenticated, and
  data-defined role names are operator-chosen and span apps, so the joined
  list would publish cross-app fleet role membership to any scraper and make
  the series count combinatorial in distinct role sets (a permanent
  histogram per new combination). Per-principal query attribution stays on
  the authenticated surfaces (structured logs, query log, history,
  `/api/v1/queries`). No principal-derived label may be added to a metric
  while `/metrics` sits outside the auth stack.
- **The conversion migration is the runtime cut point for both apps.** The
  ADR accepted a red coastwatch *build* between the arcs; the shared fleet
  database makes it a *runtime* window too — `api_key_role_assignment` is
  dropped in the same migration that creates the new tables, so a running
  pre-arc coastwatch fails at `fleet-admin migrate` time. Its companion arc
  ships in the same maintenance window. The migration also freezes
  coastwatch's permission wire strings as snake_case of its `Permission`
  variant names (recorded in the cutover runbook); that arc's `as_str` must
  match them exactly.
- **Registry seeding is migrate-time only.** trawl's 9 permission strings
  are seeded by the migration SQL; no boot-time vocabulary registration
  exists (parked — the warn-only registry makes staleness cosmetic).
  Coastwatch seeds its own namespace in its arc.
- **`require_trawl_grant` gates on *recognized* permissions.** "Holds ≥1
  trawl permission" is evaluated against trawl's compile-time vocabulary:
  a key whose roles carry only unrecognized `trawl:*` strings resolves no
  usable capability and 403s — the fail-closed doctrine applied at the
  door, matching the old unknown-role behaviour.
