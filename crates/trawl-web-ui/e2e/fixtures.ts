// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Shared Playwright fixture: every spec imports `test`/`expect` from
// here instead of `@playwright/test` directly, so the reset / homelab
// -independence / pageerror / unstubbed-call contract applies uniformly
// without being re-typed per file.

import { spawn } from 'node:child_process';
import path from 'node:path';
import { test as base, expect } from '@playwright/test';

type PageErrors = { errors: Error[] };

const STUB_START_TIMEOUT_MS = 60_000;

/** Readiness is the server's own "listening" line, not a health probe
 * alone: that line is printed from its `listen` callback, so only a
 * process that bound the port itself can print it. A stray server left on
 * the port by another worktree would answer `/__ctl/health` just as well
 * and silently serve THAT checkout's dist; here our child dies with
 * EADDRINUSE instead, and the worker fails with the child's output. */
async function startStub(port: number): Promise<{ origin: string; stop: () => Promise<void> }> {
  const origin = `http://127.0.0.1:${port}`;
  const child = spawn(process.execPath, [path.join(__dirname, 'harness', 'server.mjs')], {
    cwd: __dirname,
    env: { ...process.env, E2E_PORT: String(port) },
    // The IPC channel ties the server's lifetime to this worker; see the
    // `disconnect` handler in harness/server.mjs.
    stdio: ['ignore', 'pipe', 'pipe', 'ipc'],
  });
  const exited = new Promise<void>((resolve) => child.once('exit', () => resolve()));
  let output = '';
  const collect = (chunk: Buffer) => { output += chunk.toString(); };
  child.stdout.on('data', collect);
  child.stderr.on('data', collect);

  const stop = async () => {
    if (child.exitCode === null && child.signalCode === null) {
      child.kill('SIGTERM');
      // The server force-exits 500ms after SIGTERM on its own; SIGKILL is
      // for a process too wedged to run its handler at all.
      const killer = setTimeout(() => child.kill('SIGKILL'), 5_000);
      await exited;
      clearTimeout(killer);
    }
  };

  try {
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error(`did not start within ${STUB_START_TIMEOUT_MS}ms`)),
        STUB_START_TIMEOUT_MS,
      );
      const onExit = (code: number | null, signal: string | null) => {
        clearTimeout(timer);
        reject(new Error(`exited before listening (code ${code}, signal ${signal})`));
      };
      child.once('exit', onExit);
      child.stdout.on('data', () => {
        if (output.includes(`listening on ${origin} `)) {
          clearTimeout(timer);
          child.off('exit', onExit);
          resolve();
        }
      });
    });
    const health = await fetch(`${origin}/__ctl/health`);
    if (!health.ok) throw new Error(`/__ctl/health answered HTTP ${health.status}`);
  } catch (error) {
    await stop();
    throw new Error(
      `e2e stub server on port ${port}: ${(error as Error).message}\n` +
        `A port already in use fails here on purpose: set E2E_PORT to a free range.\n` +
        `--- server output ---\n${output}`,
    );
  }

  child.stdout.off('data', collect);
  child.stderr.off('data', collect);
  child.stdout.pipe(process.stdout, { end: false });
  child.stderr.pipe(process.stderr, { end: false });
  return { origin, stop };
}

