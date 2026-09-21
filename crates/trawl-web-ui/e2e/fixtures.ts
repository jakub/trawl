// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Shared Playwright fixture: every spec imports `test`/`expect` from
// here instead of `@playwright/test` directly, so the reset / homelab
// -independence / pageerror / unstubbed-call contract applies uniformly
// without being re-typed per file.

import { test as base, expect } from '@playwright/test';

// Same variable playwright.config.ts and harness/server.mjs read, so the
// network guard below can never disagree with where the stub actually is.
const E2E_ORIGIN = `http://127.0.0.1:${Number(process.env.E2E_PORT ?? 8123)}`;

type PageErrors = { errors: Error[] };

// The reset / network-guard / pageerror / unstubbed contract rides an
// AUTO FIXTURE, not `test.beforeEach`. This module is loaded once per
// worker (ESM module cache), so a `test.beforeEach` written here is
// registered against whichever spec file imported it FIRST and silently
// does not run for any other file — which is exactly what happened:
// every spec but the first was running with no scenario reset, no
// homelab-independence guard and no pageerror check. An auto fixture is
// attached to the `test` object itself, so it runs for every test that
// imports it, whatever the file.
export const test = base.extend<{ pageErrors: PageErrors; contract: void }>({
  pageErrors: async ({ page }, use) => {
    const bucket: PageErrors = { errors: [] };
    page.on('pageerror', (err) => bucket.errors.push(err));
    await use(bucket);
  },

  contract: [
    async ({ page, request, pageErrors }, use) => {
      // Default scenario; a spec that needs a different one calls
      // `resetScenario(request, name)` itself at the top of the test
      // body — simpler than threading it through a hook, since
      // Playwright runs fixtures before the test body even sees its own
      // annotations.
      await request.post('/__ctl/reset', { data: { scenario: 'default' } });

      // Homelab-independence contract: abort any request whose origin
      // isn't our own stub server. `data:`/`blob:` are allowed (inline
      // fonts, blob-backed workers).
      await page.context().route('**/*', (route) => {
        const url = new URL(route.request().url());
        const ok =
          url.origin === E2E_ORIGIN ||
          url.protocol === 'data:' ||
          url.protocol === 'blob:';
        if (ok) {
          route.continue();
        } else {
          route.abort();
        }
      });

      await use();

      // console.error is deliberately NOT checked — headless-GL / wasm
      // warmup noise is expected and not a test failure. A JS exception
      // (pageerror) is a real bug and does fail the test.
      if (pageErrors.errors.length > 0) {
        throw pageErrors.errors[0];
      }
      const state = await (await request.get('/__ctl/state')).json();
      expect(state.unstubbed, `unstubbed /api/* calls: ${JSON.stringify(state.unstubbed)}`).toEqual(
        [],
      );
      // The same claim one level down. Under `corpus` a pipeline with no
      // fixture is answered with a 500 and recorded here, and the page
      // renders that as an ordinary query error — which no assertion in
      // any spec would notice. Recording it and never reading it made
      // the record decoration. A hit here is a fixture gap: add the
      // shape to harness/server.mjs, never loosen this.
      expect(
        state.unhandledQueries ?? [],
        `corpus queries with no fixture: ${JSON.stringify(state.unhandledQueries)}`,
      ).toEqual([]);
    },
    { auto: true },
  ],
});

export { expect };

/** What the `populated` scenario's corpus is called. The bodies live in
 * `harness/wire/saved-queries.json` and
 * `harness/wire/service-schema-populated.json`, and
 * `crates/trawl-web-ui/tests/e2e_wire_fixture_contract.rs` pins these
 * exact values there — a spec navigating to `?svc=nginx` against a
 * fixture that renamed the service would silently render no drawer. */
export const POPULATED = {
  /** The one service `?svc=` opens the service drawer on. */
  service: 'nginx',
  /** The one net's id, for `?net=<id>&ntab=`. */
  netId: 1,
} as const;

/** What the `corpus` scenario's data is, by CONTENT.
 *
 * `corpus` is `populated` plus rows: the same one service and one net,
 * with `/api/v1/query`, `/api/v1/history` and the runs routes answering
 * with fixtures instead of empty bodies. A spec reads a row by the value
 * in it, so every value it can assert on lives here and is pinned in
 * `crates/trawl-web-ui/tests/e2e_wire_fixture_contract.rs` against the
 * `harness/wire/` files.
 */
