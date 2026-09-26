// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The filter rail's four wide states for issue #231 (ADR-0044), captured
// at 1440x900 against the stub harness. Build the SPA first, then run
// from the repository root:
//   (cd crates/trawl-web-ui && env -u NO_COLOR trunk build)
//   node crates/trawl-web-ui/e2e/scripts/issue231-visual-evidence.mjs [output-dir]
// E2E_PORT picks the harness port (default 8731). The script owns its
// harness process and browser, and a busy port stops it at startup.
// Selectors and copy come from ../selectors.ts, which Node loads by
// stripping its types.
//
// Each scene asserts the state it claims before its screenshot and again
// after it: the rail's `open` attribute and drawn width together, and the
// text a reader should see. The screenshots stay in memory until every
// scene has passed, so a failed run exits non-zero and writes no PNG.
// Stub fixtures only: no daemon, database, or real ingest.
import { spawn, execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { once } from 'node:events';
import fs from 'node:fs/promises';
import { constants } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium, expect } from '@playwright/test';
import { snapshotPath } from '../harness/dist-snapshot.mjs';
import { COPY, SEL } from '../selectors.ts';

const SCRIPT = fileURLToPath(import.meta.url);
const E2E = path.resolve(path.dirname(SCRIPT), '..');
const ROOT = path.resolve(E2E, '../../..');
const PORT = Number(process.env.E2E_PORT ?? 8731);
const BASE = `http://127.0.0.1:${PORT}`;
const OUT = path.resolve(process.argv[2] ?? path.join(ROOT, 'visual-evidence/issue-231'));
const VIEWPORT = { width: 1440, height: 900 };

// filter-rail.ts's OPEN_WIDTH and STRIP_WIDTH: fleet-ui's `--facets-w`
// and trawl-web-ui's `--facet-strip-w`.
const OPEN_WIDTH = 224;
const STRIP_WIDTH = 32;

// The queries of filter-rail.spec.ts. `corpus` answers the plain search
// with 8 countable rows and the aggregation with its `stats count() by`
// fixture; `default` answers every query with no rows.
const COUNTABLE_URL = '/search?q=service%3Dnginx';
const AGGREGATE = 'service=nginx | stats count() by service';
/** `host="web-01"` and `status="200"`, the two link filters of
 * live-coherence.spec.ts. */
const TWO_FILTERS = 'v1.' + Buffer.from(JSON.stringify([
  { op: '+', field: 'host', value: 'web-01' },
  { op: '+', field: 'status', value: '200' },
])).toString('base64url');

const git = (...args) => execFileSync('git', ['-C', ROOT, ...args], { encoding: 'utf8' }).trimEnd();

/** filter-rail.ts's expectRail: the `open` attribute and the drawn width,
 * read together. Returns what it read. */
async function expectRail(page, open) {
  const rail = page.locator(SEL.filterRail);
  if (open) await expect(rail).toHaveAttribute('open', '');
  else await expect(rail).not.toHaveAttribute('open');
  const want = open ? OPEN_WIDTH : STRIP_WIDTH;
  await expect
    .poll(async () => Math.abs((await rail.boundingBox()).width - want), {
      message: `the rail should be ${want}px wide`,
    })
    .toBeLessThanOrEqual(1);
  return { open, width: (await rail.boundingBox()).width };
}

/** filter-rail.ts's expectTextShown: one run of `text` inside `locator`
 * has a box of its own, inside the element's box, in a visible element. */
async function expectTextShown(locator, text) {
  await expect(locator).toBeVisible();
  await expect
    .poll(
      () =>
        locator.evaluate((el, text) => {
          const box = el.getBoundingClientRect();
          const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
          for (let node = walker.nextNode(); node; node = walker.nextNode()) {
            const at = node.textContent.indexOf(text);
            if (at < 0) continue;
            const range = document.createRange();
            range.setStart(node, at);
            range.setEnd(node, at + text.length);
            const run = range.getBoundingClientRect();
            return (
              run.width > 0 &&
              run.height > 0 &&
              run.left >= box.left - 1 &&
              run.right <= box.right + 1 &&
              run.top >= box.top - 1 &&
              run.bottom <= box.bottom + 1 &&
              node.parentElement.checkVisibility({ opacityProperty: true, visibilityProperty: true })
            );
          }
          return false;
        }, text),
      { message: `"${text}" should be drawn inside the element` },
    )
    .toBe(true);
}

