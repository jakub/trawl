---
title: Deploy Trawl
description: Prepare both databases, select an artifact, and verify a named installation.
---

A deployment has a daemon, a dedicated app-state database, a shared Fleet
keystore, and a data filesystem. The browser adds `trawl-web` and its embedded
SPA. Keep exactly one daemon writing the app-state database and corpus.

## Select the installation

Record the host or Kubernetes context, namespace, release, artifact version,
and configuration source. A saved CLI profile is a connection, not a statement
that its server is disposable. Follow the existing GitOps source when it owns
the workload. Use [installation](/getting-started/) for obtaining binaries.

Before an upgrade, read [upgrade boundaries](/operate/upgrades/) and take a
[coordinated backup](/operate/backup-restore/). A build request does not require
publishing a release or changing a running server.

## Provision the databases

Provision `fleet` and `trawl` on PostgreSQL. They are separate databases, even
when they share a PostgreSQL instance. On PostgreSQL 15 or later, an administrator
can prepare the simple single-owner arrangement as follows in `psql`:

```sql
CREATE ROLE fleet LOGIN;
CREATE DATABASE fleet OWNER fleet;
CREATE ROLE trawl LOGIN;
CREATE DATABASE trawl OWNER trawl;
```

Set each login's password with `\password fleet` and `\password trawl`, or use
the database administrator's existing authentication policy. Do not put real
passwords in these examples, shell history, or deployment output. The database
owner has the schema creation rights needed for migrations. If Fleet already
exists, reuse its established administrator and privileges instead of recreating it.

Supply the Fleet DSN to `fleet-admin` as `DATABASE_URL` through the protected
credential source, then run:

```bash
fleet-admin migrate
```

`trawld` uses `FLEET_DATABASE_URL` and `TRAWL_DATABASE_URL`, or the corresponding
`[auth] database_url` and `[storage] database_url` settings. It does not read bare
`DATABASE_URL`. It migrates the app-state schema at boot and holds a session
advisory lock that rejects a second daemon. See [configuration](/reference/configuration/#auth).
Create roles and keys through [access administration](/operate/access/).

## Debian package or tarball

Install the selected architecture's complete release artifacts. A release
contains `trawl`, `trawld`, `trawl-admin`, `fleet-admin`, and `trawl-web`.
The server package supplies systemd units; a tarball requires the operator to
provide a supervisor, users, writable directories, and configuration.

For a packaged installation, configure `/etc/trawl/trawld.toml` and the protected
`/etc/default/trawld` environment file before starting the service. The daemon
runs as `trawl`; the proxy runs as `trawl-web` with group `trawl`. Keep the packaged
ownership and mode rules. The package supplies a persistent raw cookie key at
`/var/lib/trawl/web.cookie`, preserved on ordinary upgrades. This is an app-local
key, not automatic shared SSO.

The usual data path is `/var/lib/trawl/data`. Mount storage at `/var/lib/trawl`,
not directly at `data`, so repin can create sibling generations on the same
filesystem. Nested mounts in an environment subtree also prevent repin.

```bash
sudo systemctl start trawld
sudo systemctl status trawld
```

If using the browser, configure `[web] public_origins`, TLS termination, and
persistent session material first, then start `trawl-web`. The origin must match
what the address bar shows. Keep `Secure` cookies for browser HTTPS even when
the reverse proxy forwards HTTP internally. See [TLS and browser access](/operate/access/#tls-and-browser-access).

## Kubernetes and Helm

Choose a context, namespace, and chart version. Create the namespace if needed.
Provision database Secrets there through the cluster's existing secret-management
workflow. By default, the Fleet Secret key is `DATABASE_URL` and the app-state
Secret key is `TRAWL_DATABASE_URL`; these are Secret keys, not equivalent daemon
environment variable names.

Save a complete values file, for example `trawl-values.yaml`:

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
persistence:
  enabled: true
  size: 50Gi
```

The browser origin does not create an ingress or its TLS certificate. Configure
those for your controller, or use an explicitly configured local port-forward.
The raw HTTPS API and browser proxy are different endpoints. Bearer clients and
Vector use trawld; the proxy blocks HTTP ingest.

From a checkout matching the requested chart version, render before installing:

```bash
helm template "$TRAWL_RELEASE" ./chart/trawl \
  --namespace "$TRAWL_NAMESPACE" -f trawl-values.yaml > trawl-rendered.yaml
helm upgrade --install "$TRAWL_RELEASE" ./chart/trawl \
  --kube-context "$TRAWL_CONTEXT" --namespace "$TRAWL_NAMESPACE" \
  -f trawl-values.yaml
```

Both Secret values remain required with `web.enabled=false`. Enabling the
browser requires `web.publicOrigins`, including when `config.raw` supplies TOML.
The chart's init container runs Fleet migrations; the daemon migrates app state.
Inspect the [chart README](https://github.com/jakub/trawl/blob/main/chart/trawl/README.md)
and values at the selected tag for ingress, syslog Service ports, TLS, and storage options.

## Build from source

Use the checked-out release workflow for the full artifact set and target
architecture. The browser build order is SPA, precompression, then proxy:

```bash
env -u NO_COLOR cargo xtask build-web --release
```

Plain `cargo build --release` does not build all distribution binaries or this
asset sequence. The Dockerfile copies prebuilt `docker-ctx/${TARGETARCH}` binaries;
it is not a source-build recipe. Cross builds and Debian packages also need the
workflow's symbol and stripping steps. Do not improvise a second release pipeline.

## Verify the deployment

Read service or rollout status, startup logs, and `/api/v1/health` checks.
Then verify authenticated `/api/v1/whoami` and a bounded query using the intended
client profile. HTTP 200 alone does not prove healthy dependencies. If the browser
changed, verify login and asset delivery. If ingestion changed, send a labeled
sample with an ingest-capable key and check accepted counts and the resulting event.
Use [health diagnosis](/operate/health/) if any check fails.
