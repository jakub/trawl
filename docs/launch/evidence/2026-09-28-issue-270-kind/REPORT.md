# Issue #270 kind proof: trawl-web verifies trawld in `auto` and `certManager` modes

Run 2026-09-28 UTC on `fractal`. It covers this acceptance criterion of issue #270:

> A real cluster reaches a working sign-in through the proxy in tls.mode=auto and
> in certManager mode with a self-signed CA issuer, from a fresh install, without
> the sidecar entering CrashLoopBackOff.

Both legs passed. Each leg did a fresh Helm install into its own namespace of
one fresh kind cluster. In each leg, `trawl-web` stayed Ready with
`restartCount` 0 from start to finish. An API-key sign-in through `trawl-web`
set a session cookie, and authenticated API routes answered 200 through the
proxy. No product defect showed up.

## What was installed

- Source: commit `76ccc826530bcb6bce0a55b320f82887bde14abd` on branch
  `trawl-issue-270-web-trawl-web-always-verifies-trawld`, exported with
  `git archive`. The chart and the image both come from that export.
- Image: `trawl-270-kind:76ccc826530b`, OCI index digest
  `sha256:902dc68aec727c284bc2f81cd4824ce892c0019330d1bebf5380a3fdb084e4ca`,
  label `org.opencontainers.image.revision=76ccc826…`. On the kind node, both
  legs' containers report `imageID`
  `import-2026-09-28@sha256:ddd4a43339357b687b4fd3b89924f7e8669dd1efb6c8667426b7b60f268e392a`,
  the index that `kind load` imported. `image.txt` maps these digests and lists
  the sha256 of each binary, both staged and inside the image.
- kind v0.33.0, node image
  `kindest/node:v1.36.4@sha256:099e049362a1526b2db71494e1947aae99bd16290d7c895f2b7ea312e3cbfaed`,
  Kubernetes v1.36.4, Helm v4.2.2, kubectl v1.36.4, Docker 29.7.2.
- cert-manager v1.21.1 from the release manifest, sha256
  `5f6a499b8c1857d57f560f536e0dcc830914b45c420899fe7ad0692c8624e408`. That is
  the same version and checksum as `2026-09-13-cert-manager`.
- PostgreSQL `postgres:18-alpine@sha256:d3e1620b530c944afa6e887d22eb899824da68e19c52024bf98f5220c88a65b2`,
  one disposable instance per namespace (`postgres.yaml`, `mkdb.sh`).

## Commands, in order

The scripts expect the scratch directory `~/trawl-270-kind-scratch`. It holds
the source export, the build output, the kubeconfig, and a private directory
for passwords, API keys, and cookies.

1. `build-image.sh <worktree> 76ccc826530bcb6bce0a55b320f82887bde14abd`
   builds the image. Output: `build.log`.
2. `kind create cluster --name trawl-270 …` and `kind load docker-image`.
   Output: `kind-create.log`.
3. `mkdb.sh trawl-auto`, then `run-leg.sh auto trawl-auto`. Output:
   `auto/postgres.log` and `auto/transcript.log`.
4. `setup-cert-manager.sh` installs cert-manager, creates namespace `trawl-cm`
   with its PostgreSQL, and applies `cert-manager/issuers.yaml`. Output:
   `cert-manager/setup.log`.
5. `run-leg.sh cert-manager trawl-cm`. Output: `cert-manager/transcript.log`.
6. `kind delete cluster --name trawl-270`. Output: `cleanup.log`.

`run-leg.sh` writes every command it runs into `transcript.log`, followed by
the command's output. It installs with
`helm install trawl <export>/chart/trawl -f <leg>/values.yaml`. It then polls
the pod every 3 seconds until both containers have been Ready for 60 seconds,
and saves the StatefulSet pod spec (`pod-spec.json`), the rendered
`trawld.toml`, and the three container logs. Next it inspects what each
container can see. Last, it creates a `trawl-reader` role and a human key with
`fleet-admin` inside the trawld container, and signs in through
`kubectl port-forward svc/trawl 8090:8090`.

## Leg 1: `tls.mode=auto`

