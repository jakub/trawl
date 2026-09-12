---
title: Deploy Trawl
description: Install trawld and trawl-web from the Debian package, the Helm chart, or a release tarball, then verify the installation.
---

A deployment has one `trawld` daemon, a `trawl` PostgreSQL database for app
state, a `fleet` PostgreSQL database for keys and roles, and a data directory of
Parquet files. The browser UI adds the `trawl-web` proxy. Run exactly one daemon
per app-state database and data directory, and plan
[backups](/operate/backup-restore/) before you connect a sender.

## Provision the databases

Every install path needs both databases. They can share one PostgreSQL server,
but they are separate databases with separate owners.

1. As a PostgreSQL superuser, create the owners and databases in `psql`:

   ```sql
   CREATE ROLE fleet LOGIN;
   CREATE ROLE trawl LOGIN;
   CREATE DATABASE fleet OWNER fleet;
   CREATE DATABASE trawl OWNER trawl;
   ```

2. Set a password for each owner with `\password fleet` and `\password trawl`.
   If another Fleet application already has a `fleet` database, reuse it and
   skip its two statements.

3. Apply the Fleet schema. `fleet-admin` reads the Fleet DSN from
   `DATABASE_URL`, and `read` keeps the password out of your shell history:

   ```bash
   read -r -s -p 'Fleet DSN: ' DATABASE_URL && export DATABASE_URL
   fleet-admin migrate
   unset DATABASE_URL
   ```

   Expect `fleet-admin: migrations applied`. Running it again is harmless.

`trawld` migrates the `trawl` database itself at every start and holds a session
advisory lock on it, so a second daemon against the same database fails to start.

## Install the Debian package

You need a Debian or Ubuntu host with systemd, the two databases above, and the
APT repository from [Installation](/getting-started/).

1. Install the packages:

   ```bash
   sudo apt install trawl-server trawl-cli
   ```

   The package creates the users, directories, and units in the table below,
   then enables and starts both services. They fail and retry every 5 seconds
   until the configuration names real databases.

2. Put the two DSNs in `/etc/default/trawld`. They take precedence over
   `[auth] database_url` and `[storage] database_url` in the TOML file, and
   only root and the `trawl` group can read the file:

   ```bash
   FLEET_DATABASE_URL=postgres://fleet:PASSWORD@db.example.com:5432/fleet
   TRAWL_DATABASE_URL=postgres://trawl:PASSWORD@db.example.com:5432/trawl
   ```

