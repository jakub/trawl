# trawl Helm Chart

Self-hosted log collection, storage, and search for homelabs and small-to-medium infra.

## Quick Start

Choose a Kubernetes context, namespace, release name, and chart version first.
The examples below use `TRAWL_CONTEXT`, `TRAWL_NAMESPACE`, and `TRAWL_RELEASE`
for that selected target. Provision the two database Secrets through your secret
manager: `fleet-db` with key `DATABASE_URL`, and `trawl-db` with key
`TRAWL_DATABASE_URL`. Do not put real DSNs in shell history.

```bash
helm install "$TRAWL_RELEASE" oci://ghcr.io/jakub/charts/trawl \
  --version "$TRAWL_CHART_VERSION" \
  --kube-context "$TRAWL_CONTEXT" --namespace "$TRAWL_NAMESPACE" \
  --set auth.database.existingSecret=fleet-db \
  --set storage.database.existingSecret=trawl-db \
  --set-string 'web.publicOrigins[0]=http://localhost:8090' \
  --set web.allowInsecureCookies=true

kubectl --context "$TRAWL_CONTEXT" --namespace "$TRAWL_NAMESPACE" \
  port-forward "svc/$TRAWL_RELEASE" 5514:5514 8090:8090
```

This example deliberately uses HTTP for a local browser port-forward, so it
allows insecure cookies. Use HTTPS origins and secure cookies for a normal
reverse-proxy deployment. If `nameOverride` or `fullnameOverride` changes the
Service name, use the rendered name in the port-forward command.
Create keys through the [access guide](../../docs/src/content/docs/operate/access.md),
then configure a named CLI profile for this target. See the
[deployment guide](../../docs/src/content/docs/operate/deployment.md) for full
preparation and verification.

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

