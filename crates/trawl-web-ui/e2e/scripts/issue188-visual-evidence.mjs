// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Reproducible stub-only captures for issue #188. Build the SPA first, then:
// E2E_PORT=8188 node crates/trawl-web-ui/e2e/scripts/issue188-visual-evidence.mjs /tmp/issue188-captures
// Owns its harness process and browser, refuses a busy port, and records
// the served bundle digest. Real-daemon evidence is a separate experiment.
import { spawn, execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { once } from 'node:events';
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { chromium } from '@playwright/test';
import { snapshotPath } from '../harness/dist-snapshot.mjs';
import { wire } from '../harness/fixtures.mjs';

const E2E = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const ROOT = path.resolve(E2E, '../../..');
const PORT = Number(process.env.E2E_PORT ?? 8188);
const BASE = `http://127.0.0.1:${PORT}`;
const OUT = path.resolve(process.argv[2] ?? path.join(ROOT, 'visual-evidence/issue188'));
const FILTERED = '/search?q=service%3Dnginx&page=0' +
  '&f=v1.W3sib3AiOiIrIiwiZmllbGQiOiJob3N0IiwidmFsdWUiOiJ3ZWItMDEifV0' +
  '&r=2026-01-01T00:00:00Z..now';
const scenes = [
  { name: 'search-filtered', scenario: 'corpus', route: FILTERED, widths: [1440, 720], ready: '.scope-started' },
  { name: 'health', scenario: 'health-admin', route: '/settings/health', widths: [1440, 720], ready: '.health-query-table tbody tr' },
  { name: 'runs-result', scenario: 'corpus', route: '/jobs/runs?run=501&net=1', widths: [390, 720, 1099, 1100, 1440], ready: '.run-detail .preview-scroll tbody tr' },
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

async function capture(browser, scene, theme, width) {
  const context = await browser.newContext({
    viewport: { width, height: 1000 }, reducedMotion: 'reduce',
    locale: 'en-US', timezoneId: 'UTC', colorScheme: theme,
  });
  try {
    await context.addInitScript(theme => localStorage.setItem('trawl.ui', JSON.stringify({
      theme, rowstyle: 'bordered', sidebar: 'expanded', details: 'inline', rows: 'compact',
    })), theme);
    await context.route('**/*', route => {
      const url = new URL(route.request().url());
      return url.origin === BASE || ['data:', 'blob:'].includes(url.protocol)
        ? route.continue() : route.abort();
    });
    const reset = await context.request.post(`${BASE}/__ctl/reset`, { data: { scenario: scene.scenario } });
    assert.equal(reset.status(), 200);
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', error => errors.push(String(error)));
    if (scene.name === 'runs-result') {
      // The committed wide fixture exercises nested scroll and preview paging.
      await page.route('**/api/v1/saved/1/runs/501', route => route.fulfill({ json: wire('run-result-wide') }));
    }
    await page.goto(`${BASE}${scene.route}`);
    await page.waitForFunction(theme => document.documentElement.dataset.theme === theme, theme);
    await page.locator(scene.ready).first().waitFor({ state: 'visible' });
    if (scene.name === 'search-filtered') {
      assert.equal(await page.locator('.scope-count').textContent(), '8 rows returned');
      assert.equal(await page.locator('.scope-execution').textContent(), 'Execution 0.125s');
      assert.equal(await page.locator('.scope-started').textContent(), 'Started 2026-09-15 12:34:56 UTC');
      assert.equal(await page.locator('.scope .chip').count(), 1);
    }
    if (scene.name === 'health') {
      await page.locator('.health-capacity').waitFor({ state: 'visible' });
      await page.locator('.health-live').waitFor({ state: 'visible' });
    }
    const geometry = await page.evaluate(() => {
      const rect = selector => {
        const el = document.querySelector(selector);
        if (!el) return null;
        const r = el.getBoundingClientRect();
        return { x: r.x, y: r.y, width: r.width, height: r.height, bottom: r.bottom,
          clientWidth: el.clientWidth, scrollWidth: el.scrollWidth };
      };
      return {
        documentWidth: document.documentElement.scrollWidth, viewportWidth: innerWidth,
        cards: rect('.health-cards'), queries: rect('.health-queries'),
        result: rect('.run-detail .data-area'), receipt: rect('.run-detail .receipt'),
        preview: rect('.run-detail .preview-scroll'),
        tabsDisplay: document.querySelector('.run-detail .sd-tabs')
          ? getComputedStyle(document.querySelector('.run-detail .sd-tabs')).display : null,
        docked: Boolean(document.querySelector('.sd-host .run-detail.sd-docked')),
      };
    });
    assert.ok(geometry.documentWidth <= width + 1, `Document overflows: ${JSON.stringify(geometry)}`);
    if (scene.name === 'health') {
      assert.ok(geometry.queries.y >= geometry.cards.bottom - 1);
      assert.ok(Math.abs(geometry.queries.width - geometry.cards.width) <= 1);
    }
    if (scene.name === 'runs-result') {
      assert.equal(geometry.docked, width >= 1100);
      assert.equal(geometry.tabsDisplay, 'none');
      assert.ok(geometry.receipt.y >= geometry.result.bottom - 1);
      assert.ok(geometry.preview.scrollWidth > geometry.preview.clientWidth);
    }
    await page.evaluate(async () => {
      const animations = document.getAnimations().filter(a => Number.isFinite(a.effect?.getComputedTiming().endTime));
      await Promise.all(animations.map(a => a.finished.catch(() => undefined)));
      await document.fonts.ready;
    });
    const file = `${scene.name}-${theme}-${width}.png`;
    await page.screenshot({ path: path.join(OUT, file) });
    let detailFile = null;
    const detailSelector = scene.name === 'runs-result' ? '.run-detail .receipt'
      : scene.name === 'health' && width < 1100 ? '.health-queries' : null;
    if (detailSelector) {
      // Nested app scroll regions are not expanded by fullPage screenshots.
      // Keep the initial composition and a second view of the content below it.
      await page.locator(detailSelector).scrollIntoViewIfNeeded();
      detailFile = `${scene.name}-${theme}-${width}-details.png`;
      await page.screenshot({ path: path.join(OUT, detailFile) });
    }
    assert.deepEqual(errors, []);
    const state = await (await context.request.get(`${BASE}/__ctl/state`)).json();
    assert.deepEqual(state.unstubbed, []);
    assert.deepEqual(state.unhandledQueries, []);
    return { scene: scene.name, scenario: scene.scenario, route: scene.route, theme, width, height: 1000,
      fixture: scene.name === 'runs-result' ? 'run-result-wide.json' : null, file, detailFile, geometry };
  } finally { await context.close(); }
}

const server = spawn(process.execPath, ['harness/server.mjs'], {
  cwd: E2E, env: { ...process.env, E2E_PORT: String(PORT) }, stdio: ['ignore', 'pipe', 'inherit'],
});
let browser;
try {
  await waitForListener(server);
  await fs.mkdir(OUT, { recursive: true });
  const dist = await digestDist();
  browser = await chromium.launch();
  const captures = [];
  for (const scene of scenes) for (const width of scene.widths) for (const theme of ['light', 'dark']) {
    const item = await capture(browser, scene, theme, width);
    captures.push(item);
    console.log(`captured ${item.file}`);
  }
  const manifest = {
    evidence: 'Stub fixtures only; no daemon, database, or server timing claim',
    commit: execFileSync('git', ['-C', ROOT, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(),
    dirty: execFileSync('git', ['-C', ROOT, 'status', '--porcelain'], { encoding: 'utf8' }).trim(),
    captured_at: new Date().toISOString(), port: PORT, dist, captures,
  };
  await fs.writeFile(path.join(OUT, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
  const escapeHtml = text => text.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
  const sections = await Promise.all(captures.map(async c => {
    const png = await fs.readFile(path.join(OUT, c.file));
    const detail = c.detailFile ? await fs.readFile(path.join(OUT, c.detailFile)) : null;
    return `<section><h2>${c.scene}, ${c.theme}, ${c.width}px</h2><img src="data:image/png;base64,${png.toString('base64')}" alt="${c.scene} at ${c.width}px in ${c.theme} theme">
      ${detail ? `<h3>Scrolled to ${c.scene === 'health' ? 'Queries' : 'the execution receipt'}</h3><img src="data:image/png;base64,${detail.toString('base64')}" alt="${c.scene} lower content at ${c.width}px in ${c.theme} theme">` : ''}</section>`;
  }));
  await fs.writeFile(path.join(OUT, 'index.html'), `<!doctype html><html lang="en"><meta charset="utf-8">
    <title>Trawl issue 188 visual evidence</title><style>body{font:16px system-ui;margin:24px;background:#16191d;color:#e7e9ed}section{margin-block:32px}img{max-width:100%;border:1px solid #777}a{color:#a6d5fc}</style>
    <h1>Search, Health, and Runs</h1><p>Stub fixtures. Commit ${manifest.commit}.</p>
    <details><summary>Capture manifest and geometry</summary><pre>${escapeHtml(JSON.stringify(manifest, null, 2))}</pre></details>
    ${sections.join('\n')}</html>`);
  console.log(`${captures.length} captures written to ${OUT}`);
} finally {
  if (browser) await browser.close();
  if (server.exitCode === null && server.signalCode === null) {
    const exited = once(server, 'exit');
    server.kill('SIGTERM');
    await exited;
  }
}
