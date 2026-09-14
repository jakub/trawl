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

Prefer the requested version or image digest. For a Linux source distribution,
build the SPA with `(cd crates/trawl-web-ui && env -u NO_COLOR trunk build --release)`,
then run `cargo xtask compress-web` before compiling `trawl-web`. Unset
`NO_COLOR` because Trunk rejects its inherited value of `1`. Use the
[distribution helpers](../../../scripts/release/README.md) for the native or
cross-target binaries and their checksum-verified DuckDB runtime. Plain
`cargo build --release` does not prepare a complete distribution.

Read only the channel's build and installation owners:

| Channel | Owners |
| --- | --- |
| Release binaries and Debian packages | [release.yml](../../../.github/workflows/release.yml), [distribution helpers](../../../scripts/release/README.md), `crates/trawl-cli/Cargo.toml`, `crates/trawl-server/Cargo.toml`, and `crates/trawl-server/debian/` |
| macOS CLI | [macos-cli.yml](../../../.github/workflows/macos-cli.yml) and [distribution helpers](../../../scripts/release/README.md) |
| Container and Helm | [Dockerfile](../../../Dockerfile), [chart README](../../../chart/trawl/README.md), `chart/trawl/values.yaml`, and `chart/trawl/templates/` |

The Dockerfile consumes the staged `docker-ctx/${TARGETARCH}/bin/` and
`lib/trawl/` layout, licenses, and `distribution.json`; it is not a source
builder. Keep the binaries and runtime together. The `trawl-runtime` Debian
package owns the private library; CLI and server packages require its exact
version. Follow the release workflow for the complete binary set,
architecture, and symbols. Native macOS execution is required to verify a
macOS artifact; a Linux cross-build does not establish that evidence.

## Prepare and apply the change

Identify the shared Fleet keystore and the dedicated Trawl app-state database.
Provision both databases using the
[database procedure](../../../docs/src/content/docs/operate/deployment.md#provision-the-databases).
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
[SSO procedure](../../../docs/src/content/docs/operate/access.md#share-a-browser-session-across-fleet-applications)
describes shared session material; read that section only when SSO is relevant.
Account for database migrations and data-root cutovers before promising rollback.

Apply the prepared deployment and restart scope already authorized. Record the
installed artifact identity and rollout result. Inspect `/api/v1/health` checks,
then authenticated identity and a bounded query. HTTP 200 can still report
degraded health. Verify login and assets when the browser changed. Ingest checks
need their own authorized test data and a key with ingest permission.
