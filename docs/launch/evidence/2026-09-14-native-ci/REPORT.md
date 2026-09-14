# Native distribution CI, 2026-09-14 UTC

The authorized branches are available as draft [Trawl PR #186](https://github.com/jakub/trawl/pull/186)
and [Coastwatch PR #312](https://github.com/jakub/coastwatch/pull/312).
This report records the first remote results and the ARM64 build correction.
It does not assign a release version or establish public download access.

## First remote Trawl run

The PR head was `8daf4ba63972f87730101b8a1c882fec4c72158b`, based on
main `723ac5d70834cda4d923652a9797ef0037c43e54`. The native distribution
jobs checked out PR merge source and tooling revision
`3417a0089d18a7645e5ee3669fe55455c4db1b53`. GitHub's run metadata records
the PR head; the installed-job logs record the actual checkout and embedded
artifact identity. These are different identities by design.

| Check | Observed result |
| --- | --- |
| [Full CI](https://github.com/jakub/trawl/actions/runs/34811397502) | All 23 jobs passed, including workspace tests, Clippy, Wasm, browser tests, every mutation check, Helm, Vector, release helpers, and dependency checks |
| [Documentation](https://github.com/jakub/trawl/actions/runs/34811397622) | Passed |
| [Crash-dump image](https://github.com/jakub/trawl/actions/runs/34811397608) | Passed |
| [macOS Intel installed CLI](https://github.com/jakub/trawl/actions/runs/34811398487/job/103874873567) | Passed on a fresh native runner |
| [macOS Apple Silicon installed CLI](https://github.com/jakub/trawl/actions/runs/34811398487/job/103874873621) | Passed on a fresh native runner |
| [Linux amd64 package build](https://github.com/jakub/trawl/actions/runs/34811398487/job/103873321115) | Built and uploaded artifacts; native installation was skipped because the ARM64 build failed |
| [Linux ARM64 package build](https://github.com/jakub/trawl/actions/runs/34811398487/job/103873321074) | Failed when Zig rejected Rust's Cortex-A53 mitigation argument |

Both macOS jobs relocated the archive, checked clean embedded `3417a00`
provenance, read three Parquet rows, exported and reopened two rows, and passed
offline DuckDB 1.5.5 checks for built-in ICU, JSON, Parquet, UTC, and IANA
timezones. Their complete installed-job logs are retained here. The failed
Linux matrix did not establish native Linux execution at this source.

## ARM64 correction and local verification

Rust 1.98.0 passes `--fix-cortex-a53-843419` for the GNU ARM64 target.
The CI combination of cargo-zigbuild 0.21.4 and Zig 0.16.0 rejected that flag.
An independent minimal C link also failed with Zig 0.15.2, so pinning that
older Zig version did not fix the cause. Newer cargo-zigbuild drops the flag;
that would not establish the required instruction rewrite.

Integrated commit `7226dff1544b3cb3411d04007be056229bf45f2e` builds ARM64
artifacts with GNU tools inside a digest-pinned Rust 1.98.0 Debian Bookworm
container. Both release and internal development workflows use it. Build
outputs and Cargo storage are separate from host compilation, and the helper
runs as the caller's UID/GID. The amd64 Zig path is retained.

The integrated helper's actual CLI build passed in 38.78 seconds with release
debug information and an explicit SHA-1 build ID. Independent ELF inspection
confirmed AArch64, literal `$ORIGIN/../lib/trawl` RUNPATH, `.debug_info`, and
`.symtab`. `local-arm64-artifact.json` records its source and artifact hash.
This is a cross-compiled CLI; it was not executed on ARM64 locally.

The helper runs an actual Rust/linker regression before compilation. A fixed
layout places the erratum sequence across a page boundary. The control keeps
ADRP/load/load; the Rust link replaces the final load with a branch. The
fixture suppresses GCC's configured default mitigation, so the positive link
depends on Rust's explicit argument. Removing that argument in the author's
mutation check correctly left the load in place and failed the check. Root
also independently confirmed the negative/positive rewrite with GNU ld.

Root reran all 43 release-helper tests successfully. Bash syntax and diff
checks passed. Updated source-build instructions passed the development docs
check: 38 pages, 2,947 local links, 26 TOML blocks, and 18 DSL stages. Fresh
native Linux package execution remains the next remote check.

Daybreak high independently reviewed the complete seven-file implementation
at author commit `692b89bff54ab67b02dc005a692480d708444995` and its necessary
workflow, packaging, configuration, and provenance seams. No remaining
finding was reported. The reviewed files are byte-identical after integration;
this source review did not execute the builds or native verification jobs.

## Companion consumer and remaining gates

Coastwatch `dac5c096177b149e6a1f677bd1a21e7a2f78d008` still pins Trawl's
shared application source `335a34dfbcf844dae5cfb91d0bea923fffabacb8`.
Its later fixes update two patch dependencies and give CI a fresh per-job
Fleet PostgreSQL service. All 64 Coastwatch application migrations remain
unchanged. The normal push hook passed 2,754 tests with zero skipped after
the dependency update; remote tests and dependency checks passed at the final
consumer head. Its Wasm verification remains unresolved after self-hosted
runners lost contact during compilation. The PR records subsequent retries.

Required cross-family review remains unavailable after the verified account
quota failures in the readiness ledger. Native review is supplementary.
The PRs remain drafts. Publication, repository visibility, a named release
identity, existing-installation changes, and merging remain outside the
authorized delivery. The PR check pages record later runs; this report retains
the source identities and limits of the evidence captured here.

ANSI sequences and trailing whitespace were removed from copied logs.
`log-provenance.json` records input and stored hashes. `SHA256SUMS` covers every
other file in this directory.
