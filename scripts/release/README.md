# Distribution tooling

Product source and release tooling can come from different immutable commits.
Pass the product checkout to `build-distribution.sh` and `package-debian.py`;
run these scripts from the workflow checkout. `distribution.json` records both
SHAs. `check-release-source.py` requires a clean tracked checkout at the
resolved SHA, with workspace and lockfile package versions already matching
the release tag. Release builds never rewrite version fields. An old tag whose DuckDB lockfile does not match `duckdb-runtime.json` is
refused. Do not silently use a newer runtime for that tag.

`build-distribution.sh SOURCE TARGET RUNTIME [--cli-only|--image-only]` verifies the official
DuckDB archive, disables the bundled feature, retains CLI clipboard support,
and adds a relative runtime search path. `--image-only` selects the four
server-image executables without changing their release profile or debug settings.
Linux amd64 uses cargo-zigbuild with a glibc 2.31 compilation target. Linux arm64
uses Cargo with a GNU compiler driver so Rust's Cortex-A53 erratum 843419
mitigation reaches the linker. Native arm64 source builds need a GNU compiler;
cross builds also need the target compiler and standard library. macOS uses
native Cargo. Build and precompress the SPA before the Linux server build.
The official shared library includes ICU, JSON, and Parquet. Changing the
runtime manifest requires repeating the extension and timezone checks.

`build-arm64.sh SOURCE RUNTIME [--cli-only|--image-only]` builds Linux arm64
distributions in the pinned Rust 1.98.0 Bookworm container. It supports amd64
cross compilation and native arm64 hosts. Docker is required. The container
uses GNU target tools, checks the Cortex-A53 mitigation with an aligned
instruction sequence, and runs as the caller's UID/GID. It preserves
`RUSTFLAGS` and `CARGO_PROFILE_RELEASE_STRIP`. Output goes under
`SOURCE/target/bookworm`, separate from host SPA and build-script artifacts.
An isolated Cargo home under `target/bookworm-home` contains the container's
registry cache; host Cargo credentials and executables are not mounted.
Linked-worktree Git metadata is mounted read-only for source provenance.

The supported Linux distribution floor is Debian 12, including the official
DuckDB library and system C++ runtime. The amd64 Zig target does not declare an
older supported distribution. A direct native arm64 build on a newer system
does not establish Bookworm compatibility; use `build-arm64.sh` for portable
artifacts and repeat the fresh native Bookworm checks.

Ordinary native Cargo commands use `trawl-core`'s build script to call the same
`distribution.py prepare` verifier. They need Python 3.11 or newer and curl.
The helper checks each cached ZIP before extraction, stages files atomically,
and serializes writers to the same checksum-addressed archive cache. Each build
gets a private runtime plus a loader copy in its profile's `deps` directory.
Run Cargo from the source checkout to load `.cargo/config.toml`. For commands
started elsewhere, set `DUCKDB_NO_PKG_CONFIG=1`, for example
`DUCKDB_NO_PKG_CONFIG=1 cargo build --manifest-path /path/to/trawl/Cargo.toml`.
This also applies to `cargo install --path` and explicit runtime directories:
upstream still probes pkg-config with `DUCKDB_LIB_DIR` set, so the build rejects
a missing suppression setting before it can link a host-selected library.
`DUCKDB_LIB_DIR` selects an existing release runtime directory. Cargo invokes
`distribution.py verify` to check its ZIP and extracted files without writing to
that directory or downloading missing inputs. Mismatches stop the build. Only
the Cargo loader copy is staged. `distribution.py prepare` remains the explicit
command for constructing or repairing an output directory. The helper does not add an absolute rpath, so
the distribution build's relative loader path remains the only shipped rpath.

`distribution.py stage` creates `bin/` and `lib/trawl/`, includes the runtime
license, the product checkout's MPL license, platform floor, and provenance, and normalizes/signs Mach-O loader paths on macOS.
`verify-distribution.py` checks architecture, dependencies, loader paths and
provenance, then executes the runtime probe and CLI fixture in a fresh home.
Verification belongs on a fresh native runner, outside the source build tree.
For Linux, run it inside the supported Debian environment, with no network.

`package-debian.py` uses real dpkg dependency analysis and cargo-deb's documented
metadata variants. It temporarily adds literal dependency overrides and restores
the source manifests in a `finally` block. The runtime package alone owns
`/usr/lib/trawl/libduckdb.so`; both executable packages depend on its exact Debian
version. The script keeps cargo-deb's existing assets, service units, maintainer
scripts, and conffiles. It requires target-architecture system libraries and
Debian tools in the build environment. `CARGO_TARGET_DIR` must point to the
same target directory used for compilation.