3. Edit `/etc/trawl/trawld.toml`. For a first server, set the listener, the
   data path, and the browser origin:

   ```toml
   [server]
   http_addr = "0.0.0.0:5514"

   [data]
   path = "/var/lib/trawl/data"

   [web]
   public_origins = ["https://trawl.example.com"]
   cookie_secret_path = "/var/lib/trawl/web.cookie"
   ```

   `public_origins` lists the origin a browser shows, scheme and port
   included. The packaged default allows `http://127.0.0.1:8090` and
   `http://localhost:8090` for a browser on the same host. See
   [Set the browser origin](/operate/access/#set-the-browser-origin).

   Mount storage at `/var/lib/trawl`, not at `/var/lib/trawl/data`. A repin
   writes a sibling directory next to `data/` on the same filesystem.

4. Decide on TLS. Without `tls_cert_path` and `tls_key_path`, trawld
   generates a self-signed certificate for `localhost` under
   `/var/lib/trawl/tls/`. Remote clients need a certificate they trust. See
   [Configure TLS](/operate/access/#configure-tls).

5. Restart both services and check their state:

   ```bash
   sudo systemctl restart trawld trawl-web
   sudo systemctl status trawld trawl-web
   ```

   Both show `active (running)`. Then [verify the installation](#verify-the-installation).

The package creates these files and directories:

| Path | Owner and mode | Purpose |
| --- | --- | --- |
| `/usr/bin/trawld`, `/usr/bin/trawl-web`, `/usr/bin/fleet-admin`, `/usr/bin/trawl-admin` | root:root 0755 | Executables. `trawl-cli` adds `/usr/bin/trawl`. |
| `/usr/lib/systemd/system/trawld.service`, `trawl-web.service` | root:root 0644 | Run `trawld --config /etc/trawl/trawld.toml --no-monitor` as `trawl:trawl`, and `trawl-web --config /etc/trawl/trawld.toml` as `trawl-web:trawl` after it. |
| `/etc/trawl/trawld.toml` | root:trawl 0640 | Configuration for both daemons. |
| `/etc/default/trawld` | root:trawl 0640 | Environment for `trawld.service`: `FLEET_DATABASE_URL`, `TRAWL_DATABASE_URL`, `RUST_LOG`. |
| `/etc/default/trawl-web` | root:root 0644 | Environment for `trawl-web.service`: `RUST_LOG`, `TRAWL_WEB_INSECURE_UPSTREAM`. World-readable, so no secrets. |
| `/var/lib/trawl` | trawl:trawl 0750 | State directory. `trawld` writes only here and to `/var/log/trawl`. |
| `/var/lib/trawl/data` | trawl:trawl | Parquet files, `wal/`, `scheduled/`, and the `EPOCH` and `CATALOG` markers. Created on first start. |
| `/var/lib/trawl/tls` | trawl:trawl | `cert.pem` and `key.pem`, generated when `[server]` names no certificate. |
| `/var/lib/trawl/web.cookie` | trawl:trawl 0640 | 32-byte session cookie key, generated once on first install. The `trawl` group lets `trawl-web` read it. |
| `/var/lib/trawl/cores` | trawl:trawl 0700 | Crash dumps. Empty unless the [crash-dump drop-in](/reference/crash-dumps/) is enabled. |
| `/var/log/trawl` | trawl:trawl | Log directory for `[server] log_file`. |
| `/usr/lib/sysusers.d/trawl.conf`, `/usr/lib/tmpfiles.d/trawl.conf` | root:root 0644 | Create user `trawl` and user `trawl-web`, a member of group `trawl`, and reapply the modes of `cores` and `web.cookie` at every boot. |
| `/usr/share/doc/trawl-server/examples/crashdump.conf` | root:root 0644 | Optional systemd drop-in that enables crash dumps. |

## Install with Helm

You need Kubernetes 1.26 or later, Helm 3, a StorageClass that provides
`ReadWriteOnce` volumes, and the two databases above, reachable from the
cluster.

1. Create the namespace and one Secret per DSN. The key names are the chart
   defaults for `auth.database.existingSecretKey` and
   `storage.database.existingSecretKey`:

   ```bash
   kubectl create namespace trawl
   kubectl -n trawl create secret generic fleet-db \
     --from-literal=DATABASE_URL='postgres://fleet:PASSWORD@db.example.com:5432/fleet'
   kubectl -n trawl create secret generic trawl-db \
     --from-literal=TRAWL_DATABASE_URL='postgres://trawl:PASSWORD@db.example.com:5432/trawl'
   ```

2. Write `trawl-values.yaml`. `web.publicOrigins` is required while
   `web.enabled` is true, and the chart never derives it from the ingress host:

   ```yaml
   auth:
     database:
       existingSecret: fleet-db
   storage:
     database:
       existingSecret: trawl-db
   web:
     enabled: true
     publicOrigins:
       - https://trawl.example.com
   ingress:
     enabled: true
     className: nginx
     hosts:
       - host: trawl.example.com
         paths:
           - path: /
             pathType: Prefix
     tls:
       - secretName: trawl-tls
         hosts:
           - trawl.example.com
   persistence:
     size: 50Gi
   ```

   The ingress targets the `trawl-web` sidecar on port 8090. Bearer-token
   clients and Vector need trawld's port 5514 instead: in-cluster at
   `https://trawl.trawl.svc.cluster.local:5514`, or outside through a second
   ingress with `ingress.backend: trawld`. The [chart README](https://github.com/jakub/trawl/blob/main/chart/trawl/README.md)
   lists every value.

3. Install:

   ```bash
   helm upgrade --install trawl oci://ghcr.io/jakub/charts/trawl \
     --namespace trawl -f trawl-values.yaml
   ```

   The `init-auth` init container runs `fleet-admin migrate` on every pod
   start. `trawld` migrates the app-state database when it starts.

4. Wait for the pod:

   ```bash
   kubectl -n trawl rollout status statefulset/trawl
   kubectl -n trawl get pod trawl-0
   ```

   The pod reports `2/2` containers ready: `trawld` and `trawl-web`. Then
   [verify the installation](#verify-the-installation), through
   `kubectl -n trawl port-forward svc/trawl 5514:5514` if port 5514 is not
   published.

## Install from a tarball

The [release tarball](/getting-started/) holds the five executables and
nothing else. You supply what the package supplies:

- A user `trawl` and a user `trawl-web`, with `trawl-web` in group `trawl`.
- `/var/lib/trawl`, owned by `trawl:trawl`, mode 0750, with the data
  directory inside it.
- `/etc/trawl/trawld.toml`, owned by `root:trawl`, mode 0640, with the
  `[auth]` and `[storage]` DSNs or the two environment variables set for the
  daemon.
- A 32-byte cookie key: `head -c 32 /dev/urandom > /var/lib/trawl/web.cookie`,
  owned by `trawl:trawl`, mode 0640, named in `[web] cookie_secret_path`.
- A supervisor that runs `trawld --config /etc/trawl/trawld.toml --no-monitor`
  as `trawl` and `trawl-web --config /etc/trawl/trawld.toml` as `trawl-web`.
  Start from the packaged units in
  [`crates/trawl-server/debian/`](https://github.com/jakub/trawl/tree/main/crates/trawl-server/debian).

## Verify the installation

1. Check health. The route needs no token:

   ```bash
   curl --fail-with-body https://trawl.example.com:5514/api/v1/health
   ```

   Expect `"status":"ok"` and `"ok"` for each of `duckdb`, `auth_db`,
   `storage_db`, and `data_path`. A `degraded` status also returns HTTP 200,
   so read every check. For the generated certificate, run the check on the
   host itself against `https://localhost:5514` with
   `--cacert /var/lib/trawl/tls/cert.pem`.

2. Create a human key with [Create roles and keys](/operate/access/#create-roles-and-keys)
   and check its identity:

   ```bash
   curl --fail-with-body -H "Authorization: Bearer $(cat alice.token)" \
     https://trawl.example.com:5514/api/v1/whoami
   ```

   Expect the key's `name`, `kind`, `roles`, and resolved `permissions`.

3. Save the server in a [CLI profile](/start/connect/) and run one bounded
   query:

   ```bash
   trawl -p prod query 'last=15m | head 10'
   ```

   Expect rows with `service` = `trawld`. Internal telemetry is on by default,
   so the daemon's own events appear before any sender connects.

4. If the browser UI is enabled, open the origin and log in with the human
   key. The search page loads.

If a step fails, continue with [Check health and stalled work](/operate/health/).
