# Filter rail evidence (issue 231)

Captured on 2026-09-26 UTC from `c9593bf81e0bdbefa03865cbd8b62eb673a05b0f`.
The SPA was built from a clean checkout of that commit. The capture script was
still untracked when it ran and lands in the same commit as these files.
[`manifest.json`](manifest.json) records the Git status, the script's SHA-256,
the served bundle's SHA-256, and each capture's SHA-256. The rail behaviour
follows
[ADR-0044](../../docs/adr/0044-the-filter-rail-opens-for-a-countable-page.md).

Every capture is Chromium at 1440 by 900 pixels, device scale 1, reduced
motion, light system scheme, and no stored reading preferences, so the page
uses its default theme. The data comes from the stub harness fixtures, not
from a daemon.

| Capture | Scenario and route | What it shows |
| --- | --- | --- |
| [Idle strip](01-idle-strip.png) | `corpus`, `/search` | Quick start, with the rail closed to a 32px strip that reads "Filters" |
| [Countable page](02-countable-open.png) | `corpus`, `/search?q=service%3Dnginx` | 8 rows, with the rail open at 224px, the value search, and the `host`, `status`, and `message` groups |
| [Aggregation with two filters](03-aggregate-filters-strip.png) | `corpus`, `service=nginx \| stats count() by service` with the link filters `host="web-01"` and `status="200"` | The aggregate table, with the rail closed to the strip that reads "Filters · 2 active" |
| [Hand-opened empty page](04-hand-open-empty.png) | `default`, `/search?q=service%3Dnginx`, then a press on the rail's summary | 0 rows, with the rail open at 224px, "No field values to count.", and no value search |

The `corpus` scenario answers every `stats count() by` query with the same
fixture. In the third capture, the table therefore has a `status` column even
though the query groups by `service`.

## How the script checks each state

[`issue231-visual-evidence.mjs`](../../crates/trawl-web-ui/e2e/scripts/issue231-visual-evidence.mjs)
asserts each state before its screenshot and again after it. It reads the
rail's `open` attribute and its drawn width together, within 1px of 32 or 224.
It also checks that the claimed text is drawn inside its element: "Filters",
"2 active", or the empty-page hint. Selectors and copy come from
`crates/trawl-web-ui/e2e/selectors.ts`. The run also fails on a page error, an
unstubbed API call, or a query the fixture set cannot answer. The script
writes no PNG until all four scenes pass.

## Reproduce the captures

From the repository root:

```sh
(cd crates/trawl-web-ui && env -u NO_COLOR trunk build)
E2E_PORT=8731 node crates/trawl-web-ui/e2e/scripts/issue231-visual-evidence.mjs
```

The script starts its own harness on `E2E_PORT` and its own browser. It was
run with Node 26.8.1, which loads `selectors.ts` by stripping its types. Two
runs from the same build produced byte-identical PNGs.
