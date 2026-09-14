# Final local launch candidate, 2026-09-13

The candidate at `384a28a8fbc2615c3f6de439f1027b115e2c43d5` passed the checks
recorded here. Its product version remains `0.4.0`. No version bump, tag,
release, publication, deployment, or existing-state reset occurred.
Later ledger and evidence commits do not change the tested application source.

All processes, databases, accounts, events, keys, certificates, and homes used
for these checks were disposable. This directory retains no credentials or
private keys. Process and container IDs in logs are historical evidence and
must not be used for cleanup.

## Results

| Check | Observed result |
| --- | --- |
| Default workspace nextest suite | 4,413 passed, 14 skipped, no failed tests |
| Secondary feature suite from `lefthook.yml` | 2,089 passed, 11 skipped, one slow test, no failed tests |
| Release helper suite | 43 passed, including real Git, Debian metadata, parsed workflow dependencies, and provenance fixtures |
| Release SPA | Built from the candidate; gzip 3.33 MiB and Brotli 2.05 MiB, within existing budgets |
| Linux archive | Extracted elsewhere; exact binary set, ELF architecture, relative loader paths, licenses, source/tooling SHA, and clean embedded CLI commit passed |
| Offline DuckDB and CLI | Built-in ICU, JSON, and Parquet; UTC and IANA zones; three input rows and two-row export/reopen passed without network or a prepopulated home |
| Debian packages | Actual CLI, server, and runtime packages installed and removed offline; exact runtime dependencies and one library owner passed |
| File-capability loading | Actual daemon configuration check passed as `trawl`; independent loader probe reported `secure=1`; no data directory was created |
| Packaged tutorial | Fresh database/key setup, three-event ingest, exact CLI/browser/TUI rows, Save/Share behavior, and local Parquet reopen passed |
| Symbols and strip | All five application symbol files generated with pinned `dump_syms 2.3.7`; every GNU build-ID note remained byte-identical after stripping |
| Documentation | Release and development builds each passed 38 pages, 2,947 local links, 26 TOML blocks, and 18 DSL stages |
| Coastwatch anchor | `fee31c6a72ebf4db552475a5ae90492633f0d2bf` pins the candidate; all 64 Coastwatch application migration objects match its original base |

The nextest suites ran sequentially against one owned PostgreSQL 18 container,
with eight test threads and the committed admission configuration. The default
run includes the two real Cargo/Git build-provenance fixtures and the eleven-test
startup suite: nine actual-daemon tests and two fixture-helper checks. The secondary run uses the existing
`http_api`, `hot_query`, and `timestamp_repair` exclusions from `lefthook.yml`.
The skipped counts are reported as skips, not passes.

The build used `build-distribution.sh`, the pinned official DuckDB runtime,
Rust 1.98.0, cargo-zigbuild, a GNU libc 2.31 link target, and the rebuilt embedded
SPA. Binaries were copied to a disposable staging tree before symbol generation
and stripping. The original build cache was not stripped.

Debian packaging used the same stripped binaries, `package-debian.py`, and an
export of the candidate's committed source. The packager restored both modified
Cargo manifests byte-for-byte. Packaging and installation ran in the immutable
Bookworm image recorded in `artifact-manifest.json`, with networking disabled.
The installed image contains Rust 1.98.1, used only by cargo-deb for metadata.
Application binaries were built with Rust 1.98.0.

## Reproduce the checks

Use an owned PostgreSQL cluster for the Rust commands. Never point these tests
at a saved profile or an existing installation.

```bash
DATABASE_URL="$OWNED_TEST_DATABASE_URL" cargo nextest run --workspace --test-threads=8
DATABASE_URL="$OWNED_TEST_DATABASE_URL" cargo nextest run \
  -p fleet-auth -p trawl-web -p trawl-engine -p trawl-server -p trawl-cli \
  --no-default-features --test-threads=8 \
  -E 'not (binary(http_api) | binary(hot_query) | binary(timestamp_repair))'
python3 -m unittest discover -s scripts/release -p 'test_*.py' -v
```

The recorded tutorial harness is evidence of this run, not a supported product
command. Copy `block-*.sh`, `run.sh`, `browser.mjs`, and `tui.py` to
`.tmp/final-tutorial/` in a checkout with the Playwright dependencies installed.
From the checkout root, run `bash .tmp/final-tutorial/run.sh /absolute/package`.
The package argument must name an extracted directory with `bin/` and `lib/`.
The harness refuses an existing tutorial container and occupied test ports.
All fifteen extracted Bash blocks match the candidate manual byte-for-byte.

The harness supervises the daemon and proxy commands that the tutorial places
in separate terminals. It executes the TUI through an owned PTY, private home,
explicit client config, and driver socket. The two literal terminal-cleanup
blocks are replaced by the harness trap. The final log records successful
process, container, temporary-home, and socket cleanup. The separate workspace
PostgreSQL container was removed after both suites completed.

## Evidence and limits

`artifact-manifest.json` records archive, Debian package, and application-symbol
hashes. Recorded logs remove trailing whitespace only; `log-provenance.json`
records both original and stored hashes. The large artifacts remain in the owned local artifact directory, outside
Git. The JSON reports, logs, source harness, screenshots, and checksums here can
be inspected through Git. The screenshots contain only synthetic records and
an empty login field. Root inspected all three images and the TUI text capture.

The final artifact directory is
`.tmp/final-artifacts-384a28a8fbc2615c3f6de439f1027b115e2c43d5/` in the
launch-readiness worktree. Build and packaging commands are maintained in
[`scripts/release`](../../../../scripts/release/README.md).

Local stripping used system LLVM 22.1.8. CI selects LLVM from the Rust toolchain,
so these are the same operations with a different tool installation. Application
Breakpad files were generated; full upstream DuckDB C++ symbols were not.
TUI evidence covers exact rows, text rendering, clean exit, and socket cleanup,
not colors, pixels, or clipboard behavior. The Debian check does not start a
systemd service. The tutorial exercises real daemons separately.

Native macOS and Linux arm64 execution, public anonymous downloads, live APT,
registry publication, and stable-site deployment remain unverified. Earlier
Coastwatch consumer tests and live cert-manager issuance are recorded separately
in the readiness ledger. This candidate's native source review has no remaining
finding; the required cross-family review remains unavailable after verified
account quota failures. Local evidence does not satisfy that review gate.

Two local harness invocations failed before verification: the distribution
checker was initially given `--version` instead of `--expected-version`, and
offline cargo-deb was initially pointed at absent Rust 1.98.0 instead of the
container's installed 1.98.1 metadata toolchain. Corrected invocations passed.
Neither failure changed the application binaries or the package implementation.
