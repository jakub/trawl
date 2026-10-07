---
title: Manage access
description: Create roles and API keys, rotate or revoke a key, and configure TLS, the browser origin, and shared browser sessions.
---

Permissions are fixed in code. Roles are named sets of permissions stored in
the Fleet database, and a key's permissions are the union of its roles. A role
name grants nothing by itself. The [permission list](/reference/api/#roles-and-permissions)
names every permission Trawl checks.

Every `fleet-admin` command except `generate-session-key` reads the Fleet DSN
from `DATABASE_URL`, which is separate from the daemon's `FLEET_DATABASE_URL`.

## Create roles and keys

On a Helm install, run each `fleet-admin` command inside the trawld container,
which holds `DATABASE_URL`. Put `kubectl exec` in front of it. For a release
named `trawl`:

```bash
kubectl -n NAMESPACE exec -i trawl-0 -c trawld -- fleet-admin roles list
(umask 077
 kubectl -n NAMESPACE exec -i trawl-0 -c trawld -- \
   fleet-admin keys create --name alice --kind human --role trawl-reader > alice.token)
```

The redirect runs on your machine, so the token file is written there. Do not
add `-t` to `keys create`, because a terminal mixes standard error into the
token file. A command that asks `[y/N]` needs `-it`, or `--yes`.

1. List the existing roles. A new Fleet database has none:

   ```bash
   fleet-admin roles list
   ```

2. Create one role per job. Each `--perm` is `trawl:` followed by a permission
   name:

   ```bash
   fleet-admin roles create --name trawl-reader \
     --perm trawl:query --perm trawl:schema_read --perm trawl:validate \
     --perm trawl:export --perm trawl:stream --perm trawl:saved_query \
     --perm trawl:query_cancel
   fleet-admin roles create --name trawl-ingest --perm trawl:ingest
   fleet-admin roles create --name trawl-schema-admin \
     --perm trawl:schema_read --perm trawl:schema_write
   ```

   `fleet-admin` warns about a permission it does not recognize but stores it
   anyway. Check the spelling in the warning.

3. Create one key per person or collector. A `human` key is for a person and a
   `service` key is for software:

   ```bash
   (umask 077
    fleet-admin keys create --name alice --kind human --role trawl-reader > alice.token
    fleet-admin keys create --name vector --kind service --role trawl-ingest \
      --expires 90d > vector.token)
   ```

   `umask 077` makes each new token file readable by you alone. A redirect
   into a file that already exists keeps that file's mode, so delete a stale
   token file before you reuse its name. The token goes to standard output once. The name, kind, roles, 8-character
   prefix, and expiry go to standard error. `--role` repeats. `--expires`
   accepts `24h`, `90d`, or `52w`, and a key without it never expires.

4. Check what a key can do:

   ```bash
   curl --fail-with-body -H "Authorization: Bearer $(cat alice.token)" \
     https://trawl.example.com:5514/api/v1/whoami
   ```

   The response lists the key's `roles` and its resolved `permissions`.

To change roles and keys later, use these commands. `PREFIX` is the prefix that
`fleet-admin keys list` prints.

| Task | Command |
| --- | --- |
| Add permissions to a role, for every key that holds it | `fleet-admin roles add-perm ROLE trawl:export trawl:stream` |
| Remove permissions from a role | `fleet-admin roles remove-perm ROLE trawl:export` |
| Show a role and how many keys hold it | `fleet-admin roles show ROLE` |
| Cap requests per minute for keys that hold a role, or clear the cap | `fleet-admin roles set-rate ROLE --rate-rpm 600`, `fleet-admin roles set-rate ROLE --default` |
| Delete a role. `--force` takes it from every key first | `fleet-admin roles delete ROLE` |
| Give one key a role, or take one away | `fleet-admin keys assign-role PREFIX ROLE`, `fleet-admin keys unassign-role PREFIX ROLE` |
| Change a key between `human` and `service` | `fleet-admin keys retype PREFIX service` |
| List keys, including revoked ones | `fleet-admin keys list --all` |

`keys revoke`, `keys unassign-role`, and `roles delete` ask `[y/N]` on a
terminal. Without a terminal they refuse unless you pass `--yes`.

## Rotate or revoke an API key

1. Find the key and its roles with `fleet-admin keys list`. Saved queries,
   schedules, and query history belong to the key's id, and a replacement key
   does not inherit them.

2. Create the replacement with the same roles, as in
   [Create roles and keys](#create-roles-and-keys).

3. Install the new token in the client or collector. Confirm it with
   `/api/v1/whoami` and with the real operation. For a collector, confirm that
   events are accepted, not only that the process restarted.

4. Revoke the old key:

   ```bash
   fleet-admin keys revoke PREFIX
   ```

   The command prints the key and asks `revoke this key? [y/N]`. trawld checks
   every request against the database, so the next request with the old token
   gets a 401, and a browser session logged in with that key ends. Revocation
   applies in every Fleet application that trusts the key.

## Configure TLS

trawld serves HTTPS only. When `tls_cert_path` and `tls_key_path` are unset, it
generates a self-signed certificate at startup, valid for `localhost`,
`127.0.0.1`, and `::1`. It writes the certificate to `tls/cert.pem` beside the
data path, and the private key to `tls-key/key.pem`, in a directory that only
trawld's user can open. Clients on other hosts need a certificate they trust:

```toml
[server]
tls_cert_path = "/etc/trawl/cert.pem"
tls_key_path = "/etc/trawl/key.pem"
tls_reload_interval_secs = 300
```

Set both paths or neither. Make the key readable by the daemon, then restart:

```bash
sudo chown root:trawl /etc/trawl/key.pem && sudo chmod 0640 /etc/trawl/key.pem
sudo systemctl restart trawld
```

trawld re-reads both files every `tls_reload_interval_secs` seconds, so a
renewed certificate needs no restart. `trawl --insecure` skips certificate
verification. Use it only for a check on the same host.

### Choose how trawl-web trusts trawld

`trawl-web` always verifies trawld's certificate. It trusts one of two sets of
CAs:

- The platform trust store, when `[web] upstream_ca_path` is unset.
- Only the CA certificates in the PEM file that `upstream_ca_path` names.

In both modes the certificate must name the host in `upstream_url`. By default
that host is `127.0.0.1`, derived from `[server] http_addr`. `trawl-web`
refuses to start when `upstream_url` is not `https`, or when it holds a user
name or password. It never follows a redirect from trawld, and it ignores
proxy variables such as `HTTPS_PROXY`, so it always connects to trawld
directly.

The Debian package ships this setting, which pins the certificate that trawld
generates:

```toml
[web]
upstream_ca_path = "/var/lib/trawl/tls/cert.pem"
```

If you set `tls_cert_path`, change `upstream_ca_path` to the CA that issued
your certificate. That certificate must also name the host in `upstream_url`.
Restart `trawl-web` after you change `trawld.toml`.

`trawl-web` starts even when the pinned file does not exist yet, because
trawld writes its generated certificate only on its first start. Until the
file exists, `/healthz` answers `ok`, and every request that needs trawld gets
a 503 response with `{"error":"upstream certificate not available"}`. The
first request after the file appears reads it and succeeds.

`trawl-web` also reads the file again every 30 seconds and compares its
contents. A CA that you replace takes effect within 30 seconds, with no
restart. A file that does not parse at startup stops `trawl-web`. If a later
change does not parse, or the file disappears, `trawl-web` keeps the last CA
that loaded and logs one `upstream_ca_refused` warning for the change.

### Verify a certificate name over another address

Set `upstream_connect_addr` when trawld's certificate names a DNS host that
does not resolve to trawld where `trawl-web` runs. `trawl-web` connects to the
address in `upstream_connect_addr` and still verifies the name in
`upstream_url`. For a certificate issued for `api.example.com`, with trawld on
the same host as `trawl-web`:

```toml
[web]
upstream_url = "https://api.example.com:5514"
upstream_connect_addr = "127.0.0.1:5514"
upstream_ca_path = "/etc/trawl/ca.pem"
```

- The value is an IP address and a port. Write an IPv6 address in brackets,
  as in `[::1]:5514`. A host name is refused.
- The host in `upstream_url` must be a DNS name, not an IP address.
- The two ports must be equal. A URL with no port counts as port 443.

The Helm chart sets `upstream_url` and `upstream_connect_addr` in `tls.mode`
`secret` and `certManager`. See
[Configure the daemon API certificate](/operate/deployment/#configure-the-daemon-api-certificate).

## Set the browser origin

`trawl-web` accepts a cookie-authenticated request only when its `Origin`
header is in `[web] public_origins`. Write the origin the browser shows, scheme
and non-default port included:

```toml
[web]
public_origins = ["https://trawl.example.com"]
bind_addr = "127.0.0.1:8090"
cookie_secret_path = "/var/lib/trawl/web.cookie"
allow_insecure_cookies = false
```

- trawl-web compares the whole header, so `http://localhost:8090` and
  `http://127.0.0.1:8090` are two entries. `Forwarded` and `X-Forwarded-*`
  headers never extend the list, and an empty list stops trawl-web at startup.
- Behind a TLS-terminating reverse proxy, list the `https://` origin, not the
  `http://127.0.0.1:8090` address the proxy forwards to, and keep
  `allow_insecure_cookies = false`. Set it to `true` only when the browser
  itself uses HTTP.
- `bind_addr` defaults to loopback. External browsers need a reverse proxy in
  front of it.

Restart the proxy after a change with `sudo systemctl restart trawl-web`. The
[`[web]` reference](/reference/configuration/#web) lists every key, including
`session_ttl_secs`, `upstream_url`, and `cookie_secret_env`.

## Share a browser session across Fleet applications

On its own, Trawl reads the key at `cookie_secret_path` and scopes the cookie
to its origin. To let one login work in Trawl and another Fleet application,
every application needs the same 32-byte session key and the same
`shared_domain`.

1. Generate the key once and keep the file private:

   ```bash
   umask 077
   fleet-admin generate-session-key > fleet-session.b64
   ```

   The output is one line of 43 base64url characters. Store it in your secret
   manager and give the same value to every application.

2. On a Debian host, decode it to the raw 32 bytes that `cookie_secret_path`
   reads, then install it over the package-generated key:

   ```bash
   { tr -d '\n' < fleet-session.b64; printf '='; } | basenc --base64url -d > web.cookie.new
   test "$(stat -c %s web.cookie.new)" -eq 32
   sudo install -o trawl -g trawl -m 0640 web.cookie.new /var/lib/trawl/web.cookie
   ```

   `install` replaces the file in place, and the `trawl` group lets the
   `trawl-web` user read it. `cookie_secret_env` names an environment variable
   that holds the base64 text instead and takes precedence. Protect the file
   that sets it, because `/etc/default/trawl-web` is world-readable.

3. On Kubernetes, create a Secret from the raw bytes and name it in the values
   file:

   ```bash
   kubectl -n trawl create secret generic trawl-session --from-file=cookie.key=web.cookie.new
   ```

   ```yaml
   web:
     sharedDomain: .fleet.example.com
     cookieSecret:
       existingSecret: trawl-session
   ```

4. Set the same parent domain in every application. Each application keeps
   its own `public_origins`. For Trawl on Debian:

   ```toml
   [web]
   shared_domain = ".fleet.example.com"
   ```

5. Restart every proxy, then log in at one application and open the other.
   Replacing the key ends every existing browser session. API tokens are
   unaffected.

A shared cookie makes the applications one session trust boundary. A
compromised application can overwrite or clear the cookie for all of them, and
`public_origins` does not prevent that.