/** A settled answer: the scope strip shows execution facts for an
 * accepted response only, never while a request is pending. */
async function expectAccepted(page) {
  await expect(page.locator(SEL.scopeExecution)).toBeVisible();
}

const scenes = [
  {
    file: '01-idle-strip.png',
    claim: 'Idle /search: the rail is a closed 32px strip reading "Filters"',
    scenario: 'corpus',
    route: '/search',
    async verify(page) {
      await expect(page.locator('.search-quick-start')).toBeVisible();
      const rail = await expectRail(page, false);
      await expectTextShown(page.locator(SEL.filterRailSummary), 'Filters');
      await expect(page.locator(SEL.facetFilterInput)).toBeHidden();
      return rail;
    },
  },
  {
    file: '02-countable-open.png',
    claim: 'A countable page (8 rows of service=nginx): the rail open at 224px with its field groups and value search',
    scenario: 'corpus',
    route: COUNTABLE_URL,
    async verify(page) {
      await expect(page.locator(SEL.resultsRow)).toHaveCount(8);
      await expectAccepted(page);
      const rail = await expectRail(page, true);
      await expect(page.locator(SEL.facetGroup).first()).toBeVisible();
      await expect(page.locator(SEL.facetFilterInput)).toBeVisible();
      return { ...rail, groups: await page.locator(SEL.facetGroup).count() };
    },
  },
  {
    file: '03-aggregate-filters-strip.png',
    claim: `An aggregation (${AGGREGATE}) carrying two link filters: the rail a closed 32px strip reading "Filters" and "2 active"`,
    scenario: 'corpus',
    route: `/search?q=${encodeURIComponent(AGGREGATE)}&f=${TWO_FILTERS}`,
    async verify(page) {
      await expect(page.locator(SEL.exactTable).locator('tbody tr')).toHaveCount(5);
      await expectAccepted(page);
      await expect(page.locator(SEL.filterChip)).toHaveCount(2);
      const rail = await expectRail(page, false);
      const summary = page.locator(SEL.filterRailSummary);
      await expectTextShown(summary, 'Filters');
      await expect(page.locator(SEL.facetCount)).toHaveText('2 active');
      await expectTextShown(summary, '2 active');
      return rail;
    },
  },
  {
    file: '04-hand-open-empty.png',
    claim: `A zero-row page with the rail opened by a press on its summary: "${COPY.railHintNothingToCount}" and no value search`,
    scenario: 'default',
    route: COUNTABLE_URL,
    async arrange(page) {
      await expectAccepted(page);
      await expectRail(page, false);
      await page.locator(SEL.filterRailSummary).click();
    },
    async verify(page) {
      await expectAccepted(page);
      // The table's one row is its empty-state cell.
      await expect(page.locator(SEL.resultsRow)).toHaveCount(1);
      await expect(page.locator(SEL.resultsRow).locator('td.results-empty-cell')).toHaveCount(1);
      const rail = await expectRail(page, true);
      await expect(page.locator(SEL.facetHint)).toHaveText(COPY.railHintNothingToCount);
      await expectTextShown(page.locator(SEL.facetHint), COPY.railHintNothingToCount);
      await expect(page.locator(SEL.facetFilterInput)).toHaveCount(0);
      await expect(page.locator(SEL.facetGroup)).toHaveCount(0);
      return rail;
    },
  },
];

function waitForListener(child) {
  return new Promise((resolve, reject) => {
    let output = '';
    const timer = setTimeout(() => reject(new Error(`No owned listener on ${BASE}`)), 10_000);
    child.stdout.setEncoding('utf8');
    child.stdout.on('data', chunk => {
      process.stdout.write(chunk);
      output += chunk;
      if (output.includes(`listening on ${BASE}`)) { clearTimeout(timer); resolve(); }
    });
    child.once('error', error => { clearTimeout(timer); reject(error); });
    child.once('exit', (code, signal) => {
      clearTimeout(timer);
      reject(new Error(`Harness exited: code=${code}, signal=${signal}`));
    });
  });
}

async function digestDist() {
  const dir = snapshotPath(PORT);
  const names = (await fs.readdir(dir, { recursive: true })).sort();
  const digest = createHash('sha256');
  const files = [];
  for (const name of names) {
    if (!(await fs.stat(path.join(dir, name))).isFile()) continue;
    files.push(name);
    digest.update(name);
    digest.update(await fs.readFile(path.join(dir, name)));
  }
  return { sha256: digest.digest('hex'), files };
}

