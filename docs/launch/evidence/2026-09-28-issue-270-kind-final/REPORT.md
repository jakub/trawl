# Issue #270 kind proof at the final head: trawl-web verifies trawld in `auto` and `certManager` modes

Run 2026-09-28 UTC on `fractal`. It covers this acceptance criterion of issue #270:

> A real cluster reaches a working sign-in through the proxy in tls.mode=auto and
> in certManager mode with a self-signed CA issuer, from a fresh install, without
> the sidecar entering CrashLoopBackOff.

The earlier proof in `../2026-09-28-issue-270-kind/` ran at `76ccc826`. After
that commit, the chart gained the `init-tls-dir` init container, which creates
and seals trawld's certificate directory. trawld also gained hardening for its
generated key, and the chart lost `config.raw`. This run repeats the same
harness at the branch head.

Both legs passed. Each leg did a fresh Helm install into its own namespace of
one fresh kind cluster. In each leg, `trawl-web` and `trawld` stayed Ready with
`restartCount` 0 from first Ready to the end. An API-key sign-in through
`trawl-web` set a session cookie, and authenticated API routes answered 200
through the proxy. In the `auto` leg, `init-tls-dir` completed with exit code 0
and an empty log. It created `/var/lib/trawl/tls` as mode 0755, owned by
trawld's uid 1000, before either app container started. No product defect
showed up.

## What was installed

- Source: commit `898effaa326d1b71929d516618a93c592db862ed` on branch
  `trawl-issue-270-web-trawl-web-always-verifies-trawld`, exported with
  `git archive`. The chart and the image both come from that export.
- Image: `trawl-270-kind-final:898effaa326d`, OCI index digest
  `sha256:ae2fbc03ec47f9a98b062101e258773ec593f33917b345d2684cff991589990d`,
  label `org.opencontainers.image.revision=898effaa…`. On the kind node, all
  app containers in both legs report `imageID`
  `import-2026-09-28@sha256:a0d7355a7da7d524308111d3ad67dc4fd5e8d1fa55c6157760baeafdacbad0b3`,
  the index that `kind load` imported. `image.txt` maps these digests. It also
  lists the sha256 of each binary, both staged and inside the image.
- kind v0.33.0, node image
  `kindest/node:v1.36.4@sha256:099e049362a1526b2db71494e1947aae99bd16290d7c895f2b7ea312e3cbfaed`,
  Kubernetes v1.36.4, Helm v4.2.2, kubectl v1.36.4, Docker 29.7.2.
- cert-manager v1.21.1 from the release manifest, sha256
  `5f6a499b8c1857d57f560f536e0dcc830914b45c420899fe7ad0692c8624e408`. The
  version and checksum match the earlier proof.
- PostgreSQL `postgres:18-alpine@sha256:d3e1620b530c944afa6e887d22eb899824da68e19c52024bf98f5220c88a65b2`,
  one disposable instance per namespace (`postgres.yaml`, `mkdb.sh`).

## Changes to the harness

The scripts are copies of the earlier proof's scripts. The chart changes did
not require a change to any install input. The leg values never set
`config.raw`, and `init-tls-dir` needs no values. These are the changes:

- **Names.** The scratch directory is `~/trawl-270-kind-final-scratch`. The
  cluster is `trawl-270-final` (context `kind-trawl-270-final`), the image is
  `trawl-270-kind-final:898effaa326d`, and the builder image is
  `trawl-270-final-builder:scratch`. Both `values.yaml` files name the new
  image repository and tag. The earlier names were not reused, so this run
  cannot pick up anything the earlier run left behind.
- **File names.** `.gitignore` ignores `*.log`, so every transcript and
  container log is now `*.txt`.
- **Init containers.** `pod-spec.json` now includes `initContainers`. The
  transcript records `initContainerStatuses`. In the `auto` leg, the
  transcript also runs `kubectl logs trawl-0 -c init-tls-dir`, and
  `auto/init-tls-dir.txt` saves that output.
- **Certificate directory.** The `auto` leg adds a `stat` of
  `/var/lib/trawl/tls`, `tls/cert.pem`, `tls-key`, and `tls-key/key.pem` in the
  trawld container. It shows mode, owner, and file type.
- **Birth time.** `auto/tls-dir-birth.txt` was captured by hand after
  `run-leg.sh auto`, with the command in the file. It compares the birth time
  of `/var/lib/trawl/tls` with the start and finish of each container.

## Commands, in order

1. Scratch setup. Create `~/trawl-270-kind-final-scratch/private` (mode 0700).
   Copy `postgres.yaml` into the scratch directory, because `mkdb.sh` reads it
   from there. Put the checksum-named DuckDB archive into `runtime/` (see
   Caveats).
