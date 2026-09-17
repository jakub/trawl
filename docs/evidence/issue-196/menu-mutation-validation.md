# Issue 196 menu mutation validation

The root validator executed five independent menu mutations against production
source `ce9dc49e6ecd78d45e3ebd4805f0930da49ade8d` on 2026-09-17. Each target
spec failed while its routing control passed. After source restoration and a
pristine Trunk rebuild, the full ordinary Chromium suite passed 437 tests in
6.3 minutes, using one worker. These are local results, not CI results.
The subsequent documentation changes do not alter production behavior.

## Observed mutation summary

The following excerpt is from `issue-196-menu-mutations.log`. Exit 1 is the
expected target-spec failure, not the outcome of the control or final baseline.
The runner uses `topbar-menu.spec.ts` as the target and `routing.spec.ts` as the
independent control for each of these patches.

```text
mutation-check results:
patch                                outcome
08-menu-walk.patch                   PASS (target spec failed, control passed, exit 1)
09-menu-roving-tabindex.patch        PASS (target spec failed, control passed, exit 1)
10-menu-topmost-escape.patch         PASS (target spec failed, control passed, exit 1)
11-menu-restore-before-callback.patch PASS (target spec failed, control passed, exit 1)
31-menu-command-restore.patch        PASS (target spec failed, control passed, exit 1)
```

09 independently breaks radio tab stops. 11 independently removes radio
activation focus restoration. 31 removes command activation focus restoration;
the pending and failed Sign Out test checks this path separately. The two
activation sites are not mutated together, so a radio failure cannot mask an
untested command path.

## Restored full baseline

`issue-196-post-mutation-pristine-build.log` ends with a successful `wasm-dev`
build and Trunk distribution application at `2026-09-17T07:19:45.664165Z`.
The subsequent `issue-196-post-mutation-full-browser.log` begins with
`npm test` invoking `playwright test` and reports:

```text
Running 437 tests using 1 worker
...
437 passed (6.3m)
```

The omitted lines list individual passing tests. The full run includes all
ordinary theme-preference and topbar-menu cases, including the independent
command activation case. This full restored baseline replaces the earlier
provisional evidence that had only focused pre-mutation passes.

## Provenance and limits

The root executed the checks; the documentation writer read their outputs.
This committed summary retains only compiler/build outcomes and test summaries,
with local machine paths omitted. The full source logs were in the root's
temporary issue evidence directory. Their SHA-256 checksums at extraction were:

| Source log | SHA-256 |
| --- | --- |
| `issue-196-menu-mutations.log` | `1b9f36dd123529c09ff5549cfd961d22d32d7fc06a7cced622251d6c3ac69788` |
| `issue-196-post-mutation-full-browser.log` | `9daf334107b8bed248040bab1f268ad9d1dc7b1fffc282e23c4ae1147cc33d0d` |

These results establish the local mutation and restored browser baseline
outcomes. They do not establish CI success or Coastwatch compilation. The
[consumer validation record](consumer-validation.md) retains the separate
candidate/baseline failures and the pending user decision on acceptance
criterion 7. Permanent System capture publication is tracked separately.
