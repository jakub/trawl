# Query error evidence (issue 233)

Captured on 2026-09-22 UTC. The product source is
`dc83596230d0d195b1dcadd53c2a9ed3dd689844`. The notice and the draft
diagnostic follow
[ADR-0039](../../docs/adr/0039-query-errors-server-verdict-on-the-sent-text-local-marks-on-the-draft.md).

## Stub suite captures

These come from
[`query-console-errors.spec.ts`](../../crates/trawl-web-ui/e2e/tests/query-console-errors.spec.ts),
run with `npx playwright test tests/query-console-errors.spec.ts` from
`crates/trawl-web-ui/e2e` against the disposable E2E server. The query route
answers with the committed bodies under `harness/wire/`. The viewport is the
suite's Desktop Chrome device, light theme.

| Capture | What it shows |
| --- | --- |
| [Events notice](events-query-error-notice.png) | The notice for `service=kubelet \| stats count( by host` under **Last 15m**, with the caret under the `h` of `host` in the text sent to the server |
| [Events page](events-query-error-page.png) | The same notice in the page, with the draft diagnostic under the editor |
| [Draft diagnostic](draft-diagnostic.png) | `Line 1:35 — found 'h', expected …` under the editor |
| [Draft diagnostic, +1 more open](draft-diagnostic-more-open.png) | `f=#a,#b`: the first error on the line, the second behind the opened disclosure |

## Real daemon captures

These come from
[`issue233-evidence.mjs`](../../scripts/app-experiment/issue233-evidence.mjs)
against a held app experiment built from the same commit:

```sh
bin/app-experiment --seed 42 --events 20 --batch-size 10 --rate 200 --hold-seconds 150
node scripts/app-experiment/issue233-evidence.mjs --run "$PWD/target/app-experiments/run-1790111575611-465b7d09a7"
```

Both exited 0. The default report says `passed`, with process, container and
secret cleanup all true. The custom [report](real-daemon/report.json) says
`passed` with browser cleanup true. Both name build record
`3212bdc83661bb18fd47ed29b015b29e4f02d5e9e28ff2878da77a57699706c6`.

For each case the real `trawld` answered 400, and its body equals the wire
fixture the stub suite fulfils the route with.

| Capture | Query | Code |
| --- | --- | --- |
| [Parse error](real-daemon/parse-error.png) | `service=kubelet \| stats count( by host` | `parse_error` |
| [Two parse errors](real-daemon/parse-errors-two.png) | `f=#a,#b` | `parse_error` |
| [Unknown function with a hint](real-daemon/validation-hint.png) | `* \| stats countt(x) by host` | `validation_error` |
| [Unknown function without a hint](real-daemon/validation-no-hint.png) | `* \| stats nosuchfunc(x) by host` | `validation_error` |

The real-daemon scenario does not exercise live mode. The browser cannot read
the body of a refused stream, so the stub suite covers the live notice.
