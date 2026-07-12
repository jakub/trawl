---
title: Fleet-Auth Cutover Runbook
description: Migrating a trawld deployment from the sqlite keystore to the fleet-auth postgres keystore (ADR-0004 slice 1).
---

Since ADR-0004 slice 1, trawld verifies API keys against **fleet-auth's
external postgres keystore**. The old sqlite keystore path is gone:

- trawld **will not start** until the fleet database is reachable and
  migrated (`fleet-admin migrate`).
- This is a **hard cutover with no data migration** — every existing `flt_`
  token stops working and must be re-minted. The token format is unchanged.
- Key management moved from `trawl-admin keys` (removed) to `fleet-admin`.
- The legacy `auth.db` is **quarantined in place**: postgres and sqlite key
  ids are unrelated sequences, so reusing the file would let a new key
  inherit another principal's query history, saved queries, and
  auto-executing schedules. trawld refuses to start while `[auth] db_path`
  points at a file named `auth.db` — repoint it at a fresh file (e.g.
  `store.db`) and leave `auth.db` on disk untouched.

## Expected impact

During the cutover window trawld rejects all tokens minted before the
cutover, so **vector agents buffer instead of shipping**. The ingest gap is
bounded by vector's disk buffers — make sure each vector instance carries a
disk buffer on its trawl sink (about 1 GiB is a sensible default for
homelab volumes; the same applies to vector sidecars in kubernetes):

```yaml
sinks:
  trawl:
    buffer:
      type: disk
      max_size: 1073741824   # 1 GiB
      when_full: block
```

Interactive queries fail with `401` until users switch to their re-minted
keys. Schedules must be recreated (see below).

## Runbook

1. **Migrate the schema.** Point `DATABASE_URL` at the fleet postgres
   database and apply the embedded migrations (idempotent):

   ```bash
   export DATABASE_URL='postgres://fleet:…@db.internal:5432/fleet'
   fleet-admin migrate
   ```

2. **Re-mint keys** for every principal — admin, human CLI users, and one
   service key per vector fleet:

   ```bash
   fleet-admin keys create --name ops-admin   --kind human   --grant trawl:admin
   fleet-admin keys create --name jakub-cli   --kind human   --grant trawl:analyst
   fleet-admin keys create --name vector-lab  --kind service --grant trawl:ingest
   ```

   Each command prints the plaintext token once; it is never recoverable.

3. **Distribute the vector ingest tokens** to every vector instance
   (`TRAWL_INGEST_TOKEN` in the vector env / secret), but do not restart
   vector yet if you want to minimize the buffering window.

4. **Update trawld's config and restart.** In `/etc/trawl/trawld.toml`:

   ```toml
   [auth]
   # or set DATABASE_URL in /etc/default/trawld to keep it out of the file
   database_url = "postgres://fleet:…@db.internal:5432/fleet"
   # FRESH transitional store — never the legacy auth.db
   db_path = "/var/lib/trawl/store.db"
   ```

   ```bash
   systemctl restart trawld
   ```

   trawld fails fast with a distinct "auth backend unreachable" error if the
   database is down or unmigrated. If it refuses to start complaining about
   `auth.db`, your `db_path` still points at the legacy file — repoint it,
   do not delete `auth.db`.

5. **Roll out the vector tokens** (restart/reload vector). Buffered events
   drain once the new token authenticates.

6. **Recreate schedules.** Saved queries and schedules lived in the legacy
   `auth.db` and are not migrated. Recreate them under the new keys via the
   API or web UI (`PUT /api/v1/saved/{id}/schedule`).

### Kubernetes (helm)

The chart wires this flow for you: create a Secret holding the DSN and set
`auth.database.existingSecret`. The `init-auth` container runs
`fleet-admin migrate` on every pod start; run the `fleet-admin keys create`
commands from step 2 via `kubectl exec` into the trawld container (the
image ships `fleet-admin`), or from any host with database access.

```bash
kubectl create secret generic trawl-fleet-db \
  --from-literal=DATABASE_URL='postgres://fleet:…@cnpg-rw:5432/fleet'
helm upgrade trawl chart/trawl --set auth.database.existingSecret=trawl-fleet-db
```

## What changed in enforcement

- Revocation now takes effect **immediately** (per-request liveness in
  postgres); the old in-memory token cache and its
  `auth_cache_ttl_secs` revocation window are gone.
- Keys from other fleet apps (e.g. a coastwatch-only key) get an opaque
  `403` on every authenticated trawl route, including `/whoami` and
  `/ingest`.
- The scheduler skips schedules whose owning key is revoked, expired, or
  stripped of its trawl grant.
- SSE `/stream` authenticates at the handshake; an already-established
  stream survives revocation until it disconnects (accepted policy for this
  slice).