export const CORPUS = {
  /** Same service and net as `populated` — `corpus` only adds data. */
  service: POPULATED.service,
  netId: POPULATED.netId,
  /** The net's name, as the runs page prints it. */
  netName: 'errors by host',
  /** Rows in `wire/query-rows.json`. */
  rowCount: 8,
  /** Fixed metadata in every query wire fixture. Pagination adds offset seconds/ms. */
  execution: { startedAt: '2026-09-15T12:34:56Z', durationMs: 125 },
  /** Its columns, in wire order. */
  columns: ['_time', 'host', 'status', 'message'] as const,
  /** `host`'s distinct values, most frequent first, then alphabetical —
   * the order the facet rail computes. Six of them, one past the five a
   * group shows, so the group offers "+ 1 more". */
  hosts: ['web-01', 'cache-01', 'db-01', 'edge-01', 'web-02', 'web-03'] as const,
  /** How many values the `host` facet hides behind its more control. */
  hostsHidden: 1,
  /** The first `host` cell in wire order, and the last host
   * alphabetically: enough to tell one sort order from the other. */
  firstHost: 'web-01',
  hostLastAlphabetically: 'web-03',
  /** The two `wire/history.json` entries, newest first. */
  history: {
    /** The rerunnable one. */
    query: 'service=nginx _severity>=error last=1h',
    /** The one the navigator refuses: 32769 ASCII bytes, one over
     * `MAX_SEARCH_BYTES` (`src/search_url.rs`). Its text is
     * `host=` + 32764 `a`s, so a spec can match its prefix without
     * carrying 32 KiB of literal. */
    overBoundPrefix: 'host=aaaa',
    overBoundBytes: 32769,
  },
  /** The service column `wire/service-schema-corpus.json` names in the
   * service's `degraded_fields`, which is the only thing that renders a
   * field row's degraded badge. `populated` has no such column, so a
   * badge spec has to be on `corpus`. The name is also
   * `wire/catalog-field.json`'s field, so the case file the badge opens
   * is about the field the badge sits on. */
  degradedField: 'duration',
  /** The service's columns at the ends of a name sort, from
   * `wire/service-schema-corpus.json`: `_time`, `duration`, `status`
   * ascending. Enough to tell the drawer's default direction from its
   * opposite after the field headers moved onto the shared helper. */
  fieldFirstAlphabetically: '_time',
  fieldLastAlphabetically: 'status',
  /** The field the service drawer's overview lists first under "Top
   * fields by cardinality": the highest count in
   * `wire/query-cardinality.json`. The drawer reads that answer BY
   * POSITION, so this is the service column the fixture's first cell
   * stands for, not a name the fixture carries. */
  topCardinalityField: '_time',
  /** Runs of the net, newest first (`wire/net-runs.json`). */
  runIds: [501, 502] as const,
  /** The run whose expansion has a result body. */
  runWithResult: 501,
} as const;

/** What the `schedule` scenario's two nets are.
 *
 * `schedule` exists so the drawer's schedule form has both cases on one
 * page: a net with no schedule at all and a net whose schedule tiles.
 * The bodies live in `harness/wire/saved-queries-windowed.json`,
 * `harness/wire/schedule-net-runs.json` and
 * `harness/wire/run-result-paged.json`, and
 * `crates/trawl-web-ui/tests/e2e_wire_fixture_contract.rs` pins every
 * value below against them.
 */
export const SCHEDULE = {
  /** The net with no schedule: the `populated` net, verbatim. Its query
   * carries `last=1h`, which is the clause a window conflicts with. */
  plainNetId: 1,
  /** The net whose schedule tiles. */
  windowedNetId: 2,
  /** Its name, as the drawer titles itself. */
  windowedNetName: 'tiled error digest',
  /** Its saved window and lag, which the form opens showing. */
  window: 'since_last',
  lag: '5m',
  /** The run whose stored result is longer than one preview page. */
  pagedRunId: 503,
  /** How many rows that result carries, and the page the preview cuts
   * them into (`PREVIEW_PAGE_SIZE` in `src/components/net_drawer.rs`). */
  pagedRunRows: 45,
  previewPageSize: 20,
} as const;

/** Re-point the stub server at a non-default scenario for this test. Call
 * at the top of the test body — `beforeEach` above already reset to
 * 'default' by the time the body runs. */
