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
  config default otherwise. Class-of-service moves onto the role row.
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

0. trawl rate limiter → per-key buckets (config-default RPM only; `rate_rpm`
   arrives with the roles table). Independently landable, removes the sole
   role-identity consumer in trawl-server ahead of the cutover.
1. the cutover: fleet-auth schema + store + resolution, conversion migration,
   registry, fleet-admin `roles` subcommands + `keys assign-role`/
   `unassign-role`, trawl-server policy simplification, trawl-web any-perm
   check, `trawl-api`/TUI wire updates, pinning-test rewrite.

Companion coastwatch arc is prepped separately in that repo after slice 1.
