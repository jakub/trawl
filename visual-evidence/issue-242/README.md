# Issue #242: drawer dialog names fail on main

This directory shows that the new drawer-name assertions fail against
`main`. The SPA under test is a release `trunk build` of `656e7622`, the
`origin/main` when this branch started. It was built from the unedited
checkout before any change. The specs come from branch commit
`93c96a4a`. The browser is Chromium from `@playwright/test` 1.62.1,
served by the e2e stub harness under the `corpus` scenario.

## Files

- `negative-leg-main.txt`: the list-reporter transcript of
  `drawer-names.spec.ts` and the tightened `batch1-controls.spec.ts:31`
  run against that build. Its header records the command. All 10 tests
  fail. The transcript ends with the dialog name that each failure's ARIA
  snapshot recorded.
- `probe-main-names.sh` and `probe-main-names.txt`: the rename and
  deleted-net legs fail at their first assertion, so the transcript never
  reaches their later steps. The probe runs a throwaway copy of the spec.
  In that copy, each named-dialog assertion logs the page's dialog names
  and does not assert. The probe then deletes the copy. The `.txt` file
  is the probe's output against the same build.

## Names on main

| Leg | Expected | Name on main |
|---|---|---|
| net, viewing | `errors by host` | `Rename errors by host` |
| net, rename typed | `errors by host` | `errors by service` (the edit buffer) |
| net, rename landed | `errors by service` | `Rename errors by service` |
| net, deleted | `Deleted net` | `Rename deleted net` |
| run 501 | `Run 501, errors by host` | `errors by host Succeeded` |
| run 9999, net unknown | `Run 9999` | `Run 9999 Succeeded` |
| field case from nginx | `duration` | `Back to nginx duration` |
| service drawer | `nginx` | `nginx 2026-09-01` |
| event inspector | `Event 2` | `Event 2 2026-09-01T10:01:00.000000Z` |

On main, every leg gives a name other than the expected one. The issue
records the run badge as `success`. The badge's visible text in this
build is `Succeeded`.