export async function resetScenario(request: import('@playwright/test').APIRequestContext, name: string) {
  const response = await request.post('/__ctl/reset', { data: { scenario: name } });
  expect(response.ok(), `reset ${name}: HTTP ${response.status()}`).toBe(true);
  expect(await response.json()).toMatchObject({ ok: true, scenario: name });
}

/** Saved-query request bodies captured since the last scenario reset. */
export async function capturedSavedRequests(request: Ctl): Promise<Array<{ name: string; query: string }>> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.savedRequests;
}

type Ctl = import('@playwright/test').APIRequestContext;

/** Schedule PUT bodies captured since the last scenario reset, oldest
 * first. Each entry is `{ savedId, body }`, and `body` is exactly what
 * arrived: a dropped `window` is an ABSENT key, not a null, which is the
 * distinction the schedule spec is built on. */
export async function capturedScheduleRequests(
  request: Ctl,
): Promise<Array<{ savedId: number; body: Record<string, unknown> }>> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.scheduleRequests;
}

/** How many run-result reads the stub has served since the last reset. */
export async function runDetailReadCount(request: Ctl): Promise<number> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.runDetailReads.length;
}

/** Point the run-detail route at a run whose stored result file is gone,
 * so `GET .../runs/{id}` answers the server's 409 envelope for it — what
 * trawld does once `data_dir` is repointed out from under a recorded run
 * (issue #227). Every read of that run answers the same way. */
export async function armRunUnavailable(request: Ctl, runId: number): Promise<void> {
  const response = await request.post(`/__ctl/run-unavailable/${runId}`);
  expect(response.ok(), `arm run ${runId} unavailable: HTTP ${response.status()}`).toBe(true);
}

/** Arm the next schedule PUT to be refused: `'refuse'` answers the
 * server's own 400 envelope, `'fail'` a 500 with no envelope at all. */
export async function armScheduleRefusal(request: Ctl, kind: 'refuse' | 'fail'): Promise<void> {
  const response = await request.post(`/__ctl/schedule/${kind}`);
  expect(response.ok(), `arm ${kind}: HTTP ${response.status()}`).toBe(true);
}

/** How many `POST /api/v1/query` bodies the stub has captured since the
 * last reset. A spec that asserts a URL does NOT run reads this before
 * and after a bounded quiet wait. */
export async function capturedQueryCount(request: Ctl): Promise<number> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.queries.length;
}

/** The last captured query body, once exactly `expectedCount` have
 * arrived. Polls rather than sleeping, and throws with the bodies it did
 * see — a spec must never read a stale `at(-1)` as this navigation's
 * query. */
export async function lastCapturedQuery(request: Ctl, expectedCount: number, timeoutMs = 5_000) {
  const deadline = Date.now() + timeoutMs;
  let queries: any[] = [];
  for (;;) {
    const state = await (await request.get('/__ctl/state')).json();
    queries = state.queries;
    if (queries.length === expectedCount) return queries.at(-1);
    if (Date.now() > deadline) {
      throw new Error(
        `expected ${expectedCount} captured queries, saw ${queries.length}: ${JSON.stringify(queries)}`,
      );
    }
    await new Promise((r) => setTimeout(r, 50));
  }
}

/** How many `POST /api/v1/export` bodies the stub has captured since the
 * last reset. An export is the request that mattered most in ADR-0027's
 * review: the server reads an empty query as every row, so a link the
 * page refused must not be exportable. */
export async function capturedExportCount(request: Ctl): Promise<number> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.exports.length;
}

/** Every captured query's DSL, oldest first. */
export async function capturedQueries(request: Ctl): Promise<string[]> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.queries.map((q: { query: string }) => q.query);
}

type Pg = import('@playwright/test').Page;

/** Count `setInterval` timers the page currently holds, bucketed by
 * delay.
 *
 * This is the only oracle for a LEAKED timer. A leaked `gloo_timers`
 * `Interval` whose body runs a disposed leptos callback is network-
 * silent: the callback no-ops, so the leak issues no HTTP read and
 * throws no `pageerror`. Nothing the server or the DOM can see
 * distinguishes it from a cancelled one — only the browser's own timer
 * table does.
 *
 * Must be called BEFORE `page.goto`: `addInitScript` runs ahead of the
 * page's own scripts, which is what keeps the app from capturing the
 * native `setInterval` before the wrapper is in place. Native signatures
 * and return values are preserved, so the app cannot tell the difference.
 */
