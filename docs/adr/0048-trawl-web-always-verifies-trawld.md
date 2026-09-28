# trawl-web always verifies trawld

status: accepted (2026-09-28), prep record for the server-host doctors (ADR-0047 slice 2); amends ADR-0045

`trawl-web` forwards every signed-in request to trawld with the user's key. It has three ways to trust trawld's certificate: the platform roots, a pinned CA file (`[web] upstream_ca_path`), or `TRAWL_WEB_INSECURE_UPSTREAM`, which skips verification for a loopback upstream. The Debian package and the trial pin a CA. The Helm chart and the development stack use the switch. Under ADR-0047 a doctor cannot pass a connection it did not verify, so every Helm installation's web doctor would fail forever. Pinning only the chart's default certificate mode would have left the switch in place for the other two modes.

## Decision

**trawl-web verifies trawld's certificate in every installation.** `TRAWL_WEB_INSECURE_UPSTREAM` is removed: the variable, its loopback rules, and its tests. Nothing shipped sets it. The two remaining modes are the platform roots and a pinned CA file. The upstream must be `https` in both.

**A certificate names trawld; the connection may go elsewhere.** A new setting, `[web] upstream_connect_addr`, sends the connection to a fixed socket address while TLS still verifies the host name in `upstream_url`. A sidecar can then dial `https://trawl.lab.example:5514`, connect to `127.0.0.1:5514`, and check that the certificate covers `trawl.lab.example`. The setting accepts only an IP address and port, and it requires an `upstream_url` whose host is a name.

**A pinned CA file may appear late and may change.** `trawl-web` starts even when `upstream_ca_path` does not exist yet, because trawld writes its generated certificate only on its first start. Until the file exists, a request that needs trawld gets 503 `upstream certificate not available`, and `/healthz` stays live. `trawl-web` re-reads the file on an interval and on the first request after it appears. A file that exists but does not parse is refused at startup, as today. A later change that does not parse keeps the last good roots and logs the refusal. This also covers a self-signed certificate that an operator rotates.

**Each channel pins what trawld serves:**

- **Debian:** unchanged. `upstream_ca_path` names trawld's generated certificate. The unit no longer restart-loops before trawld's first start.
- **Helm, `tls.mode=auto`:** the sidecar mounts only the `tls/` directory of trawld's data volume, read-only, and pins the generated certificate. That certificate covers `localhost` and `127.0.0.1`.
- **Helm, `tls.mode=secret` or `certManager`:** the sidecar dials `tls.upstreamServerName` and connects to trawld over loopback through `upstream_connect_addr`. For `certManager` the name defaults to the first `dnsNames` entry that is not a wildcard. `tls.upstreamCa` is required and has no default:
  - `secret` pins the Secret's `ca.crt`.
  - `system` uses the platform roots, for a public issuer.
  - A path pins a CA the operator mounts.

  The chart refuses to render without `tls.upstreamCa`, because it cannot see inside the Secret to choose one.
- **Trial:** unchanged. It pins its own CA.
- **Development stack:** `trawl-web` pins the certificate that the development trawld generates.

## Considered options

**Pin only `tls.mode=auto`**, rejected by the operator. The other two modes would keep the switch, and their web doctors would fail on every run. Removing the switch removes a trust mode that every later change would have to reason about.

**Pin the exact certificate trawld serves**, rejected. It ignores host names, but `trawl-web` would have to reload the pin at the same moment trawld reloads its certificate. Every cert-manager renewal would then open a window in which sign-in fails. CA verification survives a renewal, because the CA does not change.

**Keep the switch for hand-built installations**, rejected. A loopback connection inside one pod or host is already hard to intercept, but the switch cannot be told apart from a misconfiguration by a doctor or a reader, and a pinned CA costs one setting.

**Refuse to start until the CA file exists**, as before, rejected. Kubernetes restarts a failing container with a growing backoff, so a first Helm install could wait minutes before sign-in works.