// Those checks ride an AUTO FIXTURE, not `test.beforeEach`. This module
// is loaded once per worker (ESM module cache), so a `test.beforeEach`
// written here is registered against whichever spec file imported it
// FIRST and silently does not run for any other file.
export const test = base.extend<{ pageErrors: PageErrors; contract: void }, { stubOrigin: string }>({
  stubOrigin: [
    async ({}, use, workerInfo) => {
      const port = Number(process.env.E2E_PORT ?? 8123) + workerInfo.parallelIndex;
      const stub = await startStub(port);
      try {
        await use(stub.origin);
      } finally {
        await stub.stop();
      }
    },
    { scope: 'worker', timeout: STUB_START_TIMEOUT_MS + 10_000 },
  ],

  baseURL: async ({ stubOrigin }, use) => {
    await use(stubOrigin);
  },

  pageErrors: async ({ page }, use) => {
    const bucket: PageErrors = { errors: [] };
    page.on('pageerror', (err) => bucket.errors.push(err));
    await use(bucket);
  },

  contract: [
    async ({ page, request, pageErrors, stubOrigin }, use, testInfo) => {
      // Default scenario; a spec that needs a different one calls
      // `resetScenario(request, name)` itself at the top of the test
      // body.
      await request.post('/__ctl/reset', { data: { scenario: 'default' } });

      await page.context().route('**/*', (route) => {
        const url = new URL(route.request().url());
        const ok =
          url.origin === stubOrigin ||
          url.protocol === 'data:' ||
          url.protocol === 'blob:';
        if (ok) {
          route.continue();
        } else {
          route.abort();
        }
      });

      await use();

      // The failure screenshot is taken here, not by Playwright's `screenshot`
      // option (off in playwright.config.ts). Playwright takes that one while
      // closing the context, and a closing context no longer dispatches
      // routes, so every request the page starts then hangs on the guard
      // above.
      let failed = testInfo.status !== testInfo.expectedStatus;
      try {
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
        // Under `corpus` a pipeline with no fixture is answered with a 500,
        // which the page renders as an ordinary query error. A hit here is
        // a fixture gap: add the shape to harness/server.mjs, never loosen
        // this.
        expect(
          state.unhandledQueries ?? [],
          `corpus queries with no fixture: ${JSON.stringify(state.unhandledQueries)}`,
        ).toEqual([]);
      } catch (error) {
        failed = true;
        throw error;
      } finally {
        if (failed) {
          let index = 0;
          for (const open of page.context().pages()) {
            const path = testInfo.outputPath(`test-failed-${++index}.png`);
            if (await open.screenshot({ path, timeout: 5_000 }).then(() => true, () => false)) {
              await testInfo.attach('screenshot', { path, contentType: 'image/png' });
            }
          }
        }
      }
    },
    { auto: true },
  ],
});

export { expect };

/** `crates/trawl-web-ui/tests/e2e_wire_fixture_contract.rs` pins these
 * exact values in `harness/wire/` — a spec navigating to `?svc=nginx`
 * against a fixture that renamed the service would silently render no
 * drawer. */
export const POPULATED = {
  service: 'nginx',
  netId: 1,
} as const;

/** Every value here is pinned in
 * `crates/trawl-web-ui/tests/e2e_wire_fixture_contract.rs` against the
 * `harness/wire/` files.
 */
export const CORPUS = {
  service: POPULATED.service,
  netId: POPULATED.netId,
  netName: 'errors by host',
  /** Rows in `wire/query-rows.json`. */
  rowCount: 8,
  /** Fixed metadata in every query wire fixture. Pagination adds offset seconds/ms. */
  execution: { startedAt: '2026-09-15T12:34:56Z', durationMs: 125 },
  columns: ['_time', 'host', 'status', 'message'] as const,
  /** Most frequent first, then alphabetical. Six of them, one past the
   * five a group shows. */
  hosts: ['web-01', 'cache-01', 'db-01', 'edge-01', 'web-02', 'web-03'] as const,
  hostsHidden: 1,
  firstHost: 'web-01',
  hostLastAlphabetically: 'web-03',
  /** The two `wire/history.json` entries, newest first. */
  history: {
    query: 'service=nginx _severity>=error last=1h',
    /** The one the navigator refuses: 32769 ASCII bytes, one over
     * `MAX_SEARCH_BYTES` (`crates/trawl-search-url/src/search_url.rs`). Its text is
     * `host=` + 32764 `a`s, so a spec can match its prefix without
     * carrying 32 KiB of literal. */
    overBoundPrefix: 'host=aaaa',
    overBoundBytes: 32769,
  },
  /** The column `wire/service-schema-corpus.json` lists in
   * `degraded_fields`, the only thing that renders a field row's degraded
   * badge; `populated` lists none. Also `wire/catalog-field.json`'s field. */
  degradedField: 'duration',
  fieldFirstAlphabetically: '_time',
  fieldLastAlphabetically: 'status',
  /** The drawer reads `wire/query-cardinality.json` BY POSITION, so this
   * is the service column the fixture's first cell stands for, not a name
   * the fixture carries. */
  topCardinalityField: '_time',
  /** Newest first. */
  runIds: [501, 502] as const,
  /** This run is also the newest row of `wire/runs-all.json`. */
  runWithResult: 501,
  runWithResultRows: 3,
  runWithResultQuery: '_severity>=error last=1h | stats count() by host',
} as const;

/** `crates/trawl-web-ui/tests/e2e_wire_fixture_contract.rs` pins every
 * value below against `harness/wire/`.
 */
export const SCHEDULE = {
  /** This net's query carries `last=1h`, the clause a window conflicts
   * with. */
  plainNetId: 1,
  windowedNetId: 2,
  fixedNetId: 3,
  /** The net with a paused query-mode schedule: no window, not enabled. */
  queryModeNetId: 4,
  windowedNetName: 'tiled error digest',
  window: 'since_last',
  lag: '5m',
  pagedRunId: 503,
  pagedRunRows: 45,
  /** `PREVIEW_PAGE_SIZE` in `src/components/net_drawer.rs`. */
  previewPageSize: 20,
} as const;

