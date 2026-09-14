---
title: Local development
description: Run Trawl locally with the Fleet development controller.
---

`fleet-dev` owns Trawl's interactive development stack. It prepares Fleet auth
and PostgreSQL, reconciles a persistent development identity, generates private
per-process configuration, and runs Trawl under an attached `mprocs` supervisor.

## Start the localhost stack

With no profile, the controller uses Docker and localhost:

```bash
bin/fleet-dev doctor trawl
bin/dev
```

The first launch starts `pgvector/pgvector:pg18` on `127.0.0.1:5435`, migrates
Fleet auth, and creates the `fleet-developer` role and an API key. It then runs
`trawld`, `trawl-web`, and `trunk serve`. The `trawl-login` pane prints the key.
Open `http://localhost:8081/login` and paste it.

Ordinary Cargo builds and `bin/dev` link the downloaded DuckDB shared library,
which includes ICU, JSON, and Parquet. The download is cached under
`target/duckdb-download` and reused. Cargo supplies the development loader path
for `cargo run` and tests; `bin/trawld-dev` supplies it when Fleet launches the
already-built daemon. Keep the library with its development build. For distributable artifacts, use the
[shared-runtime source build](/getting-started/#build-from-source), which verifies
the official library and stages it with the executables.

The interactive database lives in the named `fleet-dev-postgres-data` volume,
which is separate from the disposable clusters in `docker-compose.dev.yml`. When
`fleet-dev` starts the container, it stops the container as the attached session
exits. The volume survives.

Other useful commands:

```bash
bin/fleet-dev plan trawl --format human
bin/fleet-dev plan trawl --format json
bin/fleet-dev setup trawl
bin/dev --release-spa
bin/dev --with-coastwatch
```

`plan` reads no token files, resolves no credentials, opens no database
connection, and changes no Tailscale configuration. Under Tailscale exposure it
does make two read-only queries to the local daemon, for the node's MagicDNS
name and its tailnet IPv4, so `plan` then needs a running, logged-in
`tailscaled` unless you pin both values in a profile.

## Write a machine profile

Invocation overrides beat `~/.config/fleet/dev.toml`, and absent values fall
back to the committed conventions. A CNPG and Tailscale workstation profile can
stay small:

```toml
exposure = "tailscale"
database = "cnpg"

[op]
token_file = "~/.config/op/workstation-token"

[paths]
coastwatch = "~/code/coastwatch"

[cnpg]
host = "192.0.2.10"
port = 5432
state_scope = "cnpg"
credential_ref_template = "op://Development/CNPG {database}/{field}"
session_aead_key_ref = "op://Development/Fleet session key/credential"
```

The token file must be a regular file, not a symlink, owned by you, with mode
`0600`. Use a least-privilege 1Password service account that can read only the
development items. The controller reads the token only when the selected
database or app resolver needs 1Password, and it passes the token only to `op`
validation and to declared credential resolvers. Migrations, preparation
commands, `mprocs`, and the application processes never inherit it.

CNPG connection strings require TLS, so the client cannot fall back to
plaintext. The development contract encrypts the transport and does not verify
server identity. Provide that at the network or database boundary if you need
it. CNPG preparation is read-only: it checks the database names, `public` schema
migration rights, and Coastwatch's pre-provisioned `vector` and `pg_trgm`
extensions. It creates no databases, roles, grants, or extensions.

To override a profile for one invocation, pass the flags instead of editing it:

```bash
bin/fleet-dev --database docker --exposure localhost dev trawl \
  --app-root "$(pwd)"
```

## Expose the stack over Tailscale

Tailscale mode terminates HTTPS and issues a host-only, Secure `fleet_session`
cookie:

```bash
# Persistent setup. Missing mappings are created, and conflicts are refused.
bin/fleet-dev setup trawl --exposure tailscale

# Ordinary launches verify Serve state and never modify it.
bin/dev --tailscale
```

The conventional Trawl mapping is `:8444 -> http://127.0.0.1:8081`. If that
public port already points somewhere else, read the old and proposed targets in
the error, then replace it explicitly:

```bash
bin/fleet-dev setup trawl --exposure tailscale --force
```

The generated Trunk backend authority uses the same MagicDNS hostname as the
browser origin, and the controller publishes that origin to the browser-facing
process as `FLEET_SESSION_PUBLIC_ORIGINS`. Fleet's CSRF check compares a present
`Origin` header against that list whole, scheme and port included, so
development gets the production rule.

The controller discovers the node's identity with `tailscale status --json` and
`tailscale ip -4`. Pin both values in the profile to skip discovery, which lets
`plan` and `doctor` run with no daemon at all:

```toml
[tailscale]
hostname = "dev-host.example.com"
ipv4 = "100.64.0.10"
```

Every app on that development hostname shares the cookie, because
`fleet_session` is host-scoped, not port-scoped. The Origin check is narrower
than the cookie's reach: it matches scheme, host, and port, so a page on another
port of that hostname is a different origin and is refused. The cookie still
belongs to the hostname, and a sibling there can overwrite it. Do not expose an
untrusted frontend on that hostname.

Matching those hostnames also means the backend binds the node's tailnet
address, not loopback. `https://<node>:8444` is the front door, and
`<node>:8090` is reachable directly by every tailnet peer without passing
through Serve. Authentication still applies. Use tailnet ACLs if that matters.
The controller prints this on every Tailscale launch.

Serve mappings are persistent and survive the stack exiting. `:8444` keeps
pointing at `127.0.0.1:8081`, so anything that later binds 8081 is published
across the tailnet. Remove a mapping when you are done with it:

```bash
tailscale serve --https 8444 off
```

## Recover persistent state

Provider-scoped controller state lives under `$XDG_STATE_HOME/fleet/<scope>/`,
or `~/.local/state/fleet/<scope>/` when `XDG_STATE_HOME` is unset:

- `provider.json` binds the scope to the normalized provider, host, port, and
  `fleet_dev` database.
- `dev-api-key` is the persistent development API key.
- `session-aead-key` is the persistent Docker session key.

Directories are `0700` and secret files are `0600`. CNPG session material is
resolved per launch and never cached. A scope identity mismatch fails instead of
reusing a key from another Fleet database.

To reset the persistent Docker database, which destroys your local development
data:

```bash
docker compose --project-name fleet-dev \
  --file fleet-dev.compose.yml down --volumes
```

To replace a missing or revoked development key on the next launch:

```bash
rm ~/.local/state/fleet/docker/dev-api-key
```

That path is the default state location. If `XDG_STATE_HOME` is set, remove
`"$XDG_STATE_HOME/fleet/docker/dev-api-key"` instead. Confirm the provider scope
before you remove controller state, because the API key and the Docker session
key cannot be recovered after deletion. Keep the Docker credentials inside the
loopback-only development provider.

## Run Coastwatch alongside Trawl

The controller supports a Coastwatch manifest and a combined plan. Run
`bin/fleet-dev all` to launch both apps. Trawl also runs on its own.

The two-app schema uses fixed conventions rather than a plugin API. A
browser-facing backend named `web` receives the Coastwatch bind alias and the
common Fleet topology and cookie environment. `web-ui` receives a private
generated copy of its committed `Trunk.toml`. Generation changes only the
absolute target and dist paths, the serve address and port, and the proxy
backend, and preserves settings such as CSP nonces and `no_redirect`. Both apps'
browser-facing processes must map
`FLEET_SESSION_AEAD_KEY = "fleet.session_aead_key"`. Manifest validation rejects
an app that could drift away from the shared development identity.

## Use an internal development image

The manually dispatched [Dev image workflow](https://github.com/jakub/trawl/actions/workflows/dev-image.yml)
publishes a `linux/arm64` image for existing internal test installations. This
channel publishes no matching CLI download, Helm package, or stable release.

Use the completed run's summary for the image tag and full source revision.
The summary gives commands to create an isolated checkout at that revision and
build `trawl` for your local host with Rust 1.98.0. Install the
[source-build prerequisites](/getting-started/#build-from-source) before that
build. The CLI and image then use the same source revision, even when your CLI
host uses a different architecture.

Use `chart/trawl` from that same checkout. Select the test Kubernetes context,
namespace, and existing release explicitly. Review a complete development
values file against that chart, then use the summary's command with explicit
image repository and tag. The command resets previous chart values before it
applies your file, so include all configuration that the test installation
requires. Do not combine an arbitrary local chart or retained release values
with a new development image.