async function capture(browser, scene) {
  // The default theme: no stored reading preferences, a light system
  // scheme, as the suite's playwright.config.ts sets.
  const context = await browser.newContext({
    viewport: VIEWPORT, deviceScaleFactor: 1, reducedMotion: 'reduce',
    locale: 'en-US', timezoneId: 'UTC', colorScheme: 'light',
  });
  try {
    await context.route('**/*', route => {
      const url = new URL(route.request().url());
      return url.origin === BASE || ['data:', 'blob:'].includes(url.protocol)
        ? route.continue() : route.abort();
    });
    const reset = await context.request.post(`${BASE}/__ctl/reset`, { data: { scenario: scene.scenario } });
    expect(reset.status(), `reset ${scene.scenario}`).toBe(200);
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', error => errors.push(String(error)));
    await page.goto(`${BASE}${scene.route}`);
    if (scene.arrange) await scene.arrange(page);
    const before = await scene.verify(page);
    await page.evaluate(() => document.fonts.ready);
    const png = await page.screenshot({ animations: 'disabled' });
    // Still the claimed state once the pixels are taken.
    const after = await scene.verify(page);
    expect(after, 'the state held through the screenshot').toEqual(before);
    expect(errors, 'page errors').toEqual([]);
    const state = await (await context.request.get(`${BASE}/__ctl/state`)).json();
    expect(state.unstubbed, 'unstubbed /api/* calls').toEqual([]);
    expect(state.unhandledQueries ?? [], 'queries with no fixture').toEqual([]);
    const theme = await page.evaluate(() => document.documentElement.dataset.theme);
    const stored = await page.evaluate(() => localStorage.getItem('trawl.ui'));
    return {
      png,
      record: {
        file: scene.file, claim: scene.claim, scenario: scene.scenario, route: scene.route,
        viewport: VIEWPORT, theme, stored_prefs: stored,
        rail: before, sha256: createHash('sha256').update(png).digest('hex'), bytes: png.length,
      },
    };
  } finally { await context.close(); }
}

const headBefore = git('rev-parse', 'HEAD');
const statusBefore = git('status', '--porcelain=v1');
const server = spawn(process.execPath, ['harness/server.mjs'], {
  cwd: E2E, env: { ...process.env, E2E_PORT: String(PORT) }, stdio: ['ignore', 'pipe', 'inherit'],
});
// A signal ends this process without running the `finally` below, and the
// harness has no channel to notice its parent went: pass the signal on, or
// the harness keeps E2E_PORT. Chromium exits with its closed pipe.
for (const signal of ['SIGINT', 'SIGTERM']) {
  process.once(signal, () => {
    server.kill('SIGTERM');
    process.exit(128 + constants.signals[signal]);
  });
}
let browser;
try {
  await waitForListener(server);
  const dist = await digestDist();
  browser = await chromium.launch();
  const captures = [];
  for (const scene of scenes) {
    try {
      captures.push(await capture(browser, scene));
    } catch (error) {
      console.log(`FAIL ${scene.file}: ${scene.claim}`);
      throw error;
    }
    const { rail } = captures.at(-1).record;
    console.log(`PASS ${scene.file}: open=${rail.open} width=${rail.width}px: ${scene.claim}`);
  }
  await fs.mkdir(OUT, { recursive: true });
  for (const { png, record } of captures) await fs.writeFile(path.join(OUT, record.file), png);
  const manifest = {
    evidence: 'Stub fixtures only; no daemon, database, or real ingest',
    head: git('rev-parse', 'HEAD'), head_before: headBefore,
    git_status: git('status', '--porcelain=v1'), git_status_before: statusBefore,
    script_sha256: createHash('sha256').update(await fs.readFile(SCRIPT)).digest('hex'),
    captured_at: new Date().toISOString(), port: PORT, reduced_motion: 'reduce', color_scheme: 'light',
    dist, captures: captures.map(c => c.record),
  };
  await fs.writeFile(path.join(OUT, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
  console.log(`${captures.length} captures written to ${OUT}`);
} finally {
  // Nested, so a browser that fails to close still lets the harness go.
  try {
    if (browser) await browser.close();
  } finally {
    if (server.exitCode === null && server.signalCode === null) {
      const exited = once(server, 'exit');
      server.kill('SIGTERM');
      await exited;
    }
  }
}
