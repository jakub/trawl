# trawl Helm Chart

Self-hosted log collection, storage, and search for homelabs and small-to-medium infra.

## Quick Start

```bash
# Two Secrets holding the postgres DSNs (fleet keystore + trawl app state)
kubectl create secret generic fleet-db \
  --from-literal=DATABASE_URL='postgres://fleet:...@pg:5432/fleet'
kubectl create secret generic trawl-db \
  --from-literal=TRAWL_DATABASE_URL='postgres://trawl:...@pg:5432/trawl'

# Install from GHCR OCI registry
helm install trawl oci://ghcr.io/jakub/charts/trawl \
  --set auth.database.existingSecret=fleet-db \
  --set storage.database.existingSecret=trawl-db

# Port-forward for local access
kubectl port-forward svc/trawl 5514:5514

# Query via CLI (self-signed cert; mint tokens with fleet-admin)
trawl query --url https://localhost:5514 --insecure --token <TOKEN> "* | head 5"
```

## Architecture

trawl is **single-node only** — the chart deploys a StatefulSet with exactly 1 replica. trawld owns its parquet data directory and in-memory hot buffer outright, and it holds a session advisory lock on the trawl app-state database, so a second replica fails startup rather than split-braining. This is by design: trawl targets homelabs and small infra, not multi-tenant clusters.

The single pod runs `trawld` with:
- Embedded DuckDB for query execution
- Parquet files for log storage
- Optional syslog listener (UDP/TCP)
- The `trawl-web` session proxy sidecar (browser UI)

Two **external postgres databases** back it (CNPG or any reachable postgres):

| Database | DSN source | Owner | Contents |
|----------|-----------|-------|----------|
| `fleet` (shared across fleet apps) | Secret → `FLEET_DATABASE_URL` env (fleet-admin init container reads it as `DATABASE_URL`) | migrated by `fleet-admin migrate` (init container) | API keys, roles, grants |
| `trawl` (dedicated) | Secret → `TRAWL_DATABASE_URL` env | migrated by trawld at boot (sole writer, advisory-locked) | query history, saved queries, schedules, report runs |

There is no fallback between the two URLs — provision both databases. See the fleet-auth cutover runbook in the docs for the exact roles/grants (boot-time migration means the trawld role owns the `trawl` schema).

## Prerequisites

- Kubernetes 1.26+ (for HTTPS health probes)
- Helm 3.x
- A StorageClass that supports `ReadWriteOnce` PVCs
- A reachable postgres with the `fleet` and `trawl` databases provisioned

## Installing

```bash
# Default install (self-signed TLS, 50Gi storage)
helm install trawl oci://ghcr.io/jakub/charts/trawl \
  --set auth.database.existingSecret=fleet-db \
  --set storage.database.existingSecret=trawl-db

# Custom values
helm install trawl oci://ghcr.io/jakub/charts/trawl \
  --set auth.database.existingSecret=fleet-db \
  --set storage.database.existingSecret=trawl-db \
  --set persistence.size=100Gi \
  --set config.retention.maxAgeDays=180

# From source
helm install trawl ./chart/trawl
```

## Auth

API keys live in the shared fleet keystore. An init container runs `fleet-admin migrate` on every pod start (idempotent — sqlx tracks applied migrations). Mint keys with `fleet-admin` against the keystore database:

```bash
fleet-admin keys create --name "my-analyst-key" --kind human --role trawl-analyst
```

Roles are data-defined permission bundles (ADR-0006), managed with `fleet-admin roles`; the converted tiers are `trawl-admin`, `trawl-analyst`, `trawl-reader`, and `trawl-ingest`. One key can hold several roles spanning several fleet apps; only the resolved `trawl` permissions matter to trawld.

## TLS

trawld always speaks HTTPS. Three modes are available:

| Mode | Description |
|------|-------------|
| `auto` (default) | trawld generates a self-signed ECDSA P-256 cert at startup. Clients need `--insecure`. |
| `secret` | Mount an existing Kubernetes TLS Secret. Set `tls.secretName`. |
| `certManager` | Create a cert-manager Certificate. Set `tls.certManager.issuerRef`. |

```yaml
# Example: existing TLS secret
tls:
  mode: secret
  secretName: trawl-tls-cert

# Example: cert-manager
tls:
  mode: certManager
  certManager:
    issuerRef:
      name: letsencrypt-prod
      kind: ClusterIssuer
```

## Web UI and Ingress

The chart runs a **trawl-web sidecar** in the same pod as trawld by default. It serves the SPA and translates browser cookie sessions into bearer tokens against trawld on loopback.

Ingress targets the sidecar by default (plain HTTP port 8090):

```yaml
ingress:
  enabled: true
  className: nginx
  hosts:
    - host: trawl.example.com
      paths:
        - path: /
          pathType: Prefix
  tls:
    - secretName: trawl-ingress-tls
      hosts:
        - trawl.example.com
```