export async function trackIntervals(page: Pg): Promise<void> {
  await page.addInitScript(() => {
    // Live count per delay, and the side map that makes `clearInterval`
    // decrement the RIGHT bucket — a timer id carries no delay, so
    // without this a cleared 3000ms timer could cancel out a live 16ms
    // one and hide a leak.
    const counts = new Map<number, number>();
    const delays = new Map<number, number>();
    const start = window.setInterval.bind(window);
    const clear = window.clearInterval.bind(window);
    (window as any).__e2eIntervalCount = (delay: number) => counts.get(delay) ?? 0;
    window.setInterval = ((handler: TimerHandler, delay?: number, ...args: any[]) => {
      const id = start(handler, delay, ...args);
      // An omitted delay is 0 to the platform; bucket it as the platform
      // sees it rather than as a distinct "no delay" case.
      const key = delay ?? 0;
      delays.set(id, key);
      counts.set(key, (counts.get(key) ?? 0) + 1);
      return id;
    }) as typeof window.setInterval;
    window.clearInterval = ((id?: number) => {
      // Only a live id decrements: `clearInterval` is idempotent on the
      // platform, and a double clear must not drive a bucket negative.
      if (id !== undefined && delays.has(id)) {
        counts.set(delays.get(id)!, counts.get(delays.get(id)!)! - 1);
        delays.delete(id);
      }
      clear(id);
    }) as typeof window.clearInterval;
  });
}

/** How many live `setInterval` timers the page holds at `delay` ms. */
export async function intervalCount(page: Pg, delay: number): Promise<number> {
  return page.evaluate((ms) => {
    const read = (window as any).__e2eIntervalCount;
    if (typeof read !== 'function') {
      throw new Error('trackIntervals(page) was not installed before page.goto');
    }
    return read(ms) as number;
  }, delay);
}

/** Count every toast that was ever ADDED to the document, cumulatively.
 *
 * Cumulative rather than a DOM query at assert time, because a toast
 * dismisses itself after 4.5s (`fleet-ui/src/toast/runtime.rs`): a spec
 * that waits out a 3s poll period and then counts `.toast` elements is
 * racing that timer, and would report "no toast was raised" for a toast
 * that came and went. This counts arrivals, so a toast cannot outrun it.
 *
 * Must be called BEFORE `page.goto`, same as `trackIntervals`.
 */
export async function trackToasts(page: Pg): Promise<void> {
  await page.addInitScript(() => {
    let count = 0;
    // Identity dedupe: an added subtree can carry a `.toast` that is
    // both the added node itself and, on the next record, someone's
    // descendant. Counting it twice would turn one toast into two.
    const seen = new WeakSet<Element>();
    const tally = (node: Node) => {
      if (!(node instanceof Element)) return;
      const found: Element[] = node.matches('.toast') ? [node] : [];
      found.push(...node.querySelectorAll('.toast'));
      for (const el of found) {
        if (seen.has(el)) continue;
        seen.add(el);
        count += 1;
      }
    };
    (window as any).__e2eToastCount = () => count;
    // `document` itself, not `documentElement`: an init script runs
    // before the page's own scripts, and observing the document covers
    // the element's own insertion as well as everything under it.
    new MutationObserver((records) => {
      for (const record of records) {
        for (const node of record.addedNodes) tally(node);
      }
    }).observe(document, { childList: true, subtree: true });
  });
}

/** How many toasts have been raised since the page loaded. */
export async function toastCount(page: Pg): Promise<number> {
  return page.evaluate(() => {
    const read = (window as any).__e2eToastCount;
    if (typeof read !== 'function') {
      throw new Error('trackToasts(page) was not installed before page.goto');
    }
    return read() as number;
  });
}

/** Arm the stub's scripted repin status sequence for one field.
 *
 * The reset is the ONE door that arms it (see `harness/server.mjs`), so
 * this wraps that reset rather than adding a second control route. With
 * the script disarmed, which is every other spec, the status route
 * answers `{ job: null }`.
 */
export async function scriptRepinStatus(request: Ctl, field: string): Promise<void> {
  await request.post('/__ctl/reset', { data: { scenario: 'default', repinField: field } });
}

/** Requests to the global Runs endpoint, including the requested order. */
export async function capturedRunListRequests(request: Ctl): Promise<Array<{
  path: string; offset: number; limit: number; sort: string; dir: string;
}>> {
  return (await (await request.get('/__ctl/state')).json()).runListReads;
}
