// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Evidence tooling, not a test: drives the built SPA against the same
// stub harness the suite uses and photographs every surface the
// 2026-09-13 redesign touched, light and dark, at 1440x1000 and
// 390x1000, then writes a manifest and a contact sheet that pairs each
// capture with the mockup it was drawn from.
//
// Same shape as capture-native-controls.mjs: spawn harness/server.mjs
// on E2E_PORT, wait for the child's OWN "listening" line, drive
// Chromium from @playwright/test.
//
// Theme is NOT a colorScheme emulation. fleet-ui reads the theme from
// localStorage through `theme::runtime`, never from
// prefers-color-scheme, so each context seeds the whole prefs payload
// with an init script before the first navigation. That is also how the
// reading-mode scenes reach `details=inspector` and
// `rows=message-first` without clicking through the View disclosure on
// every viewport.
//
// `reducedMotion: 'reduce'` everywhere, so nothing is photographed
// mid-fade and the captures are reproducible frame for frame.
//
// Usage, from the repo root (the SPA must already be built):
//   node crates/trawl-web-ui/e2e/scripts/visual-evidence.mjs [outdir]
//
// Env: E2E_PORT (default 8131), TRAWL_UI_MOCKUPS (default the study
// bundle at ~/trawl-ui-redesign-2026-09-13/mockups).

import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { chromium } from '@playwright/test';

const SCRIPT_DIR = path.dirname(fileURLToPath(import.meta.url));
const E2E_DIR = path.resolve(SCRIPT_DIR, '..');
const WEB_UI_DIR = path.resolve(E2E_DIR, '..');
const ROOT_DIR = path.resolve(WEB_UI_DIR, '..', '..');
const DIST_DIR = path.join(WEB_UI_DIR, 'dist');
const PORT = Number(process.env.E2E_PORT ?? 8131);
const BASE = `http://127.0.0.1:${PORT}`;
const STAMP = 'ui-redesign-2026-09-13';
const OUT = path.resolve(
  process.argv[2] ?? path.join(ROOT_DIR, 'visual-evidence', STAMP),
);
const MOCKUPS = path.resolve(
  process.env.TRAWL_UI_MOCKUPS ?? path.join(os.homedir(), 'trawl-ui-redesign-2026-09-13', 'mockups'),
);

const WIDE = { width: 1440, height: 1000 };
const NARROW = { width: 390, height: 1000 };

/** The filtered-search URL is settings-disposition.spec.ts's literal:
 * one include filter and an absolute range, neither produced by the
 * app's own encoder. */
const SAVE_URL = '/search?q=service%3Dnginx&page=0' +
  '&f=v1.W3sib3AiOiIrIiwiZmllbGQiOiJob3N0IiwidmFsdWUiOiJ3ZWItMDEifV0' +
  '&r=2026-01-01T00:00:00Z..now';

const RESULTS_ROW = '.results-table tbody tr';
const EXPAND = '.results-table td.exp-col button.row-stretch';

/** Wait out any CSS entrance animation on the elements named, by their
 * own `finished` promises rather than a sleep. */
async function settle(page, selectors) {
  await page.evaluate(
    async ({ selectors }) => {
      const els = selectors.flatMap((sel) => [...document.querySelectorAll(sel)]);
      await Promise.all(els.flatMap((el) => el.getAnimations().map((a) => a.finished)));
    },
    { selectors },
  );
}

/** Let every FINITE animation and transition on the page finish before
 * the shutter. Without it the Haul button is photographed mid-way
 * through its 120ms `background` transition out of the in-flight state
 * — near-white on near-white, which looks exactly like a contrast bug
 * and is not one. Infinite animations (the live dot's spin, the tail
 * pulse) never resolve, so they are excluded rather than waited on, and
 * the whole wait is capped so one wedged animation cannot hang a run. */
async function settleDocument(page) {
  await page.evaluate(async () => {
    const finite = document.getAnimations().filter((a) => {
      const timing = a.effect?.getComputedTiming?.();
      return timing && Number.isFinite(timing.iterations) && Number.isFinite(timing.endTime);
    });
    await Promise.race([
      Promise.all(finite.map((a) => a.finished.catch(() => undefined))),
      new Promise((resolve) => setTimeout(resolve, 2000)),
    ]);
  });
}

/** The scene table of spec §6.4. `prefs` are merged into the seeded
 * localStorage payload; `ready` is awaited after navigation; `act` runs
 * last, before the shutter. `narrowOnly` scenes skip the 1440 pass. */