Values: `auto/values.yaml`. The file sets no `tls` values, so the chart
default `auto` applies. `web.enabled` is true, `web.publicOrigins` is
`http://localhost:8090`, and persistence is on (1Gi, kind's `standard` class).

- **Render.** `[web] upstream_ca_path = "/var/lib/trawl/tls/cert.pem"` with no
  `upstream_url`, so trawl-web dials `https://127.0.0.1:5514`
  (`auto/trawld.toml`). In `auto/pod-spec.json`, trawl-web runs as
  `runAsUser: 1001` and mounts the `data` volume with `subPath: tls` at
  `/var/lib/trawl/tls`, `readOnly: true`.
- **Pod status.** trawl-web was Ready 12 seconds after the install and
  trawld 15 seconds after it. Both kept `restartCount` 0 through the 60-second hold and after the
  sign-in.
- **Late certificate.** trawl-web started first and logged
  `upstream_ca_pending` at 18:39:04.26. trawld generated the certificate and
  listened at 18:39:04.31. trawl-web logged `upstream_ca_loaded` at 18:39:34,
  on its 30-second re-read (`auto/trawl-web.log`, `auto/trawld.log`).
- **Key isolation.** In the trawld container, `/var/lib/trawl/tls` holds only
  `cert.pem` (0644, uid 1000). `/var/lib/trawl/tls-key` is 0700 uid 1000 and
  holds `key.pem` (0600, uid 1000). trawl-web runs as `uid=1001 gid=1000` and
  sees only `/var/lib/trawl/tls/cert.pem`. `cat` of
  `/var/lib/trawl/tls-key/key.pem` and of `/var/lib/trawl/tls/key.pem` both
  fail with "No such file or directory". A write into the mount fails with
  "Read-only file system". Both containers read the same certificate
  fingerprint. Its SANs are `localhost`, `127.0.0.1`, and `::1`.
- **Cookie secret under uid 1001.** `/etc/trawl/web.cookie` is mode 0440,
  owner 0, group 1000 (kubelet applied `fsGroup`), and uid 1001 can read it.
- **Sign-in.** Without a cookie, `/api/v1/whoami` answers 401.
  `POST /api/auth/login` answers 200 and sets
  `fleet_session=<redacted>; HttpOnly; SameSite=Lax; Path=/`. With the cookie,
  `/api/auth/me`, `/api/v1/whoami`, and `/api/v1/schema` answer 200. `GET /`
  answers 200 `text/html`, which is the embedded SPA.

## Leg 2: `tls.mode=certManager` with a CA Issuer

`cert-manager/issuers.yaml` creates a `selfSigned` Issuer. That Issuer signs
the CA Certificate `trawl-ca`, and the CA Issuer `trawl-ca-issuer` signs from
the `trawl-ca` Secret. In `cert-manager/values.yaml`, `tls.mode` is
`certManager`, the issuer is `trawl-ca-issuer` (kind `Issuer`), and
`dnsNames` are `trawl.trawl-cm.svc.cluster.local` and `trawl.cm.example.test`.
`tls.upstreamCa` is `secret`, and `tls.upstreamServerName` is left to its
default.

- **Issuance.** Certificate `trawl-tls` reached Ready. Secret `trawl-tls` has
  type `kubernetes.io/tls` and keys `ca.crt`, `tls.crt`, and `tls.key`.
- **Render.** `upstream_url = "https://trawl.trawl-cm.svc.cluster.local:5514"`
  uses the first `dnsNames` entry. `upstream_connect_addr = "127.0.0.1:5514"`
  and `upstream_ca_path = "/etc/trawl/upstream-ca/ca.crt"`
  (`cert-manager/trawld.toml`). In `cert-manager/pod-spec.json`, trawl-web
  mounts volume `upstream-ca` at `/etc/trawl/upstream-ca`. That volume
  projects only item `ca.crt` of Secret `trawl-tls`, with `optional: true`.
- **Pod status.** trawl-web was Ready 12 seconds after install and kept
  `restartCount` 0. trawld took about 90 seconds to pass its readiness probe.
  Its migration and boot took 36 seconds, and one startup probe failed before
  it listened. trawl-web stayed Ready the whole time.
- **Immediate load.** The CA file existed when trawl-web started, so it logged
  no `upstream_ca_pending`. Its config line shows
  `upstream=https://trawl.trawl-cm.svc.cluster.local:5514`.
- **Key isolation.** trawl-web (`uid=1001 gid=1000`) sees only `ca.crt` in
  `/etc/trawl/upstream-ca`. `/etc/trawl` holds only `trawld.toml`,
  `upstream-ca`, and `web.cookie`. `cat /etc/trawl/tls/tls.key` and
  `cat /etc/trawl/upstream-ca/tls.key` both fail with "No such file or
  directory". The pinned CA's subject is `CN=trawl-270 kind proof CA`. trawld's
  leaf certificate has that issuer and the two requested DNS SANs.
- **Sign-in.** The results match leg 1: 401 without a cookie, login 200 with a
  redacted `fleet_session` cookie, then 200 from `/api/auth/me`,
  `/api/v1/whoami`, and `/api/v1/schema` through the proxy. trawl-web dialed
  loopback and verified trawld's certificate for the name
  `trawl.trawl-cm.svc.cluster.local` against the pinned CA.

## Caveats

- **Build path.** The release builds x86_64 with cargo-zigbuild against a
  glibc 2.31 floor. This host has no cargo-zigbuild, so the image binaries
  were built with plain `cargo build` inside `rust:1.98.0-bookworm`, with the
  same package set, `--no-default-features`, and rpath flags as
  `build-distribution.sh --image-only`. The runtime image is the repository
  `Dockerfile` over a `distribution.py stage --image-only` context. trawld
  logs `git_sha="unknown"` because the source was a `git archive` export.
  The revision label and `distribution.json` record the SHA instead.
- **DuckDB download.** The host's build environment fails curl certificate
  verification for the DuckDB archive, so the checksum-named archive was
  copied from the main checkout's `target/duckdb-runtime-cache`.
  `distribution.py prepare` checked its sha256 before extraction.
- **Storage.** kind's `standard` class is the local-path provisioner. Its
  volumes are hostPath directories with mode 0777, and kubelet does not apply
  `fsGroup` to them. The kubelet-created `tls` subPath directory was therefore
  `0777 root:root`, and trawld could write into it. This run does not show the
  same directory on a volume whose root is not world-writable, such as a CSI
  block volume with `fsGroup`.
- **Not exercised here.** The run had no request during the pending window,
  so the 503 `upstream certificate not available` answer does not appear. It
  had no certificate rotation and no cert-manager renewal either. The
  `trawl-web` tests `late_ca_is_loaded` and `rotated_ca_is_reloaded` cover
  those paths.
- **Observation outside #270.** In `certManager` mode, trawld's own
  `/etc/trawl/tls/tls.key` is mode 0644 (group 1000), the Secret volume
  default. Only the trawld container mounts it. The sidecar does not.
- **Transcript gaps.** `setup-cert-manager.sh` sends `kubectl apply -f
  cert-manager.yaml` to `/dev/null`, so `setup.log` does not show that command
  or its output. The `wait` for the three deployments that follows it does
  appear. Container logs are saved with ANSI colour codes removed.

## Earlier runs

Before this run, the same procedure ran against commit `397383fa` on a
separate cluster. The first `auto` attempt there failed the sign-in with 502.
The harness caused it: `run-leg.sh` sent the `keys create` metadata from
stderr into the token file, so the login body held more than the key. The
redirect was fixed. A fresh `auto` install and a `certManager` install at
`397383fa` then both passed. Commit `76ccc826` then changed trawl-web's CA
re-read. The cluster was deleted, and everything above was redone at
`76ccc826` from a new cluster. The `397383fa` artifacts are not kept.

## Secrets and cleanup

This directory holds no database password, API key, session cookie, cookie
key, or private key. A scan checked every generated password, token, and
cookie value against these files and found none. Key prefixes do appear.
They are not secret. `cleanup.log` shows the cluster deleted and no
`trawl-270` container left. After the commit, the scratch directory and the
`trawl-270-kind` and `trawl-270-builder` images were removed.
`SHA256SUMS` covers every other file here.
