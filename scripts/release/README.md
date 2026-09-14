# Distribution tooling

Product source and release tooling can come from different immutable commits.
Pass the product checkout to `build-distribution.sh` and `package-debian.py`;
run these scripts from the workflow checkout. `distribution.json` records both
SHAs. `check-release-source.py` requires a clean tracked checkout at the
resolved SHA, with workspace and lockfile package versions already matching
the release tag. Release builds never rewrite version fields. An old tag whose DuckDB lockfile does not match `duckdb-runtime.json` is
refused. Do not silently use a newer runtime for that tag.

`build-distribution.sh SOURCE TARGET RUNTIME [--cli-only]` verifies the official
DuckDB archive, disables the bundled feature, retains CLI clipboard support,
and adds a relative runtime search path. Linux uses cargo-zigbuild; macOS uses
native Cargo. Build and precompress the SPA before the Linux server build.
The official shared library includes ICU, JSON, and Parquet. Changing the
runtime manifest requires repeating the extension and timezone checks.

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

Fast helper tests:

```sh
python3 scripts/release/test_distribution.py
python3 scripts/release/test_release_source.py
bash -n scripts/release/build-distribution.sh scripts/release/test-installed-debian.sh
```

These source-level tests do not replace the native Linux and macOS artifact
jobs. macOS signing here is ad-hoc signing for a valid modified Mach-O image;
it is not Apple Developer ID signing or notarization.