Two things to keep in mind:

1. **API clients keep talking to trawld directly.** `trawl query`, the `trawl-client` library, and vector all use bearer tokens against trawld's HTTPS port (5514). The web-UI ingress rejects non-cookie auth and blocks `/api/v1/ingest` outright. In-cluster clients hit the Service on 5514; external clients need a LoadBalancer or a second ingress with `ingress.backend: trawld`.
2. **Cookie flags assume end-to-end TLS.** The proxy sets `Secure` on session cookies. If your ingress TLS-terminates AND forwards plain HTTP to the Service, browsers will discard the cookie. Flip `web.allowInsecureCookies: true` only in that topology — never over the open internet.

Switch the ingress backend to the raw HTTPS API instead:

```yaml
ingress:
  enabled: true
  backend: trawld
  annotations:
    nginx.ingress.kubernetes.io/backend-protocol: "HTTPS"
```

Disable the web UI entirely (trawld-only deployment):

```yaml
web:
  enabled: false
```

## Gateway API (HTTPRoute)

For clusters using Gateway API instead of Ingress (e.g. Traefik, Envoy Gateway, Istio):

```yaml
httpRoute:
  enabled: true
  parentRef:
    name: my-gateway
    namespace: network
    sectionName: websecure
  hostnames:
    - trawl.example.com
```

Since trawld speaks HTTPS natively, your gateway controller needs to trust the backend certificate. Create a `BackendTLSPolicy` targeting the trawl Service — for example, with a cert-manager-issued Let's Encrypt cert:

```yaml
apiVersion: gateway.networking.k8s.io/v1alpha3
kind: BackendTLSPolicy
metadata:
  name: trawl
spec:
  targetRefs:
    - group: ""
      kind: Service
      name: trawl
  validation:
    wellKnownCACertificates: System
    hostname: trawl.example.com
```

## Syslog Listener

trawl has a native syslog listener for network appliances (firewalls, switches, APs) that don't support HTTP log shipping. Enable it and map source IPs to service names:

```yaml
service:
  syslog:
    enabled: true

config:
  syslog:
    enabled: true
    defaultService: "syslog"
    sourceServiceMap:
      "10.0.0.1": "unifi-gateway"
      "10.0.0.2": "unifi-ap"
      "10.0.0.3": "mikrotik-core"
    allowCidrs:
      - "10.0.0.0/8"
```

Source IP mappings take priority over the APP-NAME/tag from syslog messages. The `defaultService` is used when neither matches. All syslog tuning parameters (batch size, timeouts, TCP limits) are exposed — see the values reference or `values.yaml` for the full list.

## TLS Certificate Rotation

When using `tls.mode: secret` or `tls.mode: certManager`, trawld periodically checks for rotated cert files. The default check interval is 300 seconds. For cert-manager deployments with frequent rotation, lower this:

```yaml
config:
  server:
    tlsReloadIntervalSecs: 60
```

## Sending Logs with Vector