There is no fallback between the two URLs — provision both databases. See [database preparation](../../docs/src/content/docs/operate/deployment.md#provision-the-databases) for ownership and migration requirements.

## Prerequisites

- Kubernetes 1.26+ (for HTTPS health probes)
- Helm 3.x
- A StorageClass that supports `ReadWriteOnce` PVCs
- A reachable postgres with the `fleet` and `trawl` databases provisioned

## Installing

Every install needs both database Secret value names, including when the web
sidecar is disabled. These source examples use an already selected checkout and
explicit target. Render with `helm template` and the same values before applying.

```bash
# Source chart, browser behind an existing HTTPS ingress.
helm install "$TRAWL_RELEASE" ./chart/trawl \
  --kube-context "$TRAWL_CONTEXT" --namespace "$TRAWL_NAMESPACE" \
  --set auth.database.existingSecret=fleet-db \
  --set storage.database.existingSecret=trawl-db \
  --set-string 'web.publicOrigins[0]=https://trawl.example.com'

# Source chart, API only.
helm install "$TRAWL_RELEASE" ./chart/trawl \
  --kube-context "$TRAWL_CONTEXT" --namespace "$TRAWL_NAMESPACE" \
  --set auth.database.existingSecret=fleet-db \
  --set storage.database.existingSecret=trawl-db \
  --set web.enabled=false
```

For repeatable upgrades, keep these values plus storage, image version, ingress,
and TLS choices in a complete values file. `web.publicOrigins` allows an origin;
it does not create the ingress. The chart's `values.yaml` owns the full key list.

## Auth

The init container runs `fleet-admin migrate` against the shared Fleet keystore.
The daemon separately migrates its dedicated Trawl app-state database. On a fresh
Fleet database, create roles before keys; the migration only converts legacy
roles that already exist. A role name does not imply permissions.

Use [access administration](../../docs/src/content/docs/operate/access.md) for
current role creation and rotation. `fleet-admin` expects `DATABASE_URL`; trawld
receives `FLEET_DATABASE_URL`, so an administrative command executed inside the
daemon container does not automatically inherit fleet-admin's required variable.

## TLS

trawld always speaks HTTPS. Three modes are available:

| Mode | Description |
|------|-------------|
| `auto` (default) | trawld generates a self-signed ECDSA P-256 cert at startup. Configure client certificate trust; `--insecure` is for a deliberate local test. |
| `secret` | Mount an existing Kubernetes TLS Secret. Set `tls.secretName`. |
| `certManager` | Create a cert-manager Certificate. Set `tls.certManager.issuerRef`. |

```yaml
# Example: existing TLS secret
tls:
  mode: secret
  secretName: trawl-tls-cert

```

Or use cert-manager:

```yaml
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

Three things to keep in mind:

1. **API clients keep talking to trawld directly.** `trawl query`, the `trawl-client` library, and vector all use bearer tokens against trawld's HTTPS port (5514). The web-UI ingress rejects non-cookie auth and blocks `/api/v1/ingest` outright. In-cluster clients hit the Service on 5514; external clients need a LoadBalancer or a second ingress with `ingress.backend: trawld`.
2. **Keep `Secure` for browser HTTPS.** The proxy sets `Secure` on session cookies. Browsers accept these cookies over HTTPS even when the ingress forwards plain HTTP to the Service. Set `web.allowInsecureCookies: true` only when the browser itself connects over HTTP, such as on a local development network.
3. **The ingress host is not the browser origin.** `web.publicOrigins` is required whenever `web.enabled`, and the chart refuses to render without it. State what the address bar shows, scheme and port included; the chart never derives it from `ingress.hosts` or `httpRoute.hostnames`, because a host rule carries no scheme and one install often answers to several names. The proxy compares a browser's `Origin` header against this list whole, and consults no forwarding header (ADR-0016), so a TLS-terminating ingress needs the `https://` origin here even though it forwards plain HTTP.

```yaml
web:
  publicOrigins:
    - https://trawl.example.com
```

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

The chart's HTTPRoute targets the raw trawld HTTPS Service port, even when the
web sidecar is enabled. It is not a browser-proxy route. Configure a separate
route to the web Service port if the browser also needs Gateway API exposure.
For an API route:

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

Create a dedicated ingest token (the `trawl-ingest` role must exist first — see [Auth](#auth)):

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
| `web.publicOrigins` | list | `[]` | **Required when `web.enabled`.** Browser-visible origins allowed to carry a session cookie, e.g. `https://trawl.example.com`. Compared whole (scheme, host, port); never derived from ingress hosts. Rendered into `[web] public_origins` and passed to the sidecar as the identical `FLEET_SESSION_PUBLIC_ORIGINS`, so it survives a `config.raw`. The environment wins on read, and trawl-web warns only when the two lists differ |
| `web.sessionTtlSecs` | int | `86400` | Browser session lifetime (seconds) |
| `web.allowInsecureCookies` | bool | `false` | Drop `Secure` from session cookies for browser HTTP. Keep false for browser HTTPS, including when an ingress terminates TLS |
| `web.logLevel` | string | `trawl_web=info,fleet_auth=info` | RUST_LOG for the sidecar (the trawld `logLevel` names no trawl-web target) |
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
| `crashDump.enabled` | bool | `false` | Enable minidump capture; requires `persistence.enabled=true`. Adds `SYS_PTRACE` to the trawld container and nothing else: the runtime hands an added capability to the container's init process as permitted and effective, and the binary's `cap_sys_ptrace+p` file capability keeps the bit across trawld's exec of the monitor, so `allowPrivilegeEscalation` stays `false`. Restricted Pod Security still refuses any added capability except `NET_BIND_SERVICE`, so an enabled install cannot run under that profile. For what a dump contains, the yama `ptrace_scope` limits, and the equivalent Debian procedure, see [Crash dumps](https://trawl.sh/reference/crash-dumps/) |
| `crashDump.size` | string | `2Gi` | Crash-dump PVC size |
| `crashDump.storageClass` | string | `""` | Crash-dump StorageClass (empty = default) |
| `crashDump.mountPath` | string | `/var/lib/trawl/cores` | Crash-dump PVC mount path |
| `crashDump.retain` | int | `10` | Maximum number of retained minidumps |
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
| `config.ingest.severityFrom` | list | `[severity, severity_text, level]` | Wire keys `_severity` derives from, first mappable wins. Bare names, or `{field, dialect}` where `dialect` (`otel`\|`syslog`) governs numerics only. Boot-fatal on a bad entry |
| `config.ingest.timeFrom` | list | `[_time, timestamp, "@timestamp"]` | Wire keys `_time` derives from, first present wins. Must contain `_time`; a `dialect` here is an error |
| `config.retention.maxAgeDays` | int | `90` | Age limit in days for every env without an `envs` entry (0 = keep forever for those envs) |
| `config.retention.envs` | map | `{}` | Per-env age limits, env name to days, rendered as `[retention.env.<name>]` (0 = keep forever; still deleted last under disk pressure) |
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
| `logLevel` | string | `trawl_server=info,trawld=info,fleet_auth=info,auth.backend=info,storage.backend=info,preauth.transport=info` | RUST_LOG for the trawld container (keep the backend alarm and preauth.transport targets when customizing) |
| `serviceMonitor.enabled` | bool | `false` | Create prometheus-operator ServiceMonitor |
