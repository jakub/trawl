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

   The package creates the users, directories, and units in the table below.
   It does not enable or start either service, because the packaged
   configuration names no real databases yet. An upgrade keeps the units you
   enabled and restarts the ones that are running.

   The package pins trawld's generated certificate with
   `[web] upstream_ca_path` in `trawld.toml`. An upgrade from an earlier
   package can leave `trawl-web` without a working upstream.
   `/etc/trawl/trawld.toml` is a conffile. When you edited it, dpkg asks
   whether to keep your copy. Keeping it is the default answer, and
   `--force-confold` keeps it without asking. A kept `trawld.toml` does not
   get the new setting. Without it, `trawl-web` checks trawld's self-signed
   certificate against the system roots, the check fails, and every sign-in
   returns 502. After the upgrade:

   1. If the `[web]` section of `/etc/trawl/trawld.toml` has no
      `upstream_ca_path` line, add this line under `[web]`:

      ```toml
      upstream_ca_path = "/var/lib/trawl/tls/cert.pem"
      ```

      trawld writes that certificate when `[server] tls_cert_path` is not
      set. If you moved `[data] path`, use `tls/cert.pem` in the parent
      directory of that path, where trawld writes the certificate instead.
      If you set your own `tls_cert_path`, use the file of the CA that
      issued it.
   2. Run `sudo systemctl restart trawl-web`. Check that
      `systemctl status trawl-web` shows `active (running)`, then sign in.

   trawld now keeps its generated private key in `/var/lib/trawl/tls-key/`,
   not beside the certificate. On its first start after the upgrade, trawld
   deletes the old `tls/key.pem` and generates a new certificate and key.
   `trawl-web` loads the new certificate within 30 seconds. A client that
   pinned the old `cert.pem`, such as a CLI profile's `ca_cert` or a Vector
   CA file, needs a copy of the new one.

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
   `/var/lib/trawl/tls/`. The packaged `[web] upstream_ca_path` pins that
   `cert.pem`, so `trawl-web` verifies trawld with no further setup. Remote
   clients need a certificate they trust. If you set `tls_cert_path`, also
   point `upstream_ca_path` at the CA that issued your certificate. See
   [Configure TLS](/operate/access/#configure-tls).

5. Enable and start both services, then check their state:

   ```bash
   sudo systemctl enable --now trawld trawl-web
   sudo systemctl status trawld trawl-web
   ```

   Both show `active (running)`. On the first start, trawld writes its
   certificate only after it connects to both databases. Until then,
   `trawl-web` runs and answers sign-in with 503
   `upstream certificate not available`. It loads the certificate when
   trawld writes it, with no restart. Then
   [verify the installation](#verify-the-installation).

The package creates these files and directories:

| Path | Owner and mode | Purpose |
| --- | --- | --- |
| `/usr/bin/trawld`, `/usr/bin/trawl-web`, `/usr/bin/fleet-admin`, `/usr/bin/trawl-admin` | root:root 0755 | Executables. `trawl-cli` adds `/usr/bin/trawl`. |
| `/usr/lib/systemd/system/trawld.service`, `trawl-web.service` | root:root 0644 | Run `trawld --config /etc/trawl/trawld.toml --no-monitor` as `trawl:trawl`, and `trawl-web --config /etc/trawl/trawld.toml` as `trawl-web:trawl` after it. |
| `/etc/trawl/trawld.toml` | root:trawl 0640 | Configuration for both daemons. |
| `/etc/default/trawld` | root:trawl 0640 | Environment for `trawld.service`: `FLEET_DATABASE_URL`, `TRAWL_DATABASE_URL`, `RUST_LOG`. |
| `/etc/default/trawl-web` | root:root 0644 | Environment for `trawl-web.service`: `RUST_LOG`. World-readable, so no secrets. |
| `/var/lib/trawl` | trawl:trawl 0750 | State directory. `trawld` writes only here and to `/var/log/trawl`. |
| `/var/lib/trawl/data` | trawl:trawl | Parquet files, `wal/`, `scheduled/`, and the `EPOCH` and `CATALOG` markers. Created on first start. |
| `/var/lib/trawl/tls` | trawl:trawl | `cert.pem`, mode 0644, generated when `[server]` names no certificate. The packaged `[web] upstream_ca_path` pins it. |
| `/var/lib/trawl/tls-key` | trawl:trawl 0700 | `key.pem`, mode 0600, the private key of the generated certificate. |
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
       - secretName: trawl-browser-tls
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

   Before installation, supply the browser ingress Secret named
   `trawl-browser-tls` in namespace `trawl`, with a certificate for
   `trawl.example.com`. Your ingress controller can manage it, or create it
   from your certificate files:

   ```bash
   kubectl -n trawl create secret tls trawl-browser-tls \
     --cert=browser.crt --key=browser.key
   ```

   Choose the daemon API certificate separately using the section below.
   The default API certificate is self-signed.

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

### Configure the daemon API certificate

The chart defaults to `tls.mode: auto`, which lets trawld generate a self-signed
certificate. To use a certificate trusted by API clients and collectors, add
one of the following `tls` blocks to `trawl-values.yaml` before installation.
These settings require structured chart config values. They cannot be combined
with `config.raw`, because Helm cannot verify arbitrary TOML TLS paths against
its Secret mounts.

The `trawl-web` sidecar connects to trawld over the pod's loopback and always
verifies trawld's certificate. Each mode below also sets what the sidecar
trusts. The sidecar runs as `web.runAsUser`, 1001 by default, which must
differ from trawld's uid so that the sidecar cannot read trawld's private key.

#### Keep the generated certificate

`tls.mode: auto` needs no other `tls` value:

```yaml
tls:
  mode: auto
```

trawld writes `cert.pem` to the `tls/` directory beside its data path on the
data volume, and its key to `tls-key/`. The chart mounts only the `tls/`
directory into the sidecar, read-only, and sets `[web] upstream_ca_path` to
its `cert.pem`. On a first install, the sidecar answers 503 until trawld has
written the certificate. It then loads the file with no restart.

With `web.enabled`, a `config.raw` must set `[data] path`, because the chart
mounts the `tls/` directory beside it. It must also set
`upstream_ca_path` under `[web]` to `tls/cert.pem` in the parent directory of
that path, or the chart refuses to render. It must not set `tls_cert_path` or
`tls_key_path` under `[server]`: either one stops trawld generating the
certificate the sidecar pins, so the chart refuses to render. To serve your
own certificate, use `tls.mode=secret`.

#### Mount an existing TLS Secret

Obtain a certificate and matching private key for the DNS names or IP addresses
clients use. With `web.enabled`, the certificate must also name one DNS name
for the sidecar to verify. Create a TLS Secret in the release namespace, and
include the CA that issued the certificate as `ca.crt`:

```bash
kubectl -n trawl create secret generic trawl-api-tls --type=kubernetes.io/tls \
  --from-file=tls.crt=api.crt --from-file=tls.key=api.key --from-file=ca.crt=ca.crt
```

```yaml
tls:
  mode: secret
  secretName: trawl-api-tls
  upstreamServerName: api.example.com
  upstreamCa: secret
```

The chart mounts `tls.crt` and `tls.key` at `/etc/trawl/tls/` in the trawld
container and sets the daemon's certificate paths to those files. The sidecar
gets only `ca.crt`, at `/etc/trawl/upstream-ca/`. It requests
`https://api.example.com:5514` and connects to `127.0.0.1:5514`, so the name
does not need to resolve inside the pod. `tls.upstreamServerName` is required
in this mode. It must be one DNS name in the certificate, not a wildcard or an
IP address. Until the Secret holds `ca.crt`, the sidecar answers 503. The chart
does not replace or populate this Secret.

#### Request a certificate through cert-manager

Your cluster must already have cert-manager and an issuer that can issue the
requested names. This chart creates neither. The example below uses an
existing private `ClusterIssuer` named `homelab-ca`. Its policy must permit the
external API name and the internal Service name. Public ACME issuers generally
cannot issue certificates for internal cluster DNS names.

```yaml
tls:
  mode: certManager
  certManager:
    issuerRef:
      name: homelab-ca
      kind: ClusterIssuer
      group: cert-manager.io
    dnsNames:
      - api.example.com
      - trawl.trawl.svc.cluster.local
  upstreamCa: secret
```

Replace the issuer and DNS names with yours. An `Issuer` must exist in the
release namespace; a `ClusterIssuer` is cluster-scoped. There is no
`issuerRef.namespace` setting. Use `tls.mode: secret` for certificates with IP
address SANs. DNS wildcards cover one label: `*.example.com` covers
`api.example.com`, but not `example.com` or `deep.api.example.com`.

A CA or self-signed issuer writes `ca.crt` into the Secret, and
`upstreamCa: secret` pins it. The sidecar verifies the first `dnsNames` entry
that is not a wildcard, `api.example.com` here. To verify another listed name,
set `tls.upstreamServerName`.

For release `trawl`, the chart creates Certificate `trawl-tls` in namespace
`trawl`. cert-manager creates and renews Secret `trawl-tls`; the daemon mounts
its `tls.crt` and `tls.key`. Do not use that Secret name for database credentials
or browser cookies. After the Helm install, wait for issuance:

```bash
kubectl -n trawl wait certificate/trawl-tls --for=condition=Ready --timeout=120s
kubectl -n trawl describe certificate trawl-tls
kubectl -n trawl rollout status statefulset/trawl
```

A pending Certificate requires investigation of the issuer or its issuance
policy. The pod cannot start until its TLS Secret exists. The chart checks that
requested DNS names cover configured API ingress and HTTPRoute hosts. It does
not publish those names, configure DNS, or install the issuer's CA on clients.
The [cert-manager Certificate documentation](https://cert-manager.io/docs/usage/certificate/)
explains issuance and renewal.

#### Choose the CA the sidecar trusts

In `secret` and `certManager` modes, `tls.upstreamCa` is required and has no
default, because the chart cannot see inside the Secret. It takes one of three
values:

| Value | The sidecar trusts | Use it when |
| --- | --- | --- |
| `secret` | `ca.crt` from the TLS Secret | A private CA or a self-signed issuer signed the certificate |
| `system` | The platform roots in the image | A publicly trusted CA issued the certificate |
| An absolute path | The CA file at that path | The Secret holds no `ca.crt`. Mount the CA with `web.extraVolumes` and `web.extraVolumeMounts` |

For a CA in a ConfigMap named `trawl-api-ca`, under the key `ca.crt`, merge
these values into `trawl-values.yaml`:

```yaml
tls:
  mode: secret
  secretName: trawl-api-tls
  upstreamServerName: api.example.com
  upstreamCa: /etc/trawl/api-ca/ca.crt
web:
  extraVolumes:
    - name: trawl-api-ca
      configMap:
        name: trawl-api-ca
  extraVolumeMounts:
    - name: trawl-api-ca
      mountPath: /etc/trawl/api-ca
      readOnly: true
```

Mount a directory, as above, not a single file through `subPath`. Kubernetes
updates a directory mount when the ConfigMap changes, and the sidecar loads the
new file within 30 seconds after it changes.

#### Upgrade from an earlier chart

An earlier chart let the sidecar skip certificate verification. This chart
always verifies. In `secret` and `certManager` modes with `web.enabled`, set
`tls.upstreamCa` before `helm upgrade`, and set `tls.upstreamServerName` in
`secret` mode. Without them, `helm upgrade` fails, and the error names the
value to set. `auto` mode needs no new value.

In `auto` mode, trawld now keeps its generated key in `tls-key/`, outside the
directory the sidecar mounts. After the upgrade, the `init-tls-dir` container
deletes the old `tls/key.pem` before trawld and the sidecar start, and trawld
then generates a new certificate and key. A client that pinned the old
certificate needs a copy of the new one.

#### Verify the requested name and trust

Obtain the CA certificate from your issuer administrator. With the API
port-forward running, verify the certificate using a requested DNS name:

```bash
kubectl -n trawl port-forward svc/trawl 5514:5514
```

In another terminal:

```bash
curl --fail-with-body --cacert ca.crt \
  --resolve api.example.com:5514:127.0.0.1 \
  https://api.example.com:5514/api/v1/health
```

For a certificate already trusted by the operating system, omit `--cacert`.
The browser ingress certificate remains configured through `ingress.tls`.
The browser sidecar verifies the same certificate over pod loopback, with the
name and CA that `tls.upstreamServerName` and `tls.upstreamCa` set. trawld
reloads a changed mounted certificate every
`config.server.tlsReloadIntervalSecs` seconds, 300 by default. The sidecar
reads its CA file again every 30 seconds.

### Use a local browser

For a local trial through a port-forward, replace the `web` and `ingress`
sections in `trawl-values.yaml` above with these values. Keep the database
Secrets and persistence settings:

```yaml
web:
  enabled: true
  publicOrigins:
    - http://localhost:8090
  allowInsecureCookies: true
ingress:
  enabled: false
```

Apply the values with the `helm upgrade --install` command above and wait for
the StatefulSet rollout. Then keep this command running:

```bash
kubectl port-forward --namespace trawl svc/trawl 5514:5514 8090:8090
```

Open `http://localhost:8090` on the machine running the port-forward and sign
in with a personal API key from [Create roles and keys](/operate/access/#create-roles-and-keys).
Use `localhost` as written: `http://127.0.0.1:8090` is a different browser
origin. Port 5514 serves the HTTPS API; port 8090 serves the browser.

When you switch to an HTTPS ingress, set `web.publicOrigins` to its HTTPS
origin, restore the ingress values, and set `web.allowInsecureCookies: false`.
See [Set the browser origin](/operate/access/#set-the-browser-origin).

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
- `[web] upstream_ca_path = "/var/lib/trawl/tls/cert.pem"` in `trawld.toml`,
  as the package sets it. Without it, `trawl-web` checks trawld's self-signed
  certificate against the system roots and every sign-in fails.
- A supervisor that runs `trawld --config /etc/trawl/trawld.toml --no-monitor`
  as `trawl` and `trawl-web --config /etc/trawl/trawld.toml` as `trawl-web`.
  Start from the packaged units in
  [`crates/trawl-server/debian/`](https://github.com/jakub/trawl/tree/main/crates/trawl-server/debian).

## Use a current storage root

An ingesting daemon initializes a missing or empty data directory with
`EPOCH` set to `3`. A nonempty owned directory must already carry that marker.
If the marker names another format, restore a complete epoch-3 backup or
configure new empty data and WAL directories. Do not change the marker to
relabel existing files. Startup does not convert, rename, or import an older
root or its scheduled report results.

Current WAL batches live under environment directories. A batch directly
under the configured WAL directory causes startup to refuse before storage
recovery. Keep the files intact and select an empty WAL directory or restore
WAL from a complete current backup.

With ingestion disabled, the daemon can also read a generic unversioned
Parquet archive without changing its storage markers. That archive must not
contain Trawl ownership entries such as `wal`, `CATALOG`, `REPIN`, `scheduled`,
or top-level date partitions. An explicit noncurrent `EPOCH` still refuses.
For standalone exports or files from other tools, you can also
[query local Parquet](/start/local-parquet/) without running a daemon.

## Verify the installation

After verification, [load the operational alert pack](/operate/operational-alerts/)
into your existing Prometheus installation. Plain rules include a matching
HTTPS scrape example. Helm rule creation is opt-in and independent of
ServiceMonitor creation; neither option installs a monitoring system.

1. Create a human key with [Create roles and keys](/operate/access/#create-roles-and-keys).

2. Save the server in a [CLI profile](/start/connect/) with that key. When
   the API certificate is from a CA the system trusts:

   ```toml
   [profiles.prod]
   url = "https://trawl.example.com:5514"
   token = "PASTE_THE_HUMAN_KEY_HERE"
   ```

   The generated self-signed certificate names only `localhost`, and
   `trawl doctor --url` trusts only the system roots. On the host itself,
   copy the certificate and pin it with `ca_cert`:

   ```bash
   install -d -m 0700 ~/.config/trawl
   sudo cat /var/lib/trawl/tls/cert.pem > ~/.config/trawl/prod-ca.pem
   ```

   ```toml
   [profiles.prod]
   url = "https://localhost:5514"
   ca_cert = "~/.config/trawl/prod-ca.pem"
   token = "PASTE_THE_HUMAN_KEY_HERE"
   ```

   Then run `chmod 0600 ~/.config/trawl/config.toml`.

3. Run the doctor against the profile. If the browser UI is enabled, add its
   origin with `--web-url`:

   ```bash
   trawl doctor -p prod --web-url https://trawl.example.com
   ```

   On the host with the packaged `public_origins`, use
   `--web-url http://127.0.0.1:8090`. Expect `verdict: pass (exit 0)`.
   `api.health.duckdb`, `api.health.auth_db`, `api.health.storage_db`,
   `api.health.data_path`, and `api.health.ingest_capacity` are `complete`.
   `api.identity` is `complete` and shows the key's name, kind, and
   permissions. A `degraded` server fails the run, because the failing check
   has its own `failed` line. Each failed line names its next action. The
   [CLI reference](/reference/cli/#doctor-mode) lists every check.

   `web.origin` shows that `trawl-web` accepts the origin. It does not show
   that `trawl-web` reaches `trawld`. The sign-in in step 5 does.

4. Run one bounded query:

   ```bash
   trawl -p prod query 'last=15m | head 10'
   ```

   Expect rows with `service` = `trawld`. Internal telemetry is on by default,
   so the daemon's own events appear before any sender connects.

5. If the browser UI is enabled, open the origin and log in with the human
   key. The search page loads.

If a step fails, continue with [Check health and stalled work](/operate/health/).
