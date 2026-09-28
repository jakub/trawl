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
| [Complete, 1440](disk-retention-complete-1440.png) | `health-capacity-complete.json` | Two headroom rows (Data + WAL, Spill) with a floor and no deficit; a completed sweep; a projected range ("about 38–52 of 90 days"), a mixed range ("about 12 days to the full 30"), a full policy, keep-forever growth, and one env withheld for history, with the growth-excluded note |
| [Complete, 390](disk-retention-complete-390.png) | `health-capacity-complete.json` | The same card at a phone width: the pressure facts wrap to two columns, every sentence stays inside the card with no horizontal scroll, and no date breaks at its hyphen |
| [Failed, retained](disk-retention-failed-retained-1440.png) | `health-capacity-failed-retained.json` | Rows read "Collection failed; last complete reading" with a 3600 s age and a 274 MB deficit; the last sweep failed; both reaches are withheld as measurement unavailable, with no digit |
| [Withheld](disk-retention-withheld-1440.png) | `health-capacity-withheld.json` | A floor of 0 reads as "Pressure deletion off (floor 0)." with no sweep yet; the three withheld reasons each in words with no digit; a floor-0 env reads "the disk fills before retention is reached" |
| [Pressure](disk-retention-pressure-1440.png) | `health-capacity-pressure.json` | One row for all three roles with a 474 MB deficit; 12 pressure removals over 5 attempts; a sweep that "Ran out of candidates below the floor"; a shortened prod range |
| [Awaiting](disk-retention-awaiting-1440.png) | `health-capacity-awaiting.json` | Before the first measurement: headroom and reach both read "Awaiting measurement", with no sweep yet. The empty environment list is not read as "No stored date partitions yet." |
| [Scan failed](disk-retention-scan-failed-1440.png) | `health-capacity-scan-failed.json` | A Parquet scan that failed with nothing retained: headroom still shows its complete row, and reach reads "Measurement unavailable; collection failed" instead of a measured empty list |

None of the captures carries a badge, a tone class, or a reassurance
word. The spec asserts that before each capture, and also that every
reach sentence is drawn in the same ink as the Storage card's reading.

## How the spec checks each state

The `Disk and retention` cases in
[`health-page.spec.ts`](../../crates/trawl-web-ui/e2e/tests/health-page.spec.ts)
load one capacity object over the base dashboard snapshot, assert the
exact sentences of each row, then capture the card. The awaiting and
scan-failed fixtures also carry the snapshot's Parquet fields, because the
card reads the Parquet measurement to tell an unmeasured empty list from a
measured one. Each fixture is pinned
to its state natively in
[`e2e_wire_fixture_contract.rs`](../../crates/trawl-web-ui/tests/e2e_wire_fixture_contract.rs),
so a drifted fixture cannot pass the wrong case. The spec writes a JSON
sidecar per capture with its claim and SHA-256; the manifest here is
assembled from those sidecars.

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