const SCENES = [
  {
    name: 'search-raw',
    route: '/search?q=service%3Dnginx&page=0',
    scenario: 'corpus',
    ready: RESULTS_ROW,
    mockups: { a: (t) => `search-${t}-after.png`, aNarrow: (t) => `search-mobile-${t}-after.png`, b: (t) => `raw-${t}-1440.png`, bNarrow: () => 'raw-light-390.png' },
  },
  {
    name: 'search-filtered',
    route: SAVE_URL,
    scenario: 'corpus',
    ready: RESULTS_ROW,
    mockups: { a: (t) => `filtered-${t}-after.png` },
  },
  {
    name: 'search-inspector',
    route: '/search?q=service%3Dnginx&page=0',
    scenario: 'corpus',
    prefs: { details: 'inspector' },
    ready: RESULTS_ROW,
    act: async (page) => {
      await page.locator(EXPAND).nth(1).click();
      await page.locator('#search-inspector').waitFor({ state: 'visible' });
    },
  },
  {
    name: 'search-message-first',
    route: '/search?q=service%3Dnginx&page=0',
    scenario: 'corpus',
    prefs: { rows: 'message-first' },
    ready: '.results-table.msg-first tbody tr',
  },
  {
    name: 'search-aggregate',
    route: '/search?q=service%3Dnginx%20%7C%20stats%20count()%20by%20status',
    scenario: 'corpus',
    ready: '.results-table.exact tbody tr',
    mockups: { a: (t) => `aggregate-${t}-after.png`, b: () => 'aggregate-dark-1440.png' },
  },
  {
    name: 'search-live',
    route: '/search?q=service%3Dnginx&mode=live',
    scenario: 'stream-burst',
    act: async (page) => {
      await page.waitForFunction(
        () => document.querySelectorAll('.results-table tbody tr').length >= 8,
        undefined,
        { timeout: 15_000 },
      );
    },
    mockups: { b: () => 'live-dark-1440-full.png' },
  },
  {
    name: 'search-malformed',
    route: '/search?q=service%3Dnginx&f=v1.!',
    scenario: 'corpus',
    ready: '.url-notice',
  },
  {
    name: 'schema',
    route: '/search/schema?svc=nginx&stab=fields',
    scenario: 'corpus',
    ready: '.sd-drawer',
    mockups: { a: (t) => `fields-${t}-after.png`, b: () => 'schema-light-1440.png' },
  },
  {
    name: 'nets-schedule',
    route: '/jobs/nets?net=2&ntab=query',
    scenario: 'schedule',
    ready: '.sd-drawer',
    mockups: { a: (t) => `schedule-${t}-after.png`, b: () => 'schedule-light-1440.png', bNarrow: () => 'schedule-dark-390.png' },
  },
  {
    name: 'runs',
    route: '/jobs/runs?run=501&net=1',
    scenario: 'corpus',
    ready: '.list-sheet',
    mockups: { a: (t) => `run-${t}-after.png`, b: () => 'runs-dark-1440.png' },
  },
  {
    name: 'history',
    route: '/search/history',
    scenario: 'corpus',
    ready: '.history-page',
    mockups: { b: () => 'history-dark-1440.png' },
  },
  {
    name: 'health-admin',
    route: '/settings/health',
    scenario: 'health-admin',
    ready: '.health-page',
    mockups: { a: (t) => `health-${t}-after.png`, b: () => 'health-light-1440.png' },
  },
  {
    name: 'health-unavailable',
    route: '/settings/health',
    // `health-degraded` is the scenario that answers /api/v1/health with
    // harness/wire/health-unavailable.json.
    scenario: 'health-degraded',
    ready: '.health-page',
  },
  {
    name: 'login',
    route: '/login',
    scenario: 'unauth',
    ready: '.login-card',
    mockups: { a: (t) => `login-${t}-after.png`, b: () => 'login-light-1440.png' },
  },
  {
    name: 'login-error',
    route: '/login',
    scenario: 'unauth',
    ready: '.login-card',
    act: async (page) => {
      await page.getByRole('button', { name: 'Sign in', exact: true }).click();
      await page.locator('.login-card .error-banner').waitFor({ state: 'visible' });
    },
  },
  {
    name: 'not-found',
    route: '/definitely-not-a-route',
    scenario: 'default',
    ready: '.login-card h1',
  },
  {
    name: 'palette',
    route: '/search',
    scenario: 'corpus',
    ready: '.topbar button.jump',
    act: async (page) => {
      await page.locator('.topbar button.jump').click();
      await page.locator('.command-palette').waitFor({ state: 'visible' });
      await settle(page, ['.command-palette', '.command-palette-scrim']);
    },
    mockups: { a: (t) => `palette-${t}-after.png`, b: () => 'palette-dark-1440.png' },
  },
  {
    name: 'mobile-nav',
    route: '/search',
    scenario: 'corpus',
    narrowOnly: true,
    ready: '.topbar button.nav-toggle',
    act: async (page) => {
      await page.locator('.topbar button.nav-toggle').click();
      await page.locator('nav.rail.overlay').waitFor({ state: 'visible' });
      await settle(page, ['nav.rail.overlay', '.nav-scrim']);
    },
  },
];

