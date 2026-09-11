---
title: Manage access and browser sessions
description: Provision roles, rotate API keys, and configure TLS and shared sessions.
---

Trawl permissions are code-defined; roles are named bundles stored in the Fleet
keystore. Key permissions are the union of its roles. A role called `trawl-admin`
does not gain implicit privileges. Use the [permission reference](/reference/api/#roles-and-permissions)
for the available bundles and the [deployment guide](/operate/deployment/#provision-the-databases)
for database preparation.

## Create roles and keys

Run `fleet-admin` where it can reach the selected Fleet database. Supply
`DATABASE_URL` through a protected credential mechanism. It is not the daemon's
`FLEET_DATABASE_URL` override. Confirm the database identity before mutation.
A fresh keystore has no roles; do not rerun role creation on a converted deployment.

```bash
fleet-admin roles list
fleet-admin roles create --name trawl-reader \
  --perm trawl:query --perm trawl:schema_read --perm trawl:query_cancel
fleet-admin roles create --name trawl-ingest --perm trawl:ingest
fleet-admin roles create --name trawl-schema-admin \
  --perm trawl:schema_read --perm trawl:schema_write
fleet-admin keys create --name operator --kind human --role trawl-reader
fleet-admin keys create --name vector --kind service --role trawl-ingest
```

Each key creation prints its token once. Store it in the intended secret manager
or owner-only client/collector configuration immediately; do not paste it into a
report or repository. Unknown permission strings warn but still persist, so verify
the exact spelling and use authenticated `/api/v1/whoami` to inspect effective
permissions. Repin requires `schema_write` on an ingest-enabled server; it does
not require a human-kind key.

To grant an existing operator schema access, identify its stable prefix first:

```bash
fleet-admin keys list
fleet-admin keys assign-role PREFIX trawl-schema-admin
```

Replace `PREFIX` with that selected key prefix. Read role and permission state
afterward. `roles add-perm`, `remove-perm`, and `set-rate` affect all keys holding
the role, while `keys assign-role` and `unassign-role` affect one key.

## Rotate or revoke an API key

1. Identify the old key by prefix, its roles, expiry, and every consumer. Inspect
   schedules owned by the key before revoking it; replacing a token does not
   transfer those schedules to another principal.
2. Create a new key with the intended role set, store it securely, and update the
   selected client or collector through its normal secret workflow.
3. Verify `/whoami` and the actual operation with the new key. For collectors,
   verify accepted events, not just a successful process restart.
4. Revoke the old prefix and verify that the old key is rejected. This affects all
   applications that trusted that Fleet key, not only Trawl.

```bash
fleet-admin keys revoke PREFIX
```

The command confirms interactively; use `--yes` only for an already approved
noninteractive revocation. Revocation is checked on new requests. Existing browser
cookies still depend on the underlying key's validity; they do not override it.

## TLS and browser access

trawld serves HTTPS. Configure a trusted certificate/key pair and client trust for
normal use. Generated self-signed certificates cover localhost and loopback IPs;
`--insecure` disables verification and should be confined to a deliberate local
check. Certificate files can reload at `tls_reload_interval_secs`; ordinary config
changes require restarting the affected process.

The browser connects to `trawl-web`, usually through HTTPS at a reverse proxy.
Configure its exact browser-visible origin, including scheme and non-default port:

```toml
[web]
public_origins = ["https://trawl.example.com"]
cookie_secret_path = "/var/lib/trawl/web.cookie"
allow_insecure_cookies = false
```

A present Origin must match. `Forwarded` and `X-Forwarded-*` do not widen the list.
Keep secure cookies when the browser sees HTTPS, even if the proxy forwards HTTP.
Only use `allow_insecure_cookies=true` when the browser itself intentionally uses
HTTP. See [origin validation](/reference/configuration/#the-browser-origin-allowlist).

## Shared browser sessions

Standalone installations need a persistent app-local key. Shared Fleet SSO is
optional and needs the same raw 32-byte session key and parent cookie domain in
every participating app. For example, `trawl.fleet.example.com` and
`coastwatch.fleet.example.com` can use `.fleet.example.com`. Each app still has its
own public-origin allowlist. Do not assume the other repository's live config.

Generate a new key once, in a protected administrative shell with tracing off:

```bash
(
  set -euo pipefail
  umask 077
  test ! -e "$TRAWL_SESSION_KEY_B64"
  fleet-admin generate-session-key > "$TRAWL_SESSION_KEY_B64"
)
```

Set `TRAWL_SESSION_KEY_B64` to a new file in a private directory first. This is
base64url text, not the raw bytes expected by `cookie_secret_path`. Store it in the
fleet's secret manager. For Debian, run this conversion as root using that
protected file; it stages outside the service-writable state directory:

```bash
(
  set -euo pipefail
  umask 077
  tmp="$(mktemp -p /root fleet-session-key.XXXXXX)"
  trap 'rm -f "$tmp"' EXIT
  { tr -d '\n' < "$TRAWL_SESSION_KEY_B64"; printf '='; } | basenc --base64url -d > "$tmp"
  test "$(stat -c %s "$tmp")" -eq 32
  install -o trawl -g trawl -m 0640 "$tmp" /var/lib/trawl/web.cookie
)
```

`install` replaces the destination instead of redirecting through a planted
symlink. The group-readable mode lets the separate `trawl-web` user read the key.
Do not replace it with a redirect followed by chown/chmod. Keep the temporary file
cleanup in a subshell so the trap does not remain armed in your interactive shell.

For Kubernetes, decode into a private raw file, verify 32 bytes, and use the
selected context and namespace to create a Secret with key `cookie.key` from that
file. Reference its name through `web.cookieSecret.existingSecret` in the complete
[deployment values](/operate/deployment/#kubernetes-and-helm). Do not paste the
base64url text into Kubernetes' standard-base64 `data` value.

Set `[web] shared_domain` or Helm `web.sharedDomain` consistently across the apps.
Restart affected proxies in a coordinated window and verify login across both
origins. Swapping the key invalidates existing sessions; there is no old/new key
ring. The API tokens themselves remain valid. Remove temporary exported key files
once secure distribution is verified.

A shared cookie makes participating apps part of the same session trust boundary.
An origin allowlist rejects cross-origin requests, but cannot prevent a compromised
sibling app from overwriting or clearing a parent-domain cookie in its own response.
trawld 401 clears the session through `/api/auth/me`; a 403 keeps it so the same
session can remain useful in another app.
