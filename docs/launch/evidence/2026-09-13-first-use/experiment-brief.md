# Trawl launch first-use verification

Candidate: /home/jakub/code/trawl/.worktrees/launch-readiness, integration branch.
Baseline code before this batch: 25a45018dcf5ec0bf37062caeeac6701ecb6a230.
The runner records the exact prepared tree; later integrated UI/API changes
require preparation again before being claimed verified.

Hypothesis: a fresh owned Trawl deployment can provision keys, sign in through
the browser proxy, ingest known events, return the exact expected query rows,
and retain a working session after daemon/proxy restart.

Initial built-in workload: seed 42, 1000 events, batch size 50, 200 events/sec,
one run. Correctness and cleanup matter; this is not a performance comparison.
Acceptance: runner exit 0, report status passed, every expected phase present,
exact event-ID/value oracle satisfied, no duplicates/truncation, cleanup flags
for processes/container/secrets all true.

Evidence stays under the candidate worktree's target/app-experiments and is
summarized in docs/launch/1.0-readiness.md. Never preserve private key files.

The built-in workload does not establish the literal three-event tutorial,
collector TLS, or changed login/empty/save guidance. Follow with a tutorial
scenario and bounded browser inspection after the corresponding changes are
integrated. Never reuse a saved profile, bin/dev, or an existing database.

Current run: run-1789350538585-13ac35c7b9 at 6be56fd6 plus staged
N03 config/reference contract and N01 tutorial edits. It has a supervised
600-second hold. During that hold, inspect login help, real zero-match
guidance, editor-only Save with active URL filters/range, and Share roundtrip.
The corpus dates are fixed in January 2026: Run example is expected to return
zero there. Use the separate literal three-event tutorial (current timestamps)
to require that Run example actually returns 3 events. No trace before login.
