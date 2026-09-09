# History export and clear evidence

Application source is commit `2e81545c07a8eca434cdf109f7a5332c45016374`, with server checkpoint `38dd2842d081923af06294e3b24ae1dbd0b4737e` and serializer checkpoint `6c524ac7ca7792b521fc1c8c602f02007b4b510f`.

The browser tests use the built SPA and a scenario server. They do not prove live authentication or database isolation. Native server tests use real Postgres. No production database is part of this evidence.

| Acceptance criterion | Verdict | Re-executable evidence |
| --- | --- | --- |
| 1. Filtered loaded-page CSV and JSON preserve row order | PASS | [History browser spec](../../crates/trawl-web-ui/e2e/tests/history-export-clear.spec.ts), download-byte test, with the [history export fixture](../../crates/trawl-web-ui/e2e/harness/wire/history-export.json) |
| 2. CSV formula protection matches the server | PASS | [Shared formula guard](../../crates/trawl-api/src/csv.rs), [serializer tests](../../crates/trawl-web-ui/src/history_export.rs), and the browser download-byte assertion |
| 3. Export is disabled during loading, after failure, and for no matches | PASS | [History browser spec](../../crates/trawl-web-ui/e2e/tests/history-export-clear.spec.ts) covers those states and retained rows during a page request |
| 4. Clear is scoped, counted, repeatable, and permission-gated | PASS | [Store test](../../crates/trawl-server/tests/store_pg.rs) and [authenticated handler tests](../../crates/trawl-server/tests/auth_pg.rs), selected by `cargo nextest run -p trawl-server history_clear` |
| 5. Confirmation, single DELETE, canonical page-zero refetch, preserved failure state | PASS | [History browser spec](../../crates/trawl-web-ui/e2e/tests/history-export-clear.spec.ts) covers cancellation, pages zero and two, server error, lost response, invalid response, retry, and completion after unmount |
| 6. Placeholder toasts are gone | PASS | [History browser spec](../../crates/trawl-web-ui/e2e/tests/history-export-clear.spec.ts) and [HistoryPage](../../crates/trawl-web-ui/src/pages/history.rs), replacement hunks in application commit `2e81545c` |
| 7. Selectors and fixtures are pinned | PASS | [Selector contract](../../crates/trawl-web-ui/tests/e2e_selector_contract.rs) and [wire-fixture contract](../../crates/trawl-web-ui/tests/e2e_wire_fixture_contract.rs) |
| 8. Removing the key predicate fails the two-key test | PASS | [Mutation patch](history-clear-unscoped.patch), [native runner](history-clear-mutation.sh), and [observed baseline, mutation, and control transcript](mutation-transcript.txt) |

Observed checks passed: 3 server tests selected by `history_clear`, all 6 history serializer tests, 38 selector and wire-fixture contract tests, and all 12 focused history browser tests. The full browser suite passed 116 tests before the final style and assertion changes; the 12 history tests passed again after those changes. The final full-suite result is therefore limited to that earlier browser revision. CI links accompany the PR.

The [native transcript](mutation-transcript.txt) records an additional passing store-test baseline, the exact assertion that killed the unscoped DELETE mutation, and a passing restored-source control. It names the source commit and store blob used by the run. Later evidence-only amendments do not change the application or runner bytes.

Run the focused browser suite with `E2E_PORT=<unused-port> cargo xtask e2e --grep 'history'`. Rebuild after source changes. Use the worktree's own build and SPA output directories.

## Native mutation

Set `DATABASE_URL` privately to an agent-owned Postgres instance whose role can create test databases. From a clean committed worktree, run:

```sh
bash visual-evidence/issue-165/history-clear-mutation.sh
```

The runner requires exactly one passing baseline test, applies the committed patch, and accepts only nextest exit 100 plus the named other-key assertion and the missing foreign row. Compile failures, database failures, parameter errors, zero tests, and unrelated assertions cannot count as kills. It reverses the patch, reruns the same test as a passing control, and requires a clean tree. EXIT, INT, and TERM also attempt reverse-patch restoration. Detailed logs stay under ignored `target/issue-165`; connection details are not printed in the compact transcript.
