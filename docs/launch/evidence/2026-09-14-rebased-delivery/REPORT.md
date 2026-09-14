# Rebased launch delivery, 2026-09-14 UTC

The user authorized pushing both prepared branches and opening draft PRs for
nonpublishing CI. Trawl `061d1d444ec942e56a6bb26e2a9c3836aec87f6b` and
Coastwatch `58ca2f511c7246d22b7df7004ee5ec04a653b3cb` were pushed through
their normal hooks. Independent `git ls-remote` reads matched both local heads.
Later evidence commits do not change the tested application or tests.

## Integration and source identities

Trawl PR #185 merged at `723ac5d70834cda4d923652a9797ef0037c43e54` while
push authorization was pending. The launch branch was rebased onto that main
commit. The old saved-name restriction was dropped because main now supports
readable names. Search preserves main's executed-query identity and modal
lifetime behavior, together with the launch's first-use guidance. The example
callback now supplies its explicit one-hour range through `SearchNavigation`.

The application source is `335a34dfbcf844dae5cfb91d0bea923fffabacb8`.
Subsequent commits only isolate a Git/Cargo test fixture and make two browser
locators exact. Coastwatch pins this application source. Its 64 application
SQL migrations and the migration README match its original main commit
`5a29066b0d390f44fa3eec9fe027a04fee48b5ef` byte-for-byte; the 65 Git objects
are recorded in `coastwatch-migration-objects.txt`.

The [earlier packaged-install evidence](../2026-09-13-final-candidate/REPORT.md)
belongs to historical source `384a28a8`, before this rebase. The release
workflows and `scripts/release/` are byte-identical between that candidate and
this one. Those earlier logs do not establish native artifact execution at the
rebased source. The draft PR's native distribution jobs provide that next check.

## Observed results

| Check | Result |
| --- | --- |
| Trawl normal pre-push default suite | 4,427 passed, 14 skipped; 48.029 seconds test runtime |
| Trawl normal pre-push secondary suite | 2,094 passed, 11 skipped; 43.177 seconds test runtime |
| Provenance under an outer Git environment | Both real Cargo/Git fixtures passed with outer `GIT_DIR`, `GIT_WORK_TREE`, `GIT_COMMON_DIR`, `GIT_OBJECT_DIRECTORY`, and an owned dummy index path |
| Release SPA | Trunk completed at application source `335a34df`; no later production UI change |
| Focused browser integration | 67 passed in Chromium, one worker, owned port 18243 |
| Coastwatch normal pre-push suite | 2,754 passed, zero skipped; 42.411 seconds test runtime |
| Normal pre-commit hooks | Rust formatting, workspace Clippy, and applicable Wasm Clippy checks passed |
| Development documentation | 38 pages, 2,947 local links, 26 TOML blocks, and 18 DSL stages checked |
| Native independent review | Daybreak high found no remaining issue in the rebased UI/storage seams or final test-only fixes |

The browser run covers first use, Save scope, readable names, modal lifetime,
result actions, authentication failures, and accessibility/layout regressions.
It uses the existing stub server. It is separate from the earlier real packaged
tutorial. The SPA build finished before the recorded valid runs started.

## Failures and corrections

The first Trawl push stopped after two provenance fixture failures. Git's hook
environment made a nested fixture Cargo build inspect the outer Trawl checkout.
An explicit outer `GIT_DIR` reproduced both failures. Both fixture subprocess
entry points now clear the repository-local variables reported by
`git rev-parse --local-env-vars`, without changing the process-global environment
or the production build script. The reproduced tests and unchanged normal push
hook then passed.

The first valid browser run passed 64 tests and failed three because the API-key
locator also matched the new help panel's accessible name. Two locators now use
`exact: true`; their focus, error association, and invalid-state assertions are
unchanged. All 67 then passed. An earlier run started before Trunk finished
publishing assets and was interrupted; it is not product verification.

Coastwatch's first push hit seven cold-database migration deadlocks. PostgreSQL
showed concurrent index creation waiting on virtual transactions while other
tests waited on the migration advisory lock. One existing database-backed test
initialized only the owned fixture database. The unchanged full hook then
passed. No migration or hook was modified to obtain this result.

## Evidence and limits

The complete successful logs and relevant failed runs are committed here.
ANSI sequences and trailing whitespace were removed for readability;
`log-provenance.json` records original and stored hashes. `SHA256SUMS` covers
every other file in this directory.

All three PostgreSQL containers belonged to this verification. Trawl's two
suites used separate PostgreSQL 18 clusters on loopback ports 32788 and 32789.
Coastwatch used its own pgvector/PostgreSQL 18 container on loopback port 5432.
The persistent development databases on 5433 and 5434 were not used.
All three owned containers were removed and their absence verified; see
`cleanup.json`. An initial cleanup assertion expected Docker's error text with
different capitalization; the corrected check uses the complete container-ID
inventory and confirms absence independently.
Recorded container identifiers are evidence, not reusable cleanup targets.

The required cross-family review is still unavailable after the previously
verified account quota failures. Native review does not satisfy that gate.
Native macOS and Linux arm64 execution remains pending the draft PR jobs.
No version bump, tag, release, repository visibility change, deployment,
existing-state reset, PR merge, or remote branch deletion occurred.