/**
 * `harness/wire/saved-queries-spent.json`,
 * pinned by `crates/trawl-web-ui/tests/e2e_wire_fixture_contract.rs`.
 */
export const SPENT = {
  /** A capped net under its cap: Run now stays enabled. */
  cappedNetId: 1,
  cappedNetName: 'capped error digest',
  /** A net spent past a lowered cap. */
  spentNetId: 2,
  spentNetName: 'spent error digest',
  spentLabel: 'spent 5/3',
  /** A `max_runs = 0` net that never ran. */
  zeroNetId: 3,
  zeroNetName: 'zero-run sweep',
  zeroLabel: 'spent 0/0',
} as const;

export async function resetScenario(request: import('@playwright/test').APIRequestContext, name: string) {
  const response = await request.post('/__ctl/reset', { data: { scenario: name } });
  expect(response.ok(), `reset ${name}: HTTP ${response.status()}`).toBe(true);
  expect(await response.json()).toMatchObject({ ok: true, scenario: name });
}

export async function capturedSavedRequests(request: Ctl): Promise<Array<{ name: string; query: string }>> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.savedRequests;
}

type Ctl = import('@playwright/test').APIRequestContext;

/** `body` is exactly what arrived: a dropped `window` is an ABSENT key,
 * not a null. */
export async function capturedScheduleRequests(
  request: Ctl,
): Promise<Array<{ savedId: number; body: Record<string, unknown> }>> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.scheduleRequests;
}

export async function runDetailReadCount(request: Ctl): Promise<number> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.runDetailReads.length;
}

/** Make `GET .../runs/{runId}` answer the server's 409 envelope for a run
 * whose stored result file is gone, on every read, not just the next. */
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

export async function capturedRunRequests(request: Ctl): Promise<number[]> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.runRequests;
}

/** Arm the next Run now to answer the server's 409 envelope for an
 * empty since_last window (`harness/wire/run-refused.json`). */
export async function armRunRefusal(request: Ctl): Promise<void> {
  const response = await request.post('/__ctl/run/refuse');
  expect(response.ok(), `arm run refusal: HTTP ${response.status()}`).toBe(true);
}

export async function capturedQueryCount(request: Ctl): Promise<number> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.queries.length;
}

/** A spec must never read a stale `at(-1)` as this navigation's query. */
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

export async function capturedExportCount(request: Ctl): Promise<number> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.exports.length;
}

export async function capturedQueries(request: Ctl): Promise<string[]> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.queries.map((q: { query: string }) => q.query);
}

type Pg = import('@playwright/test').Page;

/** This is the only oracle for a LEAKED timer. A leaked `gloo_timers`
 * `Interval` whose body runs a disposed leptos callback is network-
 * silent: the callback no-ops, so the leak issues no HTTP read and
 * throws no `pageerror`.
 *
 * Must be called BEFORE `page.goto`: `addInitScript` runs ahead of the
 * page's own scripts, which is what keeps the app from capturing the
 * native `setInterval` before the wrapper is in place.
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

export async function intervalCount(page: Pg, delay: number): Promise<number> {
  return page.evaluate((ms) => {
    const read = (window as any).__e2eIntervalCount;
    if (typeof read !== 'function') {
      throw new Error('trackIntervals(page) was not installed before page.goto');
    }
    return read(ms) as number;
  }, delay);
}

/** Cumulative rather than a DOM query at assert time, because a toast
 * dismisses itself after 4.5s (`fleet-ui/src/toast/runtime.rs`).
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

export async function toastCount(page: Pg): Promise<number> {
  return page.evaluate(() => {
    const read = (window as any).__e2eToastCount;
    if (typeof read !== 'function') {
      throw new Error('trackToasts(page) was not installed before page.goto');
    }
    return read() as number;
  });
}

/** The reset is the ONE door that arms the scripted repin status
 * sequence (see `harness/server.mjs`).
 */
export async function scriptRepinStatus(request: Ctl, field: string): Promise<void> {
  await request.post('/__ctl/reset', { data: { scenario: 'default', repinField: field } });
}

export async function capturedRunListRequests(request: Ctl): Promise<Array<{
  path: string; offset: number; limit: number; sort: string; dir: string;
}>> {
  return (await (await request.get('/__ctl/state')).json()).runListReads;
}
