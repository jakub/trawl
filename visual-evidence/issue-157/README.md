# Issue #157: the field case drawer's repin poll dies with the drawer

Evidence for the e2e coverage added in this branch, plus the mutation
runs that prove the specs actually catch a regression rather than passing
by accident.

Everything here was captured at commit
`a7d1216a36b56a918428cc257a5dbbf05b31af26`, the head of the mutation
checkpoint. This commit is that one's child, so the pinned SHA is not the
branch tip and is not meant to be. Pinning the tip would mean pinning a
commit that did not exist when the runs happened.

## Reading a mutation verdict

`mutation-check.sh` prints `PASS` when the MUTANT WAS KILLED. It is the
runner's own verdict on itself, not a test result, so a table full of
`PASS` lines is a table full of dead mutants. That wording predates this
issue and is left alone here.

A kill needs both halves, in the same run:

- the target spec EXECUTED and FAILED, and
- a control spec the mutation does not touch PASSED.

Exit codes cannot make that call. Playwright records a browser launch
failure as executed-and-failed tests rather than as no tests, so a broken
environment looks exactly like a kill from the target spec alone. The
control is what separates them. Every transcript below shows both.

The runner also refuses to start against a dirty tree and exits 2 if the
tree is not clean after it reverts. A transcript that gets past its first
line and ends with a results table therefore carries its own proof that
nothing was stranded in the working tree.

## Files

`e2e-suite-transcript.txt` is a full `cargo xtask e2e` at the pinned SHA,
SPA rebuilt from source rather than `--skip-build`. 26 tests, all green,
four of them the new teardown specs. The capture opens with
`git rev-parse HEAD`, an empty `git status --porcelain`, and the command
line.

`mutation-06-transcript.txt` leaks the drawer's poll `Interval` in
`on_cleanup`. All four teardown tests fail on the interval-count
assertion. That assertion is the only thing in the suite that can see
this mutation: the leaked timer's body runs a disposed leptos callback
that no-ops, so the leak issues no HTTP read and throws no `pageerror`,
and both the server's request count and the DOM report it as clean.

`mutation-07-transcript.txt` leaves the timer alone and makes the
drawer's `is_alive` latch always answer true. The status read the stub
parked open across the teardown gets released afterwards, reaches a dead
surface, and announces a finished repin to nobody. All four tests fail on
the cumulative toast count, a different assertion from the one 06 trips.
Two mutations, two mechanisms, two observables.

`mutation-03-five-runs-transcript.txt` is the live-tail patch, rewritten
in this branch to mutate both places the stream handle is dropped rather
than only `on_cleanup`. Navigating away flips the URL-derived mode and
query signals, so the mode/query effect can empty the slot before
disposal ever reaches `on_cleanup`, and the one-site mutant survived on
that ordering rather than on anything about the suite. Five consecutive
invocations, each its own run, all five killed. That is the residual
#156 carried, now retired.

## Reproducing

```sh
cargo xtask e2e
crates/trawl-web-ui/e2e/scripts/mutation-check.sh 06-repin-poll-leak.patch
crates/trawl-web-ui/e2e/scripts/mutation-check.sh 07-repin-alive-latch.patch
crates/trawl-web-ui/e2e/scripts/mutation-check.sh 03-sse-teardown.patch
```

`crates/trawl-web-ui/e2e/README.md` has the full table and the mechanism
notes.
