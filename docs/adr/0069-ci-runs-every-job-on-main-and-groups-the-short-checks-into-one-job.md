# CI runs every job on main and groups the short checks into one job

status: accepted (2026-11-23). This ADR records rulings that a comment audit on 2026-11-23 moved out of `.github/workflows/ci.yml`, where they were the only record. They were already in force. None is a new decision.

`ci.yml` runs on every pull request to `main` and on every push to `main`. Its `changes` job runs `ci/changed-scopes.py`, which sets two outputs, `web_mutations` and `server_mutations`. Three jobs run only when one of them is true: the web UI mutation legs, the atmosphere mutation and the server guard mutations. Every `Swatinem/rust-cache` step saves only from a run on `main` (`save-if: github.ref == 'refs/heads/main'`), and pull requests restore those entries. A run on `main` starts about 25 jobs.

## Decision

**The short repository checks run as steps of one job, `repo-checks`.** Each of its eight checks, such as `rustfmt`, `cargo-deny` and `helm template`, does under a minute of work. As separate jobs, each would pay for its own runner, checkout and tool setup, and each would count against the account's limit on concurrent jobs. Every check step has `if: ${{ !cancelled() }}` and a five-minute `timeout-minutes`, so a check that fails or hangs does not stop the steps after it. The whole job, setup included, has a 20-minute timeout. If several checks hang, the job can reach that limit before the last checks run.
