# Fleet development controller

status: accepted (2026-07-30); amended (2026-11-23): rulings moved from code comments, see the Amendment

Issue #54 replaces the Trawl and Coastwatch shell launchers with one
Trawl-owned, local-only Rust controller. The goal is a convention-heavy
development path, not a general plugin system.

## Decisions

- `fleet-dev` owns database preparation, Fleet migration, the development
  role/key, shared session material, Tailscale Serve reconciliation, generated
  Trunk configuration, and the attached `mprocs` process tree.
- Trawl and Coastwatch declare only app-specific facts in versioned
  `fleet-dev.toml` manifests. Fleet auth is a built-in prerequisite, not a
  manifest dependency node.
- `bin/fleet-dev` is the Cargo binstub and `bin/dev` is a frozen compatibility
  translator. The controller is the only orchestration implementation.
- The three configuration layers are invocation override, machine profile,
  then committed convention. No profile means localhost plus the dedicated
  persistent Docker provider.
- Docker and CNPG are explicit modes. Docker is loopback-only and may use its
  local bootstrap superuser; CNPG is connection/rights validation only and
  never falls back to Docker. CNPG client connections require encrypted
  transport and cannot opportunistically downgrade to plaintext. Preparation
  validates existing database names, migration rights, and fixed app extension
  requirements without issuing provisioning DDL.
- Credential resolution is demand-driven. Ordinary children start from a
  small allowlist; only the validator and structured resolver receive the
  1Password service token. Resolved values are mapped to individual consumers.
- Persistent credentials are scoped and bound to a normalized Fleet database
  identity. Generated runtime configuration is private and ephemeral.
- Local shared SSO uses one host-only `fleet_session` cookie with path `/`.
  Localhost permits an insecure cookie; Tailscale uses HTTPS and a Secure
  cookie. Production configuration keeps its existing Kubernetes-managed
  key/domain behavior.
- Tailscale setup is explicit and persistent. Launch verifies mappings but
  never mutates them. Conflicts require `setup --force` after showing the old
  and proposed mapping.
- One OS-level lock permits one attached Fleet development stack at a time.

## Security boundaries

The app manifest is declarative but still validated as input: unknown fields,
path traversal, duplicate destinations, malformed resolved names, token
shadowing, and Trawl permission drift fail closed. Resolver output is a
single versioned JSON document; values are always secret, bounded, redacted,
and zeroized. Token and state files reject symlinks and broad permissions.

The generated proxy keeps browser Origin and proxied Host on the same
hostname. Development does not relax Fleet's present-only same-host CSRF
validation. Because that validation and host-only cookies intentionally span
ports, all development apps on the shared hostname are one browser trust
boundary.

(Superseded on this point by ADR-0016, 2026-09-04. The CSRF check no longer
consults `Host` at all, in this paragraph or in the Tailscale one below: it
compares the browser's `Origin` whole, port included, against the configured
`public_origins` allowlist, so two development apps on one hostname and
different ports are no longer one origin. What still spans ports is the
host-only cookie, which is a cookie-scoping rule the browser applies, not a
verdict fleet-auth reaches. fleet-dev publishes the browser origin it just
computed as `FLEET_SESSION_PUBLIC_ORIGINS`, so the allowlist follows the
stack instead of being inferred from a header.)

Tailscale exposure publishes more than the Serve front door. Trunk stamps the
proxy backend authority into `Host`, and the present-only guard requires that
host to equal the browser Origin host, so the application's API listener binds
the node's tailnet address rather than loopback. Every tailnet peer can reach
it directly on the backend port, bypassing Serve's TLS termination.
Authentication still applies and transport to remote peers is WireGuard
-encrypted, but the backend port is tailnet-visible: tailnet ACLs, not Serve,
are the access boundary. The controller says so on every Tailscale launch.

Serve mappings are persistent by design and survive the stack that created
them. A configured public port keeps pointing at the SPA's loopback port after
the controller exits, so whatever binds that port next is published tailnet
-wide until the mapping is explicitly removed.

## Consequences

Local startup is now one command and is viable for non-interactive agents.
The persistent Docker volume and provider-scoped credentials require explicit
recovery/reset commands. Adding another application is intentionally out of
scope until real use demonstrates a need beyond Trawl and Coastwatch.

This ADR supersedes the `.fleet.localhost`, hand-written temporary Trunk file,
manual key bootstrap, and joined-shell orchestration described by the retired
Trawl `bin/dev` implementation. Production deployment and credential delivery
are unchanged.

## Amendment (2026-11-23): rulings moved from code comments

A comment audit on 2026-11-23 removed these rulings from the code, where they were the only record. They were already in force. None is a new decision.

- **The CSRF allowlist that fleet-dev publishes as `FLEET_SESSION_PUBLIC_ORIGINS` has exactly one entry: the `browser_origin` of the current topology.** The browser origin depends on the machine and the exposure mode, so a committed `[web] public_origins` would be wrong in at least one mode. The topology publishes one origin, so any other origin the operator browses to is foreign and gets a 403. The contract tests in `fleet-dev/tests/contracts.rs` check both modes, because a wrong Tailscale value shows up only as a 403 on a tailnet login.
- **The child allowlist in `environment.rs` passes the proxy variables (`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY` and their lowercase forms) to ordinary children, even though a proxy URL can embed credentials.** This is a deliberate exception to "ordinary children start from a small allowlist". `op`, `docker` and `cargo` fail behind a proxy without these variables, and the value is already in the controller's own environment.
- **Correction.** The Tailscale paragraph under "Security boundaries" says the API listener binds the tailnet address because "the present-only guard requires that host to equal the browser Origin host". That guard no longer exists (ADR-0016). The live reason is that the generated Trunk proxy reaches the backend at `backend_authority`, the same MagicDNS name the browser uses, and that name resolves to the node's tailnet address, not to loopback. The exposure consequence in that paragraph is unchanged.
