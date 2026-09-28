# Issue #270: a clean `bin/dev` signs in with a pinned development certificate

Run 2026-09-28 UTC on `fractal`. It covers this acceptance criterion of issue #270:

> The development stack signs in without the switch. From a clean dev state
> with no dev certificate, bin/dev reaches a successful sign-in through the dev
> trawl-web.

It passed. `bin/dev` started with no `~/.trawl/tls/` and no `~/.trawl/tls-key/`.
`trawl-web` started before the certificate existed, logged
`upstream_ca_pending`, and answered one sign-in attempt with 503
`upstream certificate not available`. trawld then generated its certificate.
The next attempt made `trawl-web` read the pinned file, log
`upstream_ca_loaded`, and sign in with 200. The sign-in through the browser
origin `http://localhost:8081` set a session cookie. `/api/auth/me`,
`/api/v1/whoami`, and a proxied `/api/v1/query` all answered 200. No
insecure switch was set anywhere: `fleet-dev.toml` gives `trawl-web` only
`TRAWL_WEB_UPSTREAM_CA_PATH`.

## What ran

- Source: worktree HEAD `a92adb858fb2e26c9df902bc93d6341ac4c5d0e0` on branch
  `feat/issue-270-web-trawl-web-always-verifies-trawld`, with no tracked
  changes (`00-environment.txt`). `bin/dev` built `trawl-server` and
  `trawl-web` from it through the `[preparation]` step.
- rustc 1.98.0, mprocs 0.9.6, trunk 0.21.14, Docker 29.7.2, curl 8.22.0,
  Linux 7.2.5-3-omarchy.
- Plan (`01-fleet-dev-plan.txt`): exposure Localhost, database Docker, state
  scope `docker`, 1Password not required, origin `http://localhost:8081`,
  backend `localhost:8090`, database `trawl_dev`. No machine profile exists
  (`~/.config/fleet/` is absent), so the committed defaults applied.
- trawl-web pin (`02-fleet-dev-toml-trawl-web.txt`):
  `TRAWL_WEB_UPSTREAM_CA_PATH = "~/.trawl/tls/cert.pem"`.
- trawld used the developer's existing `~/.trawl/trawld.toml` unchanged:
  `[server] http_addr = "127.0.0.1:5514"`, `[data] path = "~/.trawl/data"`,
  internal telemetry on. The persistent `fleet-dev-postgres-data` volume and
  `~/.trawl/data` were reused, not reset.

## Commands, in order

1. Confirm the stack is down and move the old certificate aside. The previous
   pair used the older layout, with `key.pem` beside `cert.pem`.
   `mv ~/.trawl/tls ~/.trawl/tls.bak-270`. `~/.trawl/tls-key` did not exist.
   Output: `03-clean-state.txt`.
2. Start `bin/dev` from the worktree in a private tmux server:
   `tmux -L j270c9b new-session -d -s dev -x 250 -y 140 -c <worktree> bin/dev`,
   at 18:55:20Z.
3. Poll `POST http://127.0.0.1:8090/api/auth/login` every 0.5 s with the dev
   API key until it answers 200: `scripts/poll-login.sh 540`. Output:
   `11-login-poll.txt`.
4. Capture the mprocs panes: `scripts/cap.sh` after selecting each process
   with `j`. Output: `10-trawl-web-pane.txt`, `12-trawld-pane.txt`,
   `13-web-ui-pane.txt`, `14-login-pane.txt`.
5. Sign in through the browser origin and make authenticated requests:
   `scripts/signin.sh`. Output: `21-signin-transcript.txt`.
6. Record the generated files and the certificate trawld serves. Output:
   `20-generated-cert-files.txt`.
7. Quit mprocs with `q`, save the console, and confirm nothing is left.
   Output: `15-bin-dev-console.txt`, `30-stopped.txt`.

## Results

### trawl-web waits for the certificate, then loads it

From `10-trawl-web-pane.txt`:

```text
18:56:03.546602Z  WARN ... event_type="upstream_ca_pending" path=/home/jakub/.trawl/tls/cert.pem
18:56:03.546634Z  INFO trawl_web: loaded config config=/home/jakub/.trawl/trawld.toml bind_addr=127.0.0.1:8090 upstream=https://127.0.0.1:5514 ...
18:56:03.546969Z  INFO trawl_web: trawl-web listening addr=127.0.0.1:8090
18:56:04.209913Z  INFO ... event_type="upstream_ca_loaded" path=/home/jakub/.trawl/tls/cert.pem
```

From `11-login-poll.txt`:

```text
18:55:32.937 POST /api/auth/login -> 000
18:56:03.699 POST /api/auth/login -> 503 {"error":"upstream certificate not available"}
18:56:04.202 cert.pem now exists: /home/jakub/.trawl/tls/cert.pem mode=644 birth=2026-09-28 11:56:04.081332315 -0700
18:56:04.240 POST /api/auth/login -> 200 {"name":"fleet-dev-jakub","roles":["fleet-developer"],"permissions":[...]}
```

