# #265: the #235 restart probe, rerun on the hydration branch

This directory is the evidence for #265 acceptance criterion 20. The criterion asks for the #235 restart probe to show the full count at the first response after a restart.

## How it ran

- Source commit: `231ead3ff3943ed8c5e475a793afb902899eeba3`, with a clean tree for `crates/*/src`, `crates/*/Cargo.toml`, `Cargo.toml` and `Cargo.lock`.
- Build: `cargo build --release --locked -p trawl-server -p fleet-admin`, run from the repository root with a private `CARGO_TARGET_DIR`. This is the build step in `visual-evidence/issue-235/run-all.sh`.
- Date: 2026-09-27, from 22:12:22Z to 22:13:09Z.
- Commands, in the order `run-all.sh` uses, from the repository root with the same `CARGO_TARGET_DIR`:

  ```
  node visual-evidence/issue-235/harness.mjs setup
  node visual-evidence/issue-235/harness.mjs restart-probe
  node visual-evidence/issue-235/harness.mjs teardown
  ```

  `restart-probe` needs the keys and the Postgres container that `setup` creates, and `teardown` removes them. The `setup` seed is the same 520,000-event corpus as the #235 run. The probe writes to a new service, so the seed does not change what the probe counts.

`restart-probe.log` is the full stdout and stderr of the three commands. `restart-probe.json` is a copy of the `results/restart-probe.json` file that the probe wrote. The copy under `visual-evidence/issue-235/results/` was restored to its committed #235 baseline after the run.

## What the probe does

The probe sets `compaction_interval_secs = 30` and ingests 50 events for a new service. It counts them with `service=<probe> last=15m | stats count() as n`. It then sends SIGTERM to trawld, checks that a WAL file still holds the events, and starts a new trawld on the same data directory. After the restart it sends the same count query once a second until the query returns 50.

## Verdict

| | #235 baseline (`213f2425`) | This branch (`231ead3f`) |
| --- | ---: | ---: |
| Events acknowledged | 50 | 50 |
| Count before the restart | 50 | 50 |
| WAL files holding the events after SIGTERM | 1 | 1 |
| Count at the first response after the restart | 0 | **50** |
| Seconds after startup until all 50 were visible | 31 | 0 |

The first response after the restart returned 50, the full acknowledged count. The series has one point, `[0, 50]`, so no response after the restart returned fewer than 50. Criterion 20 is met.

## What this run does not isolate

The new trawld did two things before the probe's first count query:

1. Boot hydration loaded the surviving WAL into the hot buffer before the HTTPS listener started. The log shows `boot_hydration hydrated=2 events=74 overhang=false` at 22:12:41.909Z. The 74 events are the 50 probe events and 24 trawld telemetry events.
2. The first compaction pass, which ADR-0041 runs at boot, published the probe's WAL file at 22:12:42.131Z. This is the `compactionAfterRestart` entry in the JSON.

The first count query completed at 22:12:42.152Z, after that publish. So in this run the answer could have come from the published Parquet file and not from the hydrated hot buffer. The probe shows the result that criterion 20 asks for, which is the full count at the first response. It does not show which of the two boot steps produced that count. The `restart_` tests in `crates/trawl-server/tests/startup_pg.rs` (`cargo nextest run -p trawl-server --test startup_pg restart_`) cover the hydration path alone. They hold the boot pass so that the first read after a restart comes before any publish.

## Other notes

- During `setup`, the seed's ingest got four 503 responses and succeeded on retry (`retriedStatuses: {"503": 4}`). The #235 baseline seed recorded no retries. This run did not trace which check returned the 503s. The seed events do not affect the probe.
- `teardown` removed the Postgres container and the harness directory. No trawld process from this run is still running.