Example [Vector](https://vector.dev) sink configuration:

```toml
[sinks.trawl]
type = "http"
inputs = ["your_source"]
uri = "https://trawl.default.svc.cluster.local:5514/api/v1/ingest"
encoding.codec = "json"
compression = "gzip"
batch.max_bytes = 1048576
batch.timeout_secs = 5

[sinks.trawl.request]
headers.authorization = "Bearer <INGEST_TOKEN>"

[sinks.trawl.tls]
verify_certificate = false  # if using self-signed cert
```

Create a dedicated ingest token:

```bash
fleet-admin keys create --name "vector" --kind service --role trawl-ingest
```

## Prometheus Metrics

trawld exposes Prometheus metrics at `/metrics` (unauthenticated). Enable the ServiceMonitor for prometheus-operator:

```yaml
serviceMonitor:
  enabled: true
  interval: 30s
```

## Upgrading

Config changes trigger a rolling restart via a config checksum annotation. Since replicas = 1, this means brief downtime (old pod terminates, new pod starts).

```bash
helm upgrade trawl oci://ghcr.io/jakub/charts/trawl --reuse-values \
  --set config.retention.maxAgeDays=365
```

Data is preserved across upgrades — the PVC persists independently of the pod.

## Raw Config Override

For full control over `trawld.toml`, bypass the structured values and provide raw TOML:

```yaml
config:
  raw: |
    [server]
    http_addr = "0.0.0.0:5514"
    timeout_secs = 60

    [data]
    path = "/var/lib/trawl/data"

    [ingest]
    enabled = true
```

The postgres DSNs still arrive via the `FLEET_DATABASE_URL` / `TRAWL_DATABASE_URL` env vars (from the Secrets), so `[auth]`/`[storage]` `database_url` lines are unnecessary in raw config too.

## Values Reference

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `image.repository` | string | `ghcr.io/jakub/trawl` | Container image repository |
| `image.tag` | string | `""` (appVersion) | Image tag override |
| `image.pullPolicy` | string | `IfNotPresent` | Image pull policy |
| `service.type` | string | `ClusterIP` | Service type |
| `service.port` | int | `5514` | trawld HTTPS service port |
| `service.webPort` | int | `8090` | trawl-web session proxy port (only when `web.enabled`) |
| `service.syslog.enabled` | bool | `false` | Expose syslog ports |
| `service.syslog.udpPort` | int | `1514` | Syslog UDP port |
| `service.syslog.tcpPort` | int | `1514` | Syslog TCP port |
| `ingress.enabled` | bool | `false` | Create an Ingress resource |
| `ingress.backend` | string | `web` | Target service port: `web` (trawl-web, default) or `trawld` (raw HTTPS API) |
| `web.enabled` | bool | `true` | Run the trawl-web session proxy sidecar |
| `web.bindAddr` | string | `0.0.0.0:8090` | Bind address for trawl-web (pod-IP reachable) |
| `web.sessionTtlSecs` | int | `86400` | Browser session lifetime (seconds) |
| `web.allowInsecureCookies` | bool | `false` | Drop `Secure` flag on session cookies (behind TLS-terminating ingress only) |
| `web.resources` | object | cpu 50m / mem 64Mi–256Mi | Resource requests/limits for the sidecar |
| `web.cookieSecret.existingSecret` | string | `""` | Name of a pre-existing Secret holding the cookie key (chart generates one when empty) |
| `web.cookieSecret.existingSecretKey` | string | `cookie.key` | Key within the Secret that holds the 32-byte AEAD key |
| `httpRoute.enabled` | bool | `false` | Create a Gateway API HTTPRoute |
| `httpRoute.parentRef.name` | string | `""` | Gateway name |
| `httpRoute.parentRef.namespace` | string | `""` | Gateway namespace |
| `httpRoute.hostnames` | list | `[]` | Hostnames for the HTTPRoute |
| `persistence.enabled` | bool | `true` | Enable persistent storage |
| `persistence.size` | string | `50Gi` | PVC size |
| `persistence.storageClass` | string | `""` | StorageClass (empty = default) |
| `config.raw` | string | `""` | Raw trawld.toml (bypasses structured values) |
| `config.server.httpAddr` | string | `0.0.0.0:5514` | Bind address |
| `config.server.timeoutSecs` | int | `30` | Query timeout |
| `config.server.maxConcurrentQueries` | string | `""` | DuckDB pool size (empty = CPU count) |
| `config.server.shutdownDrainSecs` | int | `30` | Graceful shutdown timeout |
| `config.data.path` | string | `/var/lib/trawl/data` | Parquet data directory |
| `auth.database.existingSecret` | string | `""` | Secret holding the fleet keystore DSN. REQUIRED |
| `auth.database.existingSecretKey` | string | `DATABASE_URL` | Key within that Secret |
| `storage.database.existingSecret` | string | `""` | Secret holding the trawl app-state DSN. REQUIRED |
| `storage.database.existingSecretKey` | string | `TRAWL_DATABASE_URL` | Key within that Secret |
| `config.ingest.enabled` | bool | `true` | Enable ingest endpoint |
| `config.ingest.hotBufferMaxBytes` | string | `100M` | Hot buffer memory limit |
| `config.retention.maxAgeDays` | int | `90` | Data retention (days, 0 = disabled) |
| `config.retention.minFreeDiskBytes` | string | `1G` | Disk space retention threshold |
| `config.server.tlsReloadIntervalSecs` | int | `300` | Cert rotation check interval (seconds) |
| `config.syslog.enabled` | bool | `false` | Enable native syslog listener |
| `config.syslog.defaultService` | string | `syslog` | Fallback service name for syslog |
| `config.syslog.sourceServiceMap` | object | `{}` | IP → service name mapping |
| `config.syslog.allowCidrs` | list | `[]` | Source IP allowlist (CIDR notation) |
| `config.scheduler.enabled` | bool | `true` | Enable scheduled queries |
| `tls.mode` | string | `auto` | TLS mode: auto, secret, certManager |
| `tls.secretName` | string | `""` | TLS Secret name (mode=secret) |
| `initAuth.enabled` | bool | `true` | Run `fleet-admin migrate` init container on pod start |
| `initAuth.resources.requests.cpu` | string | `50m` | Init container CPU request |
| `initAuth.resources.requests.memory` | string | `64Mi` | Init container memory request |
| `initAuth.resources.limits.memory` | string | `128Mi` | Init container memory limit |
| `resources.requests.cpu` | string | `250m` | CPU request |
| `resources.requests.memory` | string | `512Mi` | Memory request |
| `resources.limits.memory` | string | `2Gi` | Memory limit |
| `logLevel` | string | `trawl_server=info` | RUST_LOG value |
| `serviceMonitor.enabled` | bool | `false` | Create prometheus-operator ServiceMonitor |
