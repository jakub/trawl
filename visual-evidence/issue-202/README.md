# Disk and retention evidence (issue 202)

Captured on 2026-09-28 UTC from a clean checkout of the commit that
[`manifest.json`](manifest.json) names as `head`; the committed spec ran
unchanged. The manifest records the Git status, the spec's SHA-256, the
served bundle's SHA-256, and each capture's SHA-256. The bundle's SHA-256
is taken over the `sha256sum` listing of every file under
`crates/trawl-web-ui/dist`, sorted by relative path. The card's behaviour
follows
[ADR-0042](../../docs/adr/0042-capacity-reads-as-retention-reach-not-a-countdown.md).

Every capture is Chromium in the light system scheme with animations
disabled, clipped to the Disk and retention card on `/settings/health` as
the `health-admin` identity. The data comes from the stub harness
fixtures named below, not from a daemon. The viewport is 1440 pixels wide
unless the file name says 390; its height was grown until the whole card
fit inside the page's own scroll container.

| Capture | Fixture | What it shows |
| --- | --- | --- |
| [Complete, 1440](disk-retention-complete-1440.png) | `health-capacity-complete.json` | Two headroom rows (Data + WAL, Spill) with a floor and no deficit, and a completed sweep. Three envs are projected in whole days at both ends from one shared fraction per end: prod "about 38–52 of 90 days", staging "about 12–17 of 30 days", and lab "about 3–4 of 7 days" from its 6 observed days. Archive shows keep-forever growth, and k8s is withheld for history and named in the growth-excluded note |
| [Complete, 390](disk-retention-complete-390.png) | `health-capacity-complete.json` | The same card at a phone width. The pressure facts wrap to two columns, every sentence stays inside the card with no horizontal scroll, and no date breaks at its hyphen |
| [Failed, retained](disk-retention-failed-retained-1440.png) | `health-capacity-failed-retained.json` | The headroom attempt failed. Its rows read "Collection failed; last complete reading" with a 3600 s age and a 274 MB deficit, and the last sweep failed. All three reaches, the rate-less k8s included, are withheld as measurement unavailable with no digit, and no env is excluded from growth |
| [Repin suppressed](disk-retention-repin-suppressed-1440.png) | `health-capacity-repin-suppressed.json` | Both measurements are complete, but a repin is in flight. The last sweep reads "Suppressed", every reach is withheld because a repin holds two generations, with no digit, and no env is excluded from growth |
| [Floor zero](disk-retention-floor-zero-1440.png) | `health-capacity-floor-zero.json` | A floor of 0 reads as "Pressure deletion off (floor 0)." with no pressure attempts. Prod and lab each read that the disk fills first at the largest observed day and keeps the full policy at the mean day. A fresh env is withheld for history and named in the growth-excluded note |
| [Pressure](disk-retention-pressure-1440.png) | `health-capacity-pressure.json` | One row for all three roles with a 474 MB deficit, 12 pressure removals over 5 attempts, and a sweep that "Ran out of candidates below the floor". That sweep left each env only today's partition, so every reach is withheld for history and the finite envs lab and prod are excluded from growth |
| [Awaiting](disk-retention-awaiting-1440.png) | `health-capacity-awaiting.json` | Before the first measurement, headroom and reach both read "Awaiting measurement", with no sweep yet. The card does not read the empty environment list as "No stored date partitions yet." |
| [Scan failed](disk-retention-scan-failed-1440.png) | `health-capacity-scan-failed.json` | A Parquet scan failed with nothing retained. Headroom still shows its complete row, and reach reads "Measurement unavailable; collection failed" instead of a measured empty list |

None of the captures carries a badge, a tone class, or a reassurance
word. The spec asserts that before each capture, and also that every
reach sentence is drawn in the same ink as the Storage card's reading.

## How the spec checks each state

The `Disk and retention` cases in
[`health-page.spec.ts`](../../crates/trawl-web-ui/e2e/tests/health-page.spec.ts)
load one fixture over the base dashboard snapshot, assert the exact
sentences of each row, then capture the card. Each fixture is a slice of
the snapshot: the capacity object with the Parquet and WAL fields it was
assembled beside. The card reads the Parquet measurement to tell an
unmeasured empty list from a measured one.

Each fixture is producer output.
[`capacity_fixtures.rs`](../../crates/trawl-server/tests/capacity_fixtures.rs)
builds every scenario from fixed inputs: partition bytes per environment
and date, a headroom attempt through the server's own sampling function,
retention evidence recorded through the retention loop's own methods, and
a retention config. The server's `capacity::assemble` turns them into the
capacity object on 2026-09-27, and the test fails with a line diff when a
committed fixture differs from what it produces. The base snapshot's
capacity slice in `health-dashboard.json` is checked the same way. After a
deliberate change to the producer or a scenario, run
`TRAWL_REGEN_CAPACITY_FIXTURES=1 cargo test -p trawl-server --test capacity_fixtures`
to rewrite the fixtures that differ, then rerun the spec and recapture.
[`e2e_wire_fixture_contract.rs`](../../crates/trawl-web-ui/tests/e2e_wire_fixture_contract.rs)
decodes every fixture as the snapshot's own wire types with no field left
over, and checks that a withheld reach carries no number. It also pins
each fixture to the state its case asserts, so a drifted fixture cannot
pass the wrong case.
The spec writes a JSON sidecar per capture with its claim and SHA-256. The
manifest here is assembled from those sidecars.

## Reproduce the captures

From the repository root:

```sh
(cd crates/trawl-web-ui && env -u NO_COLOR trunk build)
(cd crates/trawl-web-ui/e2e && npm ci --no-audit --no-fund && npx playwright install chromium)
(cd crates/trawl-web-ui/e2e && env -u NO_COLOR E2E_PORT=8641 TRAWL_HEALTH_CAPTURE_DIR=/tmp/issue-202-captures \
  npx playwright test tests/health-page.spec.ts)
```

The suite starts its own harness per worker on `E2E_PORT` and up. It was
run with Node 26.8.1 and Playwright 1.62.1. The capture directory also
receives the pre-existing `health-diagnostics-*` band captures, which are
not part of this evidence.