2. Warm-up. `build-image.sh`'s SPA, `compress-web`, and `cargo build` steps
   were first run by hand, with the same commands and directories. The
   `cargo build` ran in a detached container of the builder image, and
   `docker wait` waited for it. With the caches warm, the recorded run fits in
   one tool call.
3. `build-image.sh <worktree> 898effaa326d1b71929d516618a93c592db862ed`
   builds the image from a fresh export. Output: `build.txt`. The recorded run
   rebuilt `trawl-server` and `trawl-admin` and finished in 50 seconds.
4. `kind create cluster --name trawl-270-final …` and `kind load docker-image`.
   Output: `kind-create.txt`.
5. `mkdb.sh trawl-auto`, then `run-leg.sh auto trawl-auto`. Output:
   `auto/postgres.txt` and `auto/transcript.txt`. The `auto/tls-dir-birth.txt`
   capture followed.
6. `setup-cert-manager.sh` installs cert-manager, creates namespace `trawl-cm`
   with its PostgreSQL, and applies `cert-manager/issuers.yaml`. Output:
   `cert-manager/setup.txt`.
7. `run-leg.sh cert-manager trawl-cm`. Output: `cert-manager/transcript.txt`.
8. `kind delete cluster --name trawl-270-final`. Output: `cleanup.txt`.

`run-leg.sh` writes every command it runs into `transcript.txt`, followed by
the command's output. It installs with
`helm install trawl <export>/chart/trawl -f <leg>/values.yaml`. It then polls
the pod every 3 seconds until both app containers have been Ready for 60
seconds. It saves the StatefulSet pod spec (`pod-spec.json`), the rendered
`trawld.toml`, and the container logs. Next it inspects what each container
can see. Last, it creates a `trawl-reader` role and a human key with
`fleet-admin` inside the trawld container. It then signs in through
`kubectl port-forward svc/trawl 8090:8090`. The key goes to the private
scratch directory. Only `keys create`'s stdout goes to the token file. The
metadata on stderr goes to the transcript. The earlier proof explains why this
split matters.

## Leg 1: `tls.mode=auto`