`000` means nothing listened on :8090 yet, while fleet-dev built and migrated.
The 503 came before `cert.pem` existed. The first attempt after it appeared
loaded the pin and succeeded. That attempt came 0.13 s after the file's birth
time and well before the 30 s re-read interval.

### trawld generated the certificate on its first start

Under mprocs, trawld has a TTY, so it runs its monitor dashboard and writes
no log lines to stdout (`12-trawld-pane.txt`). Its own events reach Trawl
through internal telemetry. The query in `21-signin-transcript.txt`, run
through `trawl-web`, returns them:

```text
18:56:03.556988  no TLS certificate configured — using auto-generated self-signed certificate
18:56:03.800042  no TLS certificate found, generating self-signed certificate
18:56:04.093617  self-signed TLS certificate generated  cert=/home/jakub/.trawl/tls/cert.pem  key=/home/jakub/.trawl/tls-key/key.pem
18:56:04.094002  TLS certificate details
```

`20-generated-cert-files.txt` shows the layout:

| Path | Mode | Owner uid |
| --- | --- | --- |
| `~/.trawl/tls/` | `0755` | 1000 |
| `~/.trawl/tls/cert.pem` | `0644` | 1000 |
| `~/.trawl/tls-key/` | `0700` | 1000 |
| `~/.trawl/tls-key/key.pem` | `0600` | 1000 |

The SHA-256 fingerprint of `cert.pem` equals the fingerprint of the
certificate trawld serves on `127.0.0.1:5514`
(`33:0C:E7:D7:…:CF:A7:19`). The SANs are `localhost`, `127.0.0.1`, and `::1`.

### Sign-in through the browser origin

`21-signin-transcript.txt`, all through `http://localhost:8081`, where trunk
proxies `/api/` to `trawl-web`:

| Request | Status |
| --- | --- |
| `GET /login` | 200 `text/html` |
| `GET /api/auth/me`, no cookie | 401 `{"error":"unauthorized"}` |
| `POST /api/auth/login`, `Origin: http://localhost:8081` | 200, `set-cookie: fleet_session=<redacted>; HttpOnly; SameSite=Lax; Path=/; Max-Age=86400` |
| `GET /api/auth/me` | 200, `fleet-dev-jakub`, role `fleet-developer` |
| `GET /api/v1/whoami` (proxied to trawld) | 200, kind `human` |
| `POST /api/v1/query` (proxied to trawld) | 200, 4 rows |

The login handler validates the key by calling trawld's `/api/v1/whoami`
through the pinned client, so each 200 above crossed a verified TLS
connection to trawld. The cookie has no `Secure` flag because the
localhost exposure clears it. `trawl-web` logs that as
`session_cookie_secure_downgraded`.

### Shutdown

`q` in mprocs ended `bin/dev` with exit status 0 at 18:58:52Z.
fleet-dev stopped `fleet-dev-postgres`, which is `Exited (0)`, and the
`fleet-dev-postgres-data` volume remains, as designed. No `mprocs`,
`trawld`, `trawl-web`, `fleet-dev`, or `trunk` process remains, and nothing
listens on 8081, 8090, 5514, or 5435 (`30-stopped.txt`). The private tmux
server was killed afterward.

## State left behind

- `~/.trawl/tls.bak-270/` holds the previous pair (`cert.pem`, `key.pem`,
  2026-09-14), moved, not deleted.
- `~/.trawl/tls/cert.pem` and `~/.trawl/tls-key/key.pem` are the pair this run
  generated. The next `bin/dev` reuses them.
- `~/.trawl/trawld.toml`, `~/.trawl/data`, the database volume, and the
  fleet-dev state under `~/.local/state/fleet/docker/` were not modified by
  hand. The run wrote to them only through normal operation: trawld ingested
  its own telemetry into `~/.trawl/data`.

## Redaction

The fleet-dev API key never reaches a file here. `jq` reads it from
`~/.local/state/fleet/docker/dev-api-key` straight into each request body,
and pane captures replace any `flt_…` token with `flt_<redacted>`. The
`fleet_session` cookie value and the key lookup prefix in the
`/api/v1/whoami` body are replaced with `<redacted>`. A grep of this
directory for the key, its prefix, the session AEAD key, and any unredacted
cookie value finds none.

## Caveats

- One run, on one host, with the developer's existing data directory and
  database. The run proves the certificate path. It does not prove a
  first-ever `bin/dev` on an empty database.
- The 503 appeared because the poll happened to hit the window between
  `trawl-web` listening (18:56:03.547) and `cert.pem` appearing
  (18:56:04.081). A slower poll would show only the 200.
- trawld's certificate lines come from its internal telemetry, queried
  through the product, not from stdout. Their `_time` values are trawld's
  own event times.
- Pane captures show only what mprocs had on screen. The trawl-web pane held
  its whole log. The trunk pane (`13-web-ui-pane.txt`) begins partway through
  the Wasm build.

## Files

`SHA256SUMS` lists every file in this directory except itself. Check it with
`sha256sum -c SHA256SUMS` from this directory.