async function shoot(browser, scene, theme, viewport) {
  const context = await browser.newContext({
    viewport,
    colorScheme: theme,
    reducedMotion: 'reduce',
    locale: 'en-US',
    timezoneId: 'UTC',
  });
  // The prefs payload fleet_ui::theme::runtime reads on boot. Seeded
  // before any script on the page runs, so the first paint is already
  // in the right theme and reading mode.
  const prefs = {
    theme,
    rowstyle: 'bordered',
    sidebar: 'expanded',
    details: 'inline',
    rows: 'compact',
    ...(scene.prefs ?? {}),
  };
  await context.addInitScript((payload) => {
    window.localStorage.setItem('trawl.ui', JSON.stringify(payload));
  }, prefs);

  await context.request.post(`${BASE}/__ctl/reset`, { data: { scenario: scene.scenario } });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (err) => errors.push(String(err)));
  await page.goto(`${BASE}${scene.route}`);

  // `theme::runtime` writes the attribute once the wasm bundle boots, so
  // this is a wait, not a read: a capture taken before it lands would be
  // the light default wearing a dark filename.
  await page
    .waitForFunction(
      (want) => document.documentElement.getAttribute('data-theme') === want,
      theme,
      { timeout: 15_000 },
    )
    .catch(async () => {
      const applied = await page.evaluate(() => document.documentElement.getAttribute('data-theme'));
      throw new Error(`visual-evidence: ${scene.name} rendered data-theme ${applied}, wanted ${theme}`);
    });
  if (scene.ready) await page.locator(scene.ready).first().waitFor({ state: 'visible' });
  if (scene.act) await scene.act(page);

  await settleDocument(page);
  const file = `${scene.name}-${theme}-${viewport.width}.png`;
  await page.screenshot({ path: path.join(OUT, file) });
  await context.close();
  if (errors.length) {
    throw new Error(`visual-evidence: ${scene.name} ${theme} ${viewport.width} raised ${errors.join('; ')}`);
  }
  return file;
}

/** Identity of the bundle under the camera: the commit plus a digest of
 * the artifacts the harness actually serves. A capture set whose dist
 * hash does not match the tree it claims is not evidence. */
async function distDigest() {
  const names = ['index.html', ...(await fs.readdir(DIST_DIR)).filter((n) => n.endsWith('.wasm'))]
    .sort();
  const hash = createHash('sha256');
  for (const name of names) {
    hash.update(name);
    hash.update(await fs.readFile(path.join(DIST_DIR, name)));
  }
  return { files: names, sha256: hash.digest('hex') };
}

/** Copy the mockup a capture is paired with into the evidence dir, so
 * the contact sheet stands on its own once published. Returns the local
 * filename, or null when the study bundle has no counterpart. */
async function adoptMockup(source, dir) {
  if (!source) return null;
  const from = path.join(MOCKUPS, dir, source);
  try {
    await fs.access(from);
  } catch {
    return null;
  }
  const to = `mockup-${dir}-${source}`;
  await fs.copyFile(from, path.join(OUT, to));
  return to;
}

