# trawl-web always verifies trawld

status: accepted (2026-09-28), prep record for the server-host doctors (ADR-0047 slice 2); amends ADR-0045; amended (2026-11-23): rulings moved from code comments, see the Amendment

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

## Amendment (2026-11-23): rulings moved from code comments

A comment audit on 2026-11-23 removed these rulings from the code, where they were the only record. They were already in force. None is a new decision.

- **A read of the pin file never runs on a runtime worker, and a request waits for it at most `CA_READ_WAIT`.** Opening, reading and parsing the file are synchronous, and a file on a stalled network volume blocks for as long as the volume stalls. So `Upstream` runs each read on tokio's blocking pool, one at a time, and opens the file without waiting, which refuses a FIFO or any other path that is not a regular file. A request that reaches the limit gets the same 503 `upstream certificate not available` as a missing file, and the read goes on. That 503 leaves the session cookie alone, because the session is valid and only the hop to trawld is not ready.
- **Every refusal after a good load keeps the last good client.** A file that disappears, cannot be read, is larger than `MAX_PIN_FILE_BYTES` or does not parse is a refusal, just as a change that does not parse is. A rotation that briefly removes the file must not take sign-in down. Each distinct refusal logs one `upstream_ca_refused` warning, so a re-read every `CA_REREAD_INTERVAL` does not repeat it.
- **trawl-web reads a pin file of at most 1 MiB (`MAX_PIN_FILE_BYTES` in `config.rs`).** trawld's generated certificate is under 1 KiB, and a whole public root bundle is a few hundred KiB. Reading a larger file whole, at startup or on every re-read, would let whoever can write the path make trawl-web allocate past a container's memory limit.
- **The upstream client follows no redirect in any trust mode, and every browser-facing route answers an upstream 3xx with a 502 and no `Location`.** Every upstream request carries the signed-in user's key as a bearer token. A followed 3xx would send that key to any host the trust mode accepts, which under the platform roots means any host with a public certificate. A 3xx passed to the browser is no better: a `Location` that names another port on the browser's host carries the session cookie there, because a cookie's scope ignores the port. `refuse_redirect` in `routes/proxy.rs` refuses 304 too, because trawld sends no validators.
- **The upstream client ignores proxy settings such as `HTTPS_PROXY`, and the web doctor builds its client through the same `upstream_client`.** A proxy resolves the host name on its own side, past `upstream_connect_addr`. The doctor shares the proxy's trust and dialing rules, so a doctor pass means the proxy's requests reach trawld the same way.
- **Nothing shipped names `TRAWL_WEB_INSECURE_UPSTREAM`.** A config line, chart value, development-stack entry or document that names it would teach an operator a setting that does nothing. The test `insecure_upstream_switch_is_gone` in `tests/trust_modes.rs` enforces this. It skips `docs/launch/evidence/`, because those files are dated records of what earlier builds did, and a record is not rewritten.
