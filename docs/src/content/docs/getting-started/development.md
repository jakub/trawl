---
title: Local development
description: Run Trawl locally with the Fleet development controller.
---

`fleet-dev` is the single owner of Trawl's interactive development stack. It
prepares Fleet auth and Postgres, reconciles a persistent development identity,
generates private per-process configuration, and runs Trawl under an attached
`mprocs` supervisor.

## Portable localhost setup

With no profile, the controller uses Docker and localhost:

```bash
bin/fleet-dev doctor trawl
bin/dev
```

The first launch starts `pgvector/pgvector:pg18` on
`127.0.0.1:5435`, migrates Fleet auth, and creates the `fleet-developer`
role and API key. It then starts `trawld`, `trawl-web`, and Trunk. Open
`http://localhost:8081/login` and paste the key displayed in the login pane.

The interactive database lives in the named
`fleet-dev-postgres-data` volume. It is intentionally separate from
`docker-compose.dev.yml`, which is disposable test infrastructure. If
`fleet-dev` starts the database container, it stops the container when the
attached session exits; the volume persists.

Useful commands:

```bash
bin/fleet-dev plan trawl --format human
bin/fleet-dev plan trawl --format json
bin/fleet-dev doctor trawl
bin/fleet-dev setup trawl
bin/dev --release-spa
```

`plan` is pure: it does not read token files, resolve credentials, connect to a
database, or change Tailscale configuration.

## Machine profile

Invocation overrides take precedence over
`~/.config/fleet/dev.toml`; absent values use committed conventions.
A CNPG/Tailscale workstation profile can remain small:

```toml
exposure = "tailscale"
database = "cnpg"

[op]
token_file = "~/.config/op/homelab-workstation-token"

[paths]
coastwatch = "~/code/coastwatch"

[cnpg]
host = "10.0.100.89"
port = 5432
state_scope = "cnpg"
credential_ref_template = "op://Homelab/CNPG {database}/{field}"
session_aead_key_ref = "op://Homelab/Fleet session key/credential"
```

The token must be a regular, non-symlink file owned by the current user with
mode `0600`. Use a least-privilege 1Password service account that can read only
the required development items. The controller reads it only when the selected
database or app resolver needs 1Password. The service token is passed only to
`op` validation and declared credential resolver processes; migrations,
preparation commands, `mprocs`, and application processes never inherit it.
CNPG connection strings explicitly require TLS, so the client cannot silently
downgrade to plaintext. The current development contract encrypts transport;
deployments that require server identity verification should provide that at
the network/database boundary until the profile grows an explicit CA contract.
CNPG preparation is read-only: it verifies the selected database names,
`public` schema migration rights, and Coastwatch's pre-provisioned `vector` and
`pg_trgm` extensions, but never creates databases, roles, grants, or
extensions.

Override a profile for one invocation without editing it:

```bash
bin/fleet-dev --database docker --exposure localhost dev trawl \
  --app-root "$(pwd)"
```

## Tailscale exposure

Tailscale mode uses HTTPS termination and a host-only, Secure
`fleet_session` cookie:

```bash
# Persistent setup. Missing mappings are created; conflicts are refused.
bin/fleet-dev setup trawl --exposure tailscale

# Ordinary launches verify but never modify persistent Serve state.
bin/dev --tailscale
```

The conventional Trawl mapping is
`:8444 -> http://127.0.0.1:8081`. If that public port already points
somewhere else, inspect the old and proposed targets in the error before using
the explicit replacement path:

```bash
bin/fleet-dev setup trawl --exposure tailscale --force
```

The generated Trunk backend authority uses the same MagicDNS hostname as the
browser origin. This preserves Fleet's same-host Origin/Host CSRF check; the
controller does not weaken it for development.

All apps exposed on that one development hostname form a single browser trust
boundary: cookies and the same-host Origin check are host-scoped, not
port-scoped. Do not expose an untrusted sibling frontend on another port.

Matching those hostnames also means the backend binds the node's tailnet
address, not loopback. `https://<node>:8444` is the front door, but
`<node>:8090` is reachable directly by every tailnet peer without passing
through Serve. Authentication still applies; use tailnet ACLs if that matters.
The controller prints this on every Tailscale launch.

Serve mappings are persistent and are **not** removed when the stack exits:
`:8444` keeps pointing at `127.0.0.1:8081` afterwards, so anything that later
binds 8081 is published tailnet-wide. Remove a mapping explicitly when you are
done with it:

```bash
tailscale serve --https 8444 off
```

## Persistent state and recovery

Provider-scoped controller state lives below
`$XDG_STATE_HOME/fleet/<scope>/`, or
`~/.local/state/fleet/<scope>/` when `XDG_STATE_HOME` is unset:

- `provider.json` binds the scope to the normalized provider, host, port, and
  `fleet_dev` database;
- `dev-api-key` is the persistent development API key;
- `session-aead-key` is the persistent Docker session key.

Directories are `0700` and secret files are `0600`. CNPG session material is
resolved per launch and is not cached. A scope identity mismatch fails rather
than silently reusing a key from another Fleet database.

Recovery is explicit:

```bash
# Reset only the persistent Docker database. This destroys local dev data.
docker compose --project-name fleet-dev \
  --file fleet-dev.compose.yml down --volumes

# Replace a missing or revoked development key on the next launch:
rm ~/.local/state/fleet/docker/dev-api-key
```

The command above is for the default state location. If `XDG_STATE_HOME` is
set, remove the same exact file beneath that directory instead:
`"$XDG_STATE_HOME/fleet/docker/dev-api-key"`. Remove controller state only
after confirming the provider scope; the API key and Docker session key cannot
be recovered from those files after deletion. Never use the Docker credentials
or its superuser contract outside the loopback-only development provider.

## Coastwatch

The controller already supports a Coastwatch manifest and a combined plan.
Cross-repository adoption is tracked separately. Trawl remains independently
launchable, and `bin/dev --with-coastwatch` is a frozen compatibility
translation to `fleet-dev all` during that rollout.

The two-app schema uses fixed conventions rather than a plugin API. A
browser-facing backend named `web` receives the Coastwatch bind alias and the
common Fleet topology/cookie environment; `web-ui` receives a private generated
copy of its committed `Trunk.toml`. Generation changes only absolute
target/dist paths, serve addresses/port, and proxy backend, preserving settings
such as CSP nonces and `no_redirect`. Both apps' browser-facing process must
map `FLEET_SESSION_AEAD_KEY = "fleet.session_aead_key"`; manifest validation
rejects an app that could drift away from the shared development identity.