function sheet(rows, manifest) {
  const cells = rows.map((row) => `
  <section class="scene">
    <h2>${row.scene} <span class="meta">${row.theme} · ${row.viewport.width}×${row.viewport.height} · ${row.scenario}</span></h2>
    <p class="route"><code>${row.route}</code></p>
    <div class="pair">
      <figure><figcaption>capture</figcaption><a href="${row.file}"><img src="${row.file}" alt="${row.scene} ${row.theme} ${row.viewport.width}"></a></figure>
      ${row.mockups.map((m) => `<figure><figcaption>${m.label}</figcaption><a href="${m.file}"><img src="${m.file}" alt="${m.label}"></a></figure>`).join('\n      ')}
    </div>
  </section>`).join('\n');
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<title>Trawl UI redesign ${STAMP} — contact sheet</title>
<style>
  :root { color-scheme: light dark; font-family: system-ui, sans-serif; }
  body { margin: 0; padding: 24px; background: Canvas; color: CanvasText; }
  h1 { font-size: 20px; margin: 0 0 4px; }
  .provenance { font: 12px/1.6 ui-monospace, monospace; opacity: .75; margin: 0 0 24px; }
  .scene { border-top: 1px solid rgb(128 128 128 / .35); padding-top: 16px; margin-top: 24px; }
  .scene h2 { font-size: 15px; margin: 0 0 2px; }
  .meta { font-weight: 400; opacity: .7; font-size: 12px; }
  .route { margin: 0 0 12px; font-size: 12px; opacity: .8; }
  .pair { display: flex; flex-wrap: wrap; gap: 16px; align-items: flex-start; }
  figure { margin: 0; max-width: 640px; }
  figcaption { font-size: 11px; text-transform: uppercase; letter-spacing: .06em; opacity: .65; margin-bottom: 4px; }
  img { max-width: 100%; height: auto; border: 1px solid rgb(128 128 128 / .35); display: block; }
</style></head>
<body>
<h1>Trawl UI redesign — ${STAMP}</h1>
<p class="provenance">commit ${manifest.commit} · dist sha256 ${manifest.dist_sha256} · port ${manifest.port} · captured ${manifest.captured_at}<br>
Left is this branch through the e2e stub harness; right is the mockup the scene was drawn from (direction-a = the refined baseline, direction-b = the composition prototype).</p>
${cells}
</body></html>
`;
}

function waitForOwnedListener(child) {
  return new Promise((resolve, reject) => {
    let settled = false;
    const done = (fn, v) => {
      if (!settled) {
        settled = true;
        fn(v);
      }
    };
    const timer = setTimeout(
      () => done(reject, new Error(`visual-evidence: stub server did not report listening on ${BASE} within 10s`)),
      10_000,
    );
    child.stdout.setEncoding('utf8');
    let buf = '';
    child.stdout.on('data', (chunk) => {
      process.stdout.write(chunk);
      buf += chunk;
      if (buf.includes(`listening on http://127.0.0.1:${PORT}`)) {
        clearTimeout(timer);
        done(resolve);
      }
    });
    child.on('error', (e) => {
      clearTimeout(timer);
      done(reject, e);
    });
    child.on('exit', (code, signal) => {
      clearTimeout(timer);
      done(reject, new Error(`visual-evidence: stub server exited before listening (code ${code}, signal ${signal}); is ${BASE} already in use?`));
    });
  });
}

const server = spawn('node', ['harness/server.mjs'], {
  cwd: E2E_DIR,
  env: { ...process.env, E2E_PORT: String(PORT) },
  stdio: ['ignore', 'pipe', 'inherit'],
});

try {
  await waitForOwnedListener(server);
  await fs.mkdir(OUT, { recursive: true });
  const dist = await distDigest();
  const commit = execFileSync('git', ['-C', ROOT_DIR, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();

  const browser = await chromium.launch();
  const rows = [];
  for (const scene of SCENES) {
    const viewports = scene.narrowOnly ? [NARROW] : [WIDE, NARROW];
    for (const viewport of viewports) {
      for (const theme of ['light', 'dark']) {
        const file = await shoot(browser, scene, theme, viewport);
        const narrow = viewport.width === NARROW.width;
        const pick = scene.mockups ?? {};
        const a = narrow ? (pick.aNarrow ?? null) : (pick.a ?? null);
        const b = narrow ? (pick.bNarrow ?? null) : (pick.b ?? null);
        const mockups = [];
        const aFile = await adoptMockup(a?.(theme), 'direction-a');
        if (aFile) mockups.push({ label: 'direction-a mockup', file: aFile });
        const bFile = await adoptMockup(b?.(theme), 'direction-b');
        if (bFile) mockups.push({ label: 'direction-b mockup', file: bFile });
        rows.push({ scene: scene.name, theme, viewport, scenario: scene.scenario, route: scene.route, file, mockups });
        console.log(`captured ${file}`);
      }
    }
  }
  await browser.close();

  const manifest = {
    stamp: STAMP,
    commit,
    dist_sha256: dist.sha256,
    dist_files: dist.files,
    port: PORT,
    captured_at: new Date().toISOString(),
    viewports: { wide: WIDE, narrow: NARROW },
    reduced_motion: 'reduce',
    theme_source: "localStorage['trawl.ui']",
    scenes: rows.map((row) => ({
      scene: row.scene,
      theme: row.theme,
      viewport: `${row.viewport.width}x${row.viewport.height}`,
      scenario: row.scenario,
      route: row.route,
      file: row.file,
      mockups: row.mockups.map((m) => m.file),
    })),
  };
  await fs.writeFile(path.join(OUT, 'manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);
  await fs.writeFile(path.join(OUT, 'contact-sheet.html'), sheet(rows, manifest));
  console.log(`\n${rows.length} captures → ${OUT}`);
} finally {
  server.kill('SIGTERM');
}
