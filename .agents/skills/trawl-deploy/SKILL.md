---
name: trawl-deploy
description: Build deployable Trawl artifacts or install and upgrade Trawl on a named host or Kubernetes release.
---

# Build and deploy Trawl

Use the target and change scope given by the user. A build request ends at a
verified artifact. A deployment request also identifies a host and service,
or a Kubernetes context, namespace, release, and values source. Resolve a
missing target while doing independent local preparation. If GitOps owns the
deployment, use its declared source and existing authorized workflow.

## Prepare the artifact

Prefer the requested version or image digest. For source builds, use
`env -u NO_COLOR cargo xtask build-web --release` to build and precompress the
SPA before embedding it in `trawl-web`. Plain `cargo build --release` omits that sequence
and does not build every distribution binary. Unset `NO_COLOR` because the
installed Trunk rejects its inherited value of `1`.

Read only the channel's build and installation owners:

| Channel | Owners |
| --- | --- |
| Release binaries and Debian packages | [release.yml](../../../.github/workflows/release.yml), `crates/trawl-server/Cargo.toml`, and `crates/trawl-server/debian/` |
| Container and Helm | [Dockerfile](../../../Dockerfile), [chart README](../../../chart/trawl/README.md), `chart/trawl/values.yaml`, and `chart/trawl/templates/` |

The Dockerfile consumes prebuilt `docker-ctx/${TARGETARCH}` binaries; it is
not a source builder. Follow the release workflow for the complete binary set,
architecture, and symbols. Do not invent an independent packaging recipe.

## Prepare and apply the change

Identify the shared Fleet keystore and the dedicated Trawl app-state database.
Fleet migrations use `fleet-admin`; `trawld` migrates its own database. Preserve
the single-writer arrangement. Inspect existing service configuration and
storage before changing them, including the epoch gate in
[epoch.rs](../../../crates/trawl-server/src/epoch.rs) for an upgrade.

Helm needs both `auth.database.existingSecret` and
`storage.database.existingSecret`, including with `web.enabled=false`.
With the browser enabled, set `web.publicOrigins` to its public origins.
Check Secret key names without exposing their values. Render the complete
values before applying. For Debian, inspect the package's scripts and units.

Preserve installed cookie-key material on ordinary upgrades. Fresh local
cookie provisioning does not establish shared Fleet SSO. The
[SSO procedure](../../../docs/src/content/docs/reference/fleet-auth-cutover.md)
describes shared session material; read that section only when SSO is relevant.
Account for database migrations and data-root cutovers before promising rollback.

Apply the prepared deployment and restart scope already authorized. Record the
installed artifact identity and rollout result. Inspect `/api/v1/health` checks,
then authenticated identity and a bounded query. HTTP 200 can still report
degraded health. Verify login and assets when the browser changed. Ingest checks
need their own authorized test data and a key with ingest permission.