Run `test-installed-debian.sh PACKAGE_DIRECTORY` only inside a disposable Debian
container as root, with `SYS_PTRACE` in the capability bounding set. It blocks
service startup through `policy-rc.d`, installs all three real packages, checks
ownership and exact dependencies, exercises the CLI and daemon check mode,
and tests file-capability loading with `AT_SECURE=1`. It removes the CLI and
server independently before removing the runtime. It starts no database.

Run `test-host-debian.sh PACKAGE_DIRECTORY` only on a disposable GitHub-hosted
runner, where systemd is PID 1. It installs `trawl-runtime` and `trawl-server`
on the host and asserts that `trawld` and `trawl-web` are `disabled` and
`inactive`, and that the install prints the enable instruction. It then
enables both units, reinstalls the server package through the upgrade path,
asserts that both stay enabled and inactive, and purges the package.

Run `test-trial.sh TRAWL EVIDENCE PRIVATE` only on a disposable Docker engine.
It proves `trawl trial` (ADR-0045) end to end with the given CLI. It creates
two trials, one after the other, and deletes both. Along the way it injects
the faults that ADR-0045 names: a refused preflight, taken ports, a foreign
trial id, a kill after the key step, a lost token file, a substituted
certificate, a kill during seeding, and a sample result the trial cannot
account for. It refuses to start while the engine holds any trial resource or
a trial port is taken. `EVIDENCE` receives the log, the rendered configs, and
the browser screenshots. `PRIVATE` holds the trial's `HOME`, its state, and
`secrets.tsv`, the list of generated secret values. `scan-trial-secrets.py`
reads that list and fails when a value appears in the evidence. The script
never prints a secret. Everything it and its commands print goes first to
`PRIVATE/capture.log`. Before any of it is printed,
`harvest-trial-secrets.py` records every secret that exists at that moment,
and under GitHub Actions passes each one to `::add-mask::`. Then the scanner
checks the whole capture. When a value is found, the job log gets the scan
result, and no later output. The exit trap follows the same order.
`trial-browser.mjs` follows the tutorial's browser steps in
Chromium, with Playwright from `crates/trawl-web-ui/e2e`. For a local run,
set `TRIAL_IMAGE` to a locally built image, and set `TRIAL_BROWSER=skip` when
that image has no SPA. CI refuses both. The `trial` job in
`linux-distribution.yml` builds the image from the release tarball and tags
it with the CLI's default reference. `trawl trial up` therefore runs without
`--image`. The tag exists only on the disposable runner.

`check-anonymous-pulls.sh TAG IMAGE_REPOSITORY CHART_REPOSITORY` pulls the
release image and chart with empty Docker and Helm registry configs, so no
stored login applies. `anonymous-pull.yml` runs it after a release push, on
manual dispatch for any tag, and on pull requests that change it.

Fast helper tests:

```sh
python3 scripts/release/test_distribution.py
python3 scripts/release/test_cargo_runtime.py
python3 scripts/release/test_release_source.py
python3 scripts/release/test_scan_trial_secrets.py
python3 scripts/release/test_trial_capture.py
bash -n scripts/release/build-distribution.sh scripts/release/build-arm64.sh scripts/release/test-installed-debian.sh scripts/release/test-host-debian.sh
```

These source-level tests do not replace the native Linux and macOS artifact
jobs. macOS signing here is ad-hoc signing for a valid modified Mach-O image;
it is not Apple Developer ID signing or notarization.

Release publication prepares and validates the chart before any registry write.
The image push consumes the same normalized Docker metadata as that chart.
Helm then publishes the exact prebuilt, checksum-checked chart artifact; only
successful image and chart publication, followed by an anonymous pull of
both, permit GitHub release creation. Docs
and APT publication follow the GitHub release. This ordering prevents an
announcement before its registry channels exist. It is not a transaction
across services: a later GitHub or Pages failure still requires a rerun.

`linux-distribution.yml` and `macos-cli.yml` accept non-publishing manual runs
as well as reusable workflow calls. Both require full `source-sha` and
`tooling-sha` commits and a `release-tag` label matching the committed version
(for example, `v0.4.0`). A manual run creates no tag. Linux completion includes
both native architecture verification jobs; the release workflow calls this
same workflow and waits for its verification results.

`distribution-preflight.yml` runs these same native checks for pull requests
that change distribution workflows, release tooling, or DuckDB dependency
manifests. It derives the version label from the checked-out PR merge commit
and uses that full commit SHA for both product and tooling. This caller allows
verification before the manual workflows are registered on the default branch;
it has read-only repository permissions and no publication jobs.

Release preparation also requires a nonempty `docs/releases/RELEASE_TAG.md`
announcement in the selected product commit, for example
`docs/releases/v1.0.0.md`. Preparation reads committed bytes, preserves them in
a checksum-checked artifact, and fails before registry publication if the file
is missing or empty. GitHub publishes that exact body with generated release
notes disabled. The workflow checkout and local file edits cannot substitute
announcement text for the selected product commit.
