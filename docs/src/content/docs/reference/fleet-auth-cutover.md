---
title: Fleet-Auth Cutover Runbook
description: Migrating a trawld deployment from the sqlite keystore to the fleet-auth postgres keystore (ADR-0004 slice 1), plus fleet SSO key provisioning (slice 2).
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

## Fleet SSO (`fleet_session` cookie)

Since ADR-0004 slice 2 trawl-web issues the fleet-wide `fleet_session`
cookie (`SameSite=Lax`, app-agnostic `{token, name, exp}` payload). SSO —
one login shared with coastwatch-web — is **opt-in** and needs two things
in every participating app: the same `shared_domain` and the same session
key. Standalone installs need none of this; both channels keep
self-generating an app-local key.

### 1. Mint and stash the shared key (once per fleet)

```bash
fleet-admin generate-session-key
# prints a base64url key — never recoverable, store it immediately
op item create --category=password --title='Fleet session key' \
  --vault=Homelab credential='<the printed key>'
```

The canonical reference is `op://Homelab/Fleet session key/credential`.
Per coastwatch ADR-0038 the key stays an `op://` runtime reference — it is
cross-repo shared, so never commit it as ciphertext anywhere.

### 2. DNS / ingress

Fleet apps live under one parent domain; the cookie's `Domain=` attribute
is scoped to it:

- `trawl.fleet.lab.ktle.net` → trawl-web
- `coastwatch.fleet.lab.ktle.net` → coastwatch-web
- `shared_domain = ".fleet.lab.ktle.net"` in **both** apps

`trawl-01.lab.ktle.net` stays the direct trawld API endpoint for CLI and
vector bearer clients — cookie-free, unaffected. If a reverse proxy
terminates TLS in front of trawl-web, it must forward the original `Host`
header: login/logout validate the `Origin` header against `Host` and
`shared_domain`, and a rewritten `Host` makes legitimate logins 403.

### 3. Debian channel

The postinst-generated key at `/var/lib/trawl/web.cookie` must be replaced
with the shared one. **`cookie_secret_path` expects RAW 32 bytes, not
base64** — the `op://` item is base64url without padding (43 chars), so
re-pad and decode while writing:

```bash
printf '%s=' "$(op read 'op://Homelab/Fleet session key/credential')" \
  | basenc --base64url -d > /var/lib/trawl/web.cookie   # raw 32 bytes
chmod 600 /var/lib/trawl/web.cookie
```

Then in `/etc/trawl/trawld.toml`:

```toml
[web]
shared_domain = ".fleet.lab.ktle.net"
```

```bash
systemctl restart trawl-web
```

If trawl-web fails to start complaining the key file "must contain exactly
32 bytes", you wrote the base64 text instead of decoding it.

### 4. Kubernetes (helm)

The Secret must hold the RAW key bytes under `cookie.key` (kubernetes
`data` values are *standard* base64 — the `op://` item's url-safe unpadded
form does NOT drop in as-is). Decode first and let kubectl do its own
encoding:

```bash
printf '%s=' "$(op read 'op://Homelab/Fleet session key/credential')" \
  | basenc --base64url -d \
  | kubectl create secret generic fleet-session-key \
      --from-file=cookie.key=/dev/stdin

helm upgrade trawl chart/trawl \
  --set web.cookieSecret.existingSecret=fleet-session-key \
  --set web.sharedDomain=.fleet.lab.ktle.net
```

### 5. Coastwatch side

Set the same two values in the coastwatch repo. **Note:** coastwatch's
committed prod config predates the DNS decision and still carries
`session.shared_domain = ".fleet.home.lan"` — it must become
`".fleet.lab.ktle.net"` or SSO silently breaks (a mismatched `Domain=`
means each app sets a cookie the other never sees). Coastwatch also picks
up the default-on login/logout Origin validation on its next rebuild
against fleet-auth.

### Behavior notes

- Sessions minted before the key swap die at the swap (AEAD key change);
  users log in once more.
- Legacy `trawl_session` cookies are ignored and expire at their TTL — no
  cleanup needed.
- Upstream mapping: trawld 401 (key revoked/expired fleet-wide) clears the
  shared cookie everywhere; trawld 403 (valid key, no trawl grant) keeps
  the cookie so the user stays signed in to sibling apps.

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
