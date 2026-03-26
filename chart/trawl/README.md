# trawl Helm Chart

Self-hosted log collection, storage, and search for homelabs and small-to-medium infra.

## Quick Start

```bash
# Install from GHCR OCI registry
helm install trawl oci://ghcr.io/jakub/charts/trawl

# Get the initial admin API token (first install only)
kubectl logs trawl-0 -c init-auth

# Port-forward for local access
kubectl port-forward svc/trawl 5514:5514

# Query via CLI (self-signed cert)
trawl query --url https://localhost:5514 --insecure --token <TOKEN> "* | head 5"
```

## Architecture

trawl is **single-node only** — the chart deploys a StatefulSet with exactly 1 replica. Multiple replicas would corrupt the embedded SQLite auth database. This is by design: trawl targets homelabs and small infra, not multi-tenant clusters.

The single pod runs `trawld` with:
- Embedded DuckDB for query execution
- SQLite for auth (API keys, roles, saved queries)
- Parquet files for log storage
- Optional syslog listener (UDP/TCP)

## Prerequisites

- Kubernetes 1.26+ (for HTTPS health probes)
- Helm 3.x
- A StorageClass that supports `ReadWriteOnce` PVCs

## Installing

```bash
# Default install (self-signed TLS, 50Gi storage)
helm install trawl oci://ghcr.io/jakub/charts/trawl

# Custom values
helm install trawl oci://ghcr.io/jakub/charts/trawl \
  --set persistence.size=100Gi \
  --set config.retention.maxAgeDays=180

# From source
helm install trawl ./chart/trawl
```

## Auth Bootstrap

On first install, an init container creates the auth database and generates two API keys — an **admin** key and an **ingest** key. Both tokens are printed to the init container's logs:

```bash
kubectl logs trawl-0 -c init-auth
```

**Save these tokens** — they won't be shown again. The ingest token can be used immediately with Vector or any HTTP log shipper. To create additional keys:

```bash
kubectl exec trawl-0 -- trawl-admin --db /var/lib/trawl/auth.db keys create \
  --role analyst --name "my-analyst-key"
```

Available roles: `admin`, `analyst`, `reader`, `ingest`.

The init container is idempotent — on upgrades or restarts, it detects the existing `auth.db` and skips key creation entirely.

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

## Ingress

trawld speaks HTTPS natively, so the ingress must proxy to an HTTPS backend. The chart adds the `nginx.ingress.kubernetes.io/backend-protocol: HTTPS` annotation automatically.

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
kubectl exec trawl-0 -- trawl-admin --db /var/lib/trawl/auth.db keys create \
  --role ingest --name "vector"
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

    [auth]
    db_path = "/var/lib/trawl/auth.db"

    [ingest]
    enabled = true
```

## Values Reference

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `image.repository` | string | `ghcr.io/jakub/trawl` | Container image repository |
| `image.tag` | string | `""` (appVersion) | Image tag override |
| `image.pullPolicy` | string | `IfNotPresent` | Image pull policy |
| `service.type` | string | `ClusterIP` | Service type |
| `service.port` | int | `5514` | HTTPS service port |
| `service.syslog.enabled` | bool | `false` | Expose syslog ports |
| `service.syslog.udpPort` | int | `1514` | Syslog UDP port |
| `service.syslog.tcpPort` | int | `1514` | Syslog TCP port |
| `ingress.enabled` | bool | `false` | Create an Ingress resource |
| `persistence.enabled` | bool | `true` | Enable persistent storage |
| `persistence.size` | string | `50Gi` | PVC size |
| `persistence.storageClass` | string | `""` | StorageClass (empty = default) |
| `config.raw` | string | `""` | Raw trawld.toml (bypasses structured values) |
| `config.server.httpAddr` | string | `0.0.0.0:5514` | Bind address |
| `config.server.timeoutSecs` | int | `30` | Query timeout |
| `config.server.maxConcurrentQueries` | string | `""` | DuckDB pool size (empty = CPU count) |
| `config.server.shutdownDrainSecs` | int | `30` | Graceful shutdown timeout |
| `config.data.path` | string | `/var/lib/trawl/data` | Parquet data directory |
| `config.auth.dbPath` | string | `/var/lib/trawl/auth.db` | SQLite auth database path |
| `config.ingest.enabled` | bool | `true` | Enable ingest endpoint |
| `config.ingest.hotBufferMaxBytes` | string | `100M` | Hot buffer memory limit |
| `config.retention.maxAgeDays` | int | `90` | Data retention (days, 0 = disabled) |
| `config.retention.minFreeDiskBytes` | string | `1G` | Disk space retention threshold |
| `config.syslog.enabled` | bool | `false` | Enable native syslog listener |
| `config.scheduler.enabled` | bool | `true` | Enable scheduled queries |
| `tls.mode` | string | `auto` | TLS mode: auto, secret, certManager |
| `tls.secretName` | string | `""` | TLS Secret name (mode=secret) |
| `initAuth.enabled` | bool | `true` | Bootstrap auth on first install |
| `initAuth.keyName` | string | `helm-bootstrap` | Name for the initial admin key |
| `initAuth.createIngestKey` | bool | `true` | Also create an ingest-role key |
| `initAuth.ingestKeyName` | string | `helm-ingest` | Name for the initial ingest key |
| `resources.requests.cpu` | string | `250m` | CPU request |
| `resources.requests.memory` | string | `512Mi` | Memory request |
| `resources.limits.memory` | string | `2Gi` | Memory limit |
| `logLevel` | string | `trawl_server=info` | RUST_LOG value |
| `serviceMonitor.enabled` | bool | `false` | Create prometheus-operator ServiceMonitor |