Values: `auto/values.yaml`. The file sets no `tls` values, so the chart
default `auto` applies. `web.enabled` is true, `web.publicOrigins` is
`http://localhost:8090`, and persistence is on (1Gi, kind's `standard` class).

- **Render.** `[web] upstream_ca_path = "/var/lib/trawl/tls/cert.pem"` with no
  `upstream_url`, so trawl-web dials `https://127.0.0.1:5514`
  (`auto/trawld.toml`). In `auto/pod-spec.json`, the pod runs as uid 1000,
  gid 1000, and fsGroup 1000. trawl-web runs as `runAsUser: 1001` and mounts
  the `data` volume with `subPath: tls` at `/var/lib/trawl/tls`,
  `readOnly: true`. `init-tls-dir` mounts the whole `data` volume at
  `/var/lib/trawl`, with the same restricted security context as `init-auth`.
- **init-tls-dir.** `init-auth` ran from 21:13:48 to 21:13:54. `init-tls-dir`
  started and finished at 21:13:56, reason `Completed`, exit code 0, with no
  restart. Its log is empty (`auto/init-tls-dir.txt`, 0 bytes). The script
  writes only on the refusal paths, so an empty log means success. The two app
  containers started at 21:14:05.
- **Certificate directory.** `/var/lib/trawl/tls` was born at
  21:13:56.66. That time falls inside `init-tls-dir`'s run and is 9 seconds
  before the app containers started (`auto/tls-dir-birth.txt`). trawld created
  `tls-key/` and `cert.pem` at 21:14:42. In the trawld container:

  ```
  drwxr-xr-x 1 1000 1000 16 Sep 28 21:14 /var/lib/trawl/tls
  /var/lib/trawl/tls mode=755 uid=1000 gid=1000 type=directory
  /var/lib/trawl/tls/cert.pem mode=644 uid=1000 gid=1000 type=regular file
  /var/lib/trawl/tls-key mode=700 uid=1000 gid=1000 type=directory
  /var/lib/trawl/tls-key/key.pem mode=600 uid=1000 gid=1000 type=regular file
  ```

  In the earlier proof at `76ccc826`, kubelet created this directory as
  `0777 root:root`. It is now 0755 and owned by trawld's uid.
- **Pod status.** The pod stayed in `PodInitializing` for the two init
  containers. The 3-second poll first showed trawl-web Ready 36 seconds after
  the install and trawld Ready 72 seconds after it. Both kept `restartCount` 0
  through the 60-second hold and after the sign-in. One startup probe failed
  with "connection refused" before trawld listened. trawld needed 37 seconds
  for migration and boot.
- **Late certificate.** trawl-web started first and logged
  `upstream_ca_pending` at 21:14:05.66. trawld generated the certificate and
  listened at 21:14:42.27. trawl-web logged `upstream_ca_loaded` at 21:15:05.67
  on its 30-second re-read (`auto/trawl-web.txt`, `auto/trawld.txt`).
- **Key isolation.** trawl-web runs as `uid=1001 gid=1000`. It sees only
  `/var/lib/trawl/tls` holding `cert.pem`. `cat` of
  `/var/lib/trawl/tls-key/key.pem` and of `/var/lib/trawl/tls/key.pem` both
  fail with "No such file or directory". A write into the mount fails with
  "Read-only file system". Both containers read the same certificate
  fingerprint, `69:63:4C:…:CA:8C`. Its SANs are `localhost`, `127.0.0.1`, and
  `::1`.
- **Cookie secret under uid 1001.** `/etc/trawl/web.cookie` is mode 0440,
  owner 0, group 1000, and uid 1001 can read it.
- **Sign-in.** `/healthz` answers 200. `GET /` answers 200 `text/html`, which
  is the embedded SPA. Without a cookie, `/api/v1/whoami` answers 401.
  `POST /api/auth/login` answers 200 and sets
  `fleet_session=<redacted>; HttpOnly; SameSite=Lax; Path=/; Max-Age=86400`.
  With the cookie, `/api/auth/me`, `/api/v1/whoami`, and `/api/v1/schema`
  answer 200.

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
  The only init container is `init-auth`. The chart adds `init-tls-dir` only
  in `auto` mode with the sidecar.
- **Pod status.** The 3-second poll first showed trawl-web Ready 15 seconds
  after the install and trawld Ready 30 seconds after it. Both kept
  `restartCount` 0 through the hold and the sign-in. One startup probe failed
  before trawld listened.
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
  were built with plain `cargo build` inside `rust:1.98.0-bookworm`. The
  package set, `--no-default-features`, and rpath flags are the same as
  `build-distribution.sh --image-only`. The runtime image is the repository
  `Dockerfile` over a `distribution.py stage --image-only` context. trawld
  logs `git_sha="unknown"` because the source was a `git archive` export.
  The revision label and `distribution.json` record the SHA instead.
- **DuckDB download.** curl in this host's build environment fails
  certificate verification for the DuckDB archive. The checksum-named archive
  was therefore copied from the main checkout's `target/duckdb-runtime-cache`
  to two places. The first is `runtime/`, for `distribution.py prepare`. The
  second is `host-target/duckdb-runtime-cache/…/<sha256>/`, because the build
  script behind `cargo xtask compress-web` prepares the same runtime. Both
  paths check the sha256 before extraction.
- **Storage.** kind's `standard` class is the local-path provisioner. Its
  volumes are hostPath directories with mode 0777 and owner root, and kubelet
  does not apply `fsGroup` to them. On this storage, `init-tls-dir` took the
  create path: no `tls` directory existed, so it made one as uid 1000. Its
  other branches did not run: the foreign-owner refusal, the removal of a
  legacy `key.pem` or a foreign `cert.pem`, and the removal of setgid and
  group write that an fsGroup walk leaves. A fresh install cannot reach those
  branches. The `test_tls_dir_script_*` tests in `chart/trawl/tests/tls.py`
  run the rendered script against each of those cases.
- **Not exercised here.** The run had no request during the pending window,
  so the 503 `upstream certificate not available` answer does not appear. It
  had no certificate rotation and no cert-manager renewal either. The
  `trawl-web` tests `late_ca_is_loaded` and `rotated_ca_is_reloaded` cover
  those paths.
- **Observation outside #270.** In `certManager` mode, trawld's own
  `/etc/trawl/tls/tls.key` is mode 0644 (group 1000), the Secret volume
  default. Only the trawld container mounts it. The sidecar does not. This
  matches the earlier proof.
- **Transcript gaps.** `setup-cert-manager.sh` sends `kubectl apply -f
  cert-manager.yaml` to `/dev/null`, so `setup.txt` does not show that command
  or its output. The `wait` for the three deployments that follows it does
  appear. Container logs are saved with ANSI colour codes removed.

## Secrets and cleanup

This directory holds no database password, API key, session cookie, cookie
key, or private key. A scan checked every generated password, token, and
cookie value against these files and found none. It also found no
`PRIVATE KEY` block. Key prefixes do appear, and they are not secret.
`cleanup.txt` shows the cluster deleted and no `trawl-270` container left.
After the commit, the scratch directory and the `trawl-270-kind-final` and
`trawl-270-final-builder` images were removed. `SHA256SUMS` covers every
other file here.
