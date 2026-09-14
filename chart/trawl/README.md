# trawl Helm chart

Installs one `trawld` StatefulSet with one replica, a `trawl-web` sidecar for
the browser UI, a Service, and optional Ingress, HTTPRoute, ServiceMonitor,
and crash-dump volume. trawld is single-node, and the chart never scales above
one replica.

## Quick start

You need Kubernetes 1.26 or later, Helm 3, a StorageClass that provides
`ReadWriteOnce` volumes, PostgreSQL with the `fleet` and `trawl` databases from
[Provision the databases](https://trawl.sh/operate/deployment/#provision-the-databases),
a Secret with the Fleet DSN under key `DATABASE_URL`, and a Secret with the
Trawl DSN under key `TRAWL_DATABASE_URL`.

This install reaches the browser UI through a local port-forward, so its
origin is `http://localhost:8090` and it allows insecure cookies. Use an HTTPS
origin and secure cookies behind a reverse proxy or ingress.

```bash
helm upgrade --install trawl oci://ghcr.io/jakub/charts/trawl \
  --namespace trawl --create-namespace \
  --set auth.database.existingSecret=fleet-db \
  --set storage.database.existingSecret=trawl-db \
  --set-string 'web.publicOrigins[0]=http://localhost:8090' \
  --set web.allowInsecureCookies=true

kubectl port-forward --namespace trawl svc/trawl 5514:5514 8090:8090
```

Open `http://localhost:8090` and sign in with a key from
[Create roles and keys](https://trawl.sh/operate/access/#create-roles-and-keys).
`web.publicOrigins` is required while `web.enabled` is true, and the render
fails without it. Set `web.enabled=false` for an API-only install.
[Install with Helm](https://trawl.sh/operate/deployment/#install-with-helm)
covers a values file, an ingress with an HTTPS origin, and verification.

## Install from a source checkout

A source chart requires `image.tag`. Select an image built from the same Git
revision as the checkout and pass `--set-string image.tag="$TRAWL_IMAGE_TAG"`
when replacing the OCI chart reference above with `./chart/trawl`. Helm cannot
verify the image's source revision; check the release or development build
record. Source `Chart.yaml` versions do not select an image.

Published release packages set `image.tag` to their matching release image,
so the OCI installation above needs no image override.

## Daemon API certificates

The default `tls.mode: auto` lets trawld create a self-signed certificate.
For trusted API TLS, choose one of these routes:

- `tls.mode: secret` mounts the existing `tls.secretName` from the release namespace. The Secret must contain `tls.crt` and `tls.key`.
- `tls.mode: certManager` creates a `cert-manager.io/v1` Certificate named `<fullname>-tls`. An existing issuer supplies its certificate and key in the same-named Secret. Set `tls.certManager.issuerRef.name`, select `Issuer` or `ClusterIssuer`, and list all client-facing names in `tls.certManager.dnsNames`.

The chart installs neither cert-manager nor an issuer. An `Issuer` must be in
the release namespace; a `ClusterIssuer` is cluster-scoped. The default issuer
group is `cert-manager.io`. The generated Secret name must not also name a
Fleet/Trawl database Secret or browser-cookie Secret.

The Certificate covers the daemon API. Browser ingress uses `ingress.tls`
separately. If ingress deliberately reuses the generated Secret, its hosts
must also be covered, and ingress-shim issuer annotations must be absent so
two Certificates do not manage that Secret. The sidecar keeps its existing
loopback-only HTTPS connection with certificate verification disabled.

Chart-managed TLS mounts require structured config values. `config.raw` is
accepted only with `tls.mode: auto`, where the raw TOML controls TLS and the
chart mounts no TLS Secret. Helm cannot validate arbitrary TOML certificate
paths against its volume mounts.

Follow [configure the daemon API certificate](https://trawl.sh/operate/deployment/#configure-the-daemon-api-certificate)
for complete values, issuance checks, and client verification. cert-manager's
[Certificate documentation](https://cert-manager.io/docs/usage/certificate/)
describes issuance, Secret contents, and renewal. trawld checks mounted
certificate files every `config.server.tlsReloadIntervalSecs` seconds.

## Values

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `image.repository` | string | `ghcr.io/jakub/trawl` | Container image repository |
| `image.tag` | string | `""` in source; release version in packages | Required image tag, built from the same revision as the chart |
| `image.pullPolicy` | string | `IfNotPresent` | Image pull policy |
| `imagePullSecrets` | list | `[]` | Pull secrets for the pod |
| `nameOverride` | string | `""` | Replaces the chart name in resource names |
| `fullnameOverride` | string | `""` | Replaces the full resource name |
| `service.type` | string | `ClusterIP` | Service type |
| `service.port` | int | `5514` | trawld HTTPS port |
| `service.webPort` | int | `8090` | trawl-web port, published only when `web.enabled` |
| `service.annotations` | object | `{}` | Service annotations |
| `service.syslog.enabled` | bool | `false` | Publish the syslog ports. Needs `config.syslog.enabled` |
| `service.syslog.udpPort` | int | `1514` | Syslog UDP port |
| `service.syslog.tcpPort` | int | `1514` | Syslog TCP port |
| `ingress.enabled` | bool | `false` | Create an Ingress |
| `ingress.backend` | string | `web` | `web` targets trawl-web over HTTP. `trawld` targets the HTTPS API and needs the `nginx.ingress.kubernetes.io/backend-protocol: "HTTPS"` annotation |
| `ingress.className` | string | `""` | IngressClass name |
| `ingress.annotations` | object | `{}` | Ingress annotations |
| `ingress.hosts` | list | one host `trawl.local` with path `/` | Ingress host rules |
| `ingress.tls` | list | `[]` | Ingress TLS entries |
| `httpRoute.enabled` | bool | `false` | Create a Gateway API HTTPRoute to the trawld HTTPS port. The gateway needs a `BackendTLSPolicy` that trusts the backend certificate |
| `httpRoute.parentRef.name` | string | `""` | Gateway name |
| `httpRoute.parentRef.namespace` | string | `""` | Gateway namespace |
| `httpRoute.parentRef.sectionName` | string | `websecure` | Gateway listener name |
| `httpRoute.hostnames` | list | `[]` | HTTPRoute hostnames |
| `persistence.enabled` | bool | `true` | Create a PVC for the data directory and generated TLS files |
| `persistence.storageClass` | string | `""` | StorageClass. Empty uses the cluster default |
| `persistence.size` | string | `50Gi` | PVC size |
| `persistence.accessModes` | list | `[ReadWriteOnce]` | PVC access modes |
| `crashDump.enabled` | bool | `false` | Capture minidumps on a fatal signal. Adds `CAP_SYS_PTRACE` to the trawld container only, and needs `persistence.enabled`. See [Crash dumps](https://trawl.sh/reference/crash-dumps/) |
| `crashDump.size` | string | `2Gi` | Crash-dump PVC size |
| `crashDump.storageClass` | string | `""` | Crash-dump StorageClass. Empty uses the cluster default |
| `crashDump.mountPath` | string | `/var/lib/trawl/cores` | Crash-dump mount path, passed as `TRAWL_CRASH_DUMP_DIR` |
| `crashDump.retain` | int | `10` | Dumps to keep, passed as `TRAWL_CRASH_DUMP_RETAIN` |
| `config.raw` | string | `""` | Complete `trawld.toml` text. Replaces every `config.*` value below. Requires `tls.mode: auto`; raw TOML owns TLS configuration and the chart mounts no TLS Secret. The DSNs still arrive from the Secrets, and `web.publicOrigins` still reaches trawl-web |
| `config.server.httpAddr` | string | `0.0.0.0:5514` | `[server] http_addr` |
| `config.server.timeoutSecs` | int | `30` | `[server] timeout_secs` |
| `config.server.maxConcurrentQueries` | string | `""` | `[server] max_concurrent_queries`. Empty uses the CPU count |
| `config.server.maxResultRows` | int | `100000` | `[server] max_result_rows` |
| `config.server.maxExportRows` | int | `1000000` | `[server] max_export_rows` |
| `config.server.maxRequestBodyBytes` | string | `128K` | `[server] max_request_body_bytes` |
| `config.server.maxConcurrentRequests` | int | `256` | `[server] max_concurrent_requests` |
| `config.server.shutdownDrainSecs` | int | `30` | `[server] shutdown_drain_secs`, also the pod's `terminationGracePeriodSeconds` |
| `config.server.maxSseConnections` | int | `32` | `[server] max_sse_connections` |
| `config.server.tlsReloadIntervalSecs` | int | `300` | `[server] tls_reload_interval_secs`, rendered for `tls.mode` `secret` and `certManager` |
| `config.server.corsAllowedOrigins` | list | `[]` | `[server] cors_allowed_origins` |
| `config.server.schemaCacheTtlSecs` | int | `60` | `[server] schema_cache_ttl_secs` |
| `config.server.maxQueryHistory` | int | `1000` | `[server] max_query_history` |
| `config.server.rateLimit.defaultRpm` | int | `100` | `[server.rate_limit] default_rpm`, requests per minute per key on the API routes. `0` disables |
| `config.server.rateLimit.ingestRpm` | int | `1000` | `[server.rate_limit] ingest_rpm`, requests per minute per key on `/api/v1/ingest`. `0` disables |
| `config.data.path` | string | `/var/lib/trawl/data` | `[data] path` |
| `config.auth.auditIntervalSecs` | int | `30` | `[auth] audit_interval_secs` |
| `config.ingest.enabled` | bool | `true` | `[ingest] enabled` |
| `config.ingest.maxBodyBytes` | string | `16M` | `[ingest] max_body_bytes` |
| `config.ingest.walDir` | string | `""` | `[ingest] wal_dir`. Empty uses `{data.path}/wal` |
| `config.ingest.compactionIntervalSecs` | int | `10` | `[ingest] compaction_interval_secs` |
| `config.ingest.internalTelemetry` | bool | `true` | `[ingest] internal_telemetry` |
| `config.ingest.dailyRollup` | bool | `true` | `[ingest] daily_rollup` |
| `config.ingest.eventBusCapacity` | int | `4096` | `[ingest] event_bus_capacity` |
| `config.ingest.hotBufferMaxEvents` | int | `100000` | `[ingest] hot_buffer_max_events` |
| `config.ingest.hotBufferMaxBytes` | string | `100M` | `[ingest] hot_buffer_max_bytes` |
| `config.ingest.statsIntervalSecs` | int | `60` | `[ingest] stats_interval_secs` |
| `config.ingest.telemetryFlushIntervalSecs` | int | `1` | `[ingest] telemetry_flush_interval_secs` |
| `config.ingest.severityFrom` | list | `[severity, severity_text, level]` | `[ingest] severity_from`. Each entry is a name or `{field, dialect}` with dialect `otel` or `syslog` |
| `config.ingest.timeFrom` | list | `[_time, timestamp, "@timestamp"]` | `[ingest] time_from`. Must contain `_time` |
| `config.retention.maxAgeDays` | int | `90` | `[retention] max_age_days`. `0` keeps data until disk pressure |
| `config.retention.minFreeDiskBytes` | string | `1G` | `[retention] min_free_disk_bytes`. `0` disables disk-pressure deletion |
| `config.retention.retentionIntervalSecs` | int | `3600` | `[retention] retention_interval_secs` |
| `config.retention.envs` | map | `{}` | Environment name to `max_age_days`, rendered as `[retention.env.<name>]` tables |
| `config.syslog.enabled` | bool | `false` | `[syslog] enabled` |
| `config.syslog.udpAddr` | string | `0.0.0.0:1514` | `[syslog] udp_addr` |
| `config.syslog.udpEnabled` | bool | `true` | `[syslog] udp_enabled` |
| `config.syslog.tcpAddr` | string | `0.0.0.0:1514` | `[syslog] tcp_addr` |
| `config.syslog.tcpEnabled` | bool | `true` | `[syslog] tcp_enabled` |
| `config.syslog.maxTcpConnections` | int | `256` | `[syslog] max_tcp_connections` |
| `config.syslog.batchIntervalMs` | int | `500` | `[syslog] batch_interval_ms` |
| `config.syslog.batchMaxEvents` | int | `1000` | `[syslog] batch_max_events` |
| `config.syslog.defaultService` | string | `syslog` | `[syslog] default_service` |
| `config.syslog.tcpIdleTimeoutSecs` | int | `60` | `[syslog] tcp_idle_timeout_secs` |
| `config.syslog.maxEventsPerConnection` | int | `100000` | `[syslog] max_events_per_connection` |
| `config.syslog.consecutiveSendFailuresLimit` | int | `100` | `[syslog] consecutive_send_failures_limit` |
| `config.syslog.allowCidrs` | list | `[]` | `[syslog] allow_cidrs` |
| `config.syslog.sourceServiceMap` | map | `{}` | `[syslog] source_service_map`, source IP to service name |
| `config.syslog.channelCapacity` | int | `10000` | `[syslog] channel_capacity` |
| `config.scheduler.enabled` | bool | `true` | `[scheduler] enabled` |
| `config.scheduler.pollIntervalSecs` | int | `10` | `[scheduler] poll_interval_secs` |
| `config.scheduler.reportMaxRows` | int | `10000` | `[scheduler] report_max_rows` |
| `config.scheduler.maxRunsPerSchedule` | int | `100` | `[scheduler] max_runs_per_schedule` |
| `config.scheduler.reportRetentionDays` | int | `30` | `[scheduler] report_retention_days` |
| `config.scheduler.maxCatchupIntervals` | int | `24` | `[scheduler] max_catchup_intervals` |
| `tls.mode` | string | `auto` | `auto` lets trawld generate a self-signed certificate. `secret` mounts `tls.secretName`. `certManager` creates a Certificate in the release namespace and mounts its Secret `<fullname>-tls` |
| `tls.secretName` | string | `""` | TLS Secret name for `tls.mode: secret` |
| `tls.certManager.issuerRef.name` | string | `""` | Required for `certManager`. Name of an existing issuer |
| `tls.certManager.issuerRef.kind` | string | `ClusterIssuer` | `Issuer` in the release namespace or `ClusterIssuer` |
| `tls.certManager.issuerRef.group` | string | `cert-manager.io` | Issuer API group. No namespace field is supported |
| `tls.certManager.dnsNames` | list | `[]` | Required DNS SANs for `certManager`. Must cover API ingress/HTTPRoute hostnames. Wildcards cover one label. IP-address certificates use `secret` mode |
| `auth.database.existingSecret` | string | `""` | Secret holding the Fleet DSN. Required |
| `auth.database.existingSecretKey` | string | `DATABASE_URL` | Key in that Secret. Injected into `init-auth` as `DATABASE_URL` and into trawld as `FLEET_DATABASE_URL` |
| `storage.database.existingSecret` | string | `""` | Secret holding the Trawl app-state DSN. Required |
| `storage.database.existingSecretKey` | string | `TRAWL_DATABASE_URL` | Key in that Secret. Injected into trawld as `TRAWL_DATABASE_URL` |
| `initAuth.enabled` | bool | `true` | Run `fleet-admin migrate` in an init container on every pod start |
| `initAuth.resources` | object | cpu `50m`, memory `64Mi` to `128Mi` | Init container resources |
| `resources` | object | cpu `250m`, memory `512Mi` to `2Gi`, no CPU limit | trawld container resources |
| `extraEnv` | list | `[]` | Extra environment variables for the trawld container |
| `web.enabled` | bool | `true` | Run the trawl-web sidecar |
| `web.bindAddr` | string | `0.0.0.0:8090` | trawl-web bind address, rendered as `[web] bind_addr`. Must be reachable from the pod IP |
| `web.publicOrigins` | list | `[]` | Browser-visible origins allowed to carry a session cookie, compared whole. Required when `web.enabled`. Rendered as `[web] public_origins` and passed as `FLEET_SESSION_PUBLIC_ORIGINS` |
| `web.sessionTtlSecs` | int | `86400` | `[web] session_ttl_secs` |
| `web.allowInsecureCookies` | bool | `false` | `[web] allow_insecure_cookies`. Set `true` only when the browser connects over HTTP |
| `web.sharedDomain` | string | `""` | `[web] shared_domain`, the parent domain for a session shared with other Fleet applications. Empty scopes the cookie to the origin |
| `web.resources` | object | cpu `50m`, memory `64Mi` to `256Mi` | Sidecar resources |
| `web.extraEnv` | list | `[]` | Extra environment variables for the sidecar |
| `web.logLevel` | string | `trawl_web=info,fleet_auth=info` | `RUST_LOG` for the sidecar. Keep both targets |
| `web.cookieSecret.existingSecret` | string | `""` | Secret holding the 32-byte session key. Empty makes the chart generate one and keep it across upgrades |
| `web.cookieSecret.existingSecretKey` | string | `cookie.key` | Key in that Secret |
| `logLevel` | string | `trawl_server=info,trawld=info,fleet_auth=info,auth.backend=info,storage.backend=info,preauth.transport=info` | `RUST_LOG` for the trawld container. Keep all six targets |
| `serviceMonitor.enabled` | bool | `false` | Create a prometheus-operator ServiceMonitor for `/metrics` |
| `serviceMonitor.interval` | string | `30s` | Scrape interval |
| `serviceMonitor.scrapeTimeout` | string | `10s` | Scrape timeout |
| `serviceMonitor.namespace` | string | `""` | ServiceMonitor namespace. Empty uses the release namespace |
| `serviceAccount.create` | bool | `true` | Create a ServiceAccount |
| `serviceAccount.annotations` | object | `{}` | ServiceAccount annotations |
| `serviceAccount.name` | string | `""` | ServiceAccount name. Empty derives it from the release |
| `podAnnotations` | object | `{}` | Pod annotations |
| `podSecurityContext` | object | uid, gid, and fsGroup `1000` | Pod security context |
| `securityContext` | object | read-only root filesystem, non-root, no privilege escalation, all capabilities dropped | Container security context |
| `nodeSelector` | object | `{}` | Node selector |
| `tolerations` | list | `[]` | Tolerations |
| `affinity` | object | `{}` | Affinity rules |
