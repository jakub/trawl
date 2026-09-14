# Launch first-use evidence, 2026-09-13

These are local verification artifacts, not a release or a published site.
All events, accounts, keys, processes, and databases belonged to disposable tests.
No private keys, tokens, database passwords, browser login trace, or session cookie
is retained here. Recorded process IDs are historical; never use them for cleanup.

## Exact checks

`app-experiment-report.json` is the original report from
`run-1789350917170-5c84888283`, prepared at `6b3febff`. The command was:

```bash
CARGO_TARGET_DIR=/home/jakub/code/trawl/target/launch-readiness \
  bin/app-experiment --seed 42 --events 1000 --rate 200
```

It exited 0, reported `passed`, and verified the independently generated event-ID
and value oracle through ingest, browser search/live tail, compaction, restart,
and browser-session reuse. Its source/binary/SPA hashes are in the report. Every
cleanup flag is true. This is a debug-daemon correctness check, not a benchmark.

`browser-report.json`, `login-help.png`, `tutorial-query.png`, and `save-scope.png`
come from the final literal-tutorial smoke run. It used the daemon/admin/CLI
source built at `6b3febff`, with the release SPA rebuilt for the copy change
committed as `3f477557`. The tutorial's local export explicitly requested JSON,
as committed in `e8467466`. Later packaging tests and comments do not alter
these runtime paths.

The final smoke process exited 0. Its independently checked results were:

- Exactly three events accepted; CLI count `3` and error row `connection refused`, `1500`.
- Personal API-key browser login; login help and Sign In visible at 375 by 667.
- Run example returns the actual three events; both tutorial browser queries return exact rows.
- A deliberately absent service yields the new zero-match guidance.
- With an active host filter and absolute range, Save persists only `service=tutorial`.
- Share round-trips the executed query, filter, range, and actual three results.
- Parquet export can be queried locally for the exact same count.
- Both owned daemon processes, the PostgreSQL container/volume, and its private working directory are removed.

The screenshot preceding the API-key entry contains an empty field. Other captures
were taken after authentication and contain only synthetic data. Reduced-motion
and disabled screenshot animations make the Save dialog capture stable.

## Recorded harness and limits

`block-0.sh` through `block-14.sh` were extracted from the final tutorial's Bash
fences. `run.sh` is the orchestration used in this workstation's owned worktree;
`browser.mjs` contains the Playwright checks. These files are evidence of the run,
not a new supported command. The runner expects copies at `.tmp/tutorial-smoke/`
and source binaries in the explicit host cache it names. It supervises the two
commands that a human would run in separate terminals and supplies the built SPA
through `TRAWL_WEB_SPA_DIR`.

The interactive TUI step (`block-11.sh`) was not executed. The literal cleanup
blocks (`block-13.sh` and `block-14.sh`) were also not executed; the harness trap
performed their supervised equivalents, using the owned container ID and a
checked temporary-directory prefix. The cleanup outcome is recorded above.
This run did not install
a Debian package, pull a released image, install a Helm chart, verify macOS, or
exercise a production TLS/reverse-proxy setup. Collector delivery/trust has a
separate real Vector fixture, described in the readiness ledger.

Earlier attempts are failures, not passes: the first full-app run had an ambiguous
login selector; the first tutorial extension used a server-invalid spaced saved
name; the second expected terminal table formatting on nonterminal stdout. The
ledger records their corrections and cleanup. The final report here supersedes
them only for the tested final paths.

`experiment-brief.md` is preserved from the earlier failed full-app attempt.
Its "Current run" field names `run-1789350538585-13ac35c7b9` and the planned
hold for that attempt. The passing report preserved here is the subsequent
`run-1789350917170-5c84888283`, prepared at `6b3febff`, without that hold.
