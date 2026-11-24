// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The dark pass is not a `colorScheme` emulation. fleet-ui's theme comes
// from localStorage through `theme::runtime`, never from
// prefers-color-scheme, so the only honest way to reach dark is the
// account menu's own theme item.
//
// Usage, from the repo root (the SPA must already be built):
//   node crates/trawl-web-ui/e2e/scripts/capture-native-controls.mjs [outdir]

import { spawn } from 'node:child_process';
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from '@playwright/test';

const SCRIPT_DIR = path.dirname(fileURLToPath(import.meta.url));
const E2E_DIR = path.resolve(SCRIPT_DIR, '..');
const ROOT_DIR = path.resolve(E2E_DIR, '..', '..', '..');
const PORT = Number(process.env.E2E_PORT ?? 8129);
const BASE = `http://127.0.0.1:${PORT}`;
const OUT = path.resolve(
  process.argv[2] ?? path.join(ROOT_DIR, 'visual-evidence', 'issue-159'),
);

/** One padded rectangle around every element named, so an absolutely-
 * positioned panel outside its wrapper's own box (the menu) is in frame. */
async function shotUnion(page, selectors, file, pad = 12) {
  const rect = await page.evaluate(
    ({ selectors }) => {
      const boxes = selectors.map((sel) => {
        const el = document.querySelector(sel);
        if (!el) throw new Error(`capture: ${sel} is not mounted`);
        return el.getBoundingClientRect();
      });
      return {
        x: Math.min(...boxes.map((b) => b.left)),
        y: Math.min(...boxes.map((b) => b.top)),
        right: Math.max(...boxes.map((b) => b.right)),
        bottom: Math.max(...boxes.map((b) => b.bottom)),
      };
    },
    { selectors },
  );
  await page.screenshot({
    path: path.join(OUT, file),
    clip: {
      x: Math.max(0, rect.x - pad),
      y: Math.max(0, rect.y - pad),
      width: rect.right - rect.x + pad * 2,
      height: rect.bottom - rect.y + pad * 2,
    },
  });
  return file;
}

/** `underlineOverlap` is the tab's border-box bottom minus the strip's
 * INNER bottom edge, the top of its 1px bottom border. Each tab carries
 * `margin-bottom: -1px`, which pulls its box down INTO that border row,
 * so the two outer bottoms coincide and the tab's 2px underline paints
 * over the hairline. 1 means the tab reaches 1px past the inner edge,
 * which is the lap; 0 or less means the underline floats above the
 * border. Measuring against the strip's OUTER bottom would read 1 only
 * if the tab hung below the strip entirely. */
async function stripGeometry(page, stripSel, tabSel) {
  return page.evaluate(
    ({ stripSel, tabSel }) => {
      const strip = document.querySelector(stripSel);
      const tablist = strip.querySelector('[role="tablist"]');
      const tabs = [...strip.querySelectorAll(tabSel)];
      const meta = strip.querySelector('.meta');
      const r = (el) => (el ? el.getBoundingClientRect() : null);
      const sb = r(strip);
      const stripBorder = parseFloat(getComputedStyle(strip).borderBottomWidth) || 0;
      const stripInnerBottom = sb.bottom - stripBorder;
      return {
        strip: {
          top: sb.top,
          bottom: sb.bottom,
          height: sb.height,
          borderBottomWidth: stripBorder,
          innerBottom: stripInnerBottom,
        },
        tablist: r(tablist),
        tabs: tabs.map((t) => {
          const b = r(t);
          return { name: t.textContent.trim(), top: b.top, bottom: b.bottom, left: b.left };
        }),
        meta: meta ? { left: r(meta).left, right: r(meta).right } : null,
        underlineOverlap: tabs.map((t) => Number((r(t).bottom - stripInnerBottom).toFixed(2))),
      };
    },
    { stripSel, tabSel },
  );
}

/** Unwrap the `[role="tablist"]` box in place and re-measure. Destructive
 * to the DOM, so it runs last, after every screenshot. */
async function flattenedGeometry(page, stripSel, tabSel) {
  await page.evaluate(
    ({ stripSel }) => {
      const list = document.querySelector(`${stripSel} [role="tablist"]`);
      if (!list) throw new Error(`capture: no tablist inside ${stripSel}`);
      list.replaceWith(...list.childNodes);
    },
    { stripSel },
  );
  return stripGeometry(page, stripSel, tabSel);
}

/** The menu, the modal and the drawer all fade in, and a frame grabbed
 * mid-fade shows the page bleeding through a half-opaque panel. */
async function settle(page, selectors) {
  await page.evaluate(
    async ({ selectors }) => {
      const els = selectors.flatMap((sel) => [...document.querySelectorAll(sel)]);
      await Promise.all(els.flatMap((el) => el.getAnimations().map((a) => a.finished)));
    },
    { selectors },
  );
}

async function openAccountMenu(page) {
  await page.locator('.topbar button.user').focus();
  await page.keyboard.press('Enter');
  await page.locator('.user-menu [role="menu"]').waitFor({ state: 'visible' });
}

async function capture(browser, theme) {
  const context = await browser.newContext({
    viewport: { width: 1440, height: 900 },
    colorScheme: theme,
    reducedMotion: 'reduce',
    locale: 'en-US',
    timezoneId: 'UTC',
    permissions: ['clipboard-read', 'clipboard-write'],
  });
  const page = await context.newPage();
  const files = [];

  await page.goto(`${BASE}/search`);
  await page.locator('.topbar button.user').waitFor({ state: 'visible' });

  if (theme === 'dark') {
    await openAccountMenu(page);
    await page.keyboard.press('Enter');
    await page.waitForFunction(
      () => document.documentElement.getAttribute('data-theme') === 'dark',
    );
  }
  const applied = await page.evaluate(() =>
    document.documentElement.getAttribute('data-theme'),
  );
  if (applied !== theme) throw new Error(`capture: data-theme is ${applied}, wanted ${theme}`);

  await openAccountMenu(page);
  await settle(page, ['.user-menu', '.user-menu *']);
  files.push(await shotUnion(page, ['.topbar .user-wrap', '.user-menu'], `account-menu-${theme}.png`));
  await page.keyboard.press('Escape');

  const wsTabs = page.locator('.tabs [role="tab"]');
  await wsTabs.nth(0).focus();
  await page.keyboard.press('ArrowRight');
  files.push(await shotUnion(page, ['.tabs'], `results-tabs-${theme}.png`, 8));
  const results = await stripGeometry(page, '.tabs', '[role="tab"]');

  // Opened from the KEYBOARD: Chromium only paints `:focus-visible` when
  // the last interaction was a keypress.
  await page.locator('.tabs .action.export').focus();
  await page.keyboard.press('Enter');
  await page.locator('.modal').waitFor({ state: 'visible' });
  await settle(page, ['.modal', '.modal *']);
  await page.locator('.modal .m-hd button.x').focus();
  files.push(await shotUnion(page, ['.modal .m-hd'], `modal-header-${theme}.png`, 8));
  await page.keyboard.press('Escape');
  await page.locator('.modal').waitFor({ state: 'detached' });

  await page.locator('.editor-tools button.tool').filter({ hasText: /^Share$/ }).focus();
  await page.keyboard.press('Enter');
  await page.locator('.toast').first().waitFor({ state: 'visible' });
  await settle(page, ['.toast', '.toast *']);
  await page.locator('.toast button.x').first().focus();
  files.push(await shotUnion(page, ['.toast'], `toast-${theme}.png`, 8));

  await context.request.post(`${BASE}/__ctl/reset`, { data: { scenario: 'populated' } });
  await page.goto(`${BASE}/search/schema?svc=nginx&stab=overview`);
  await page.locator('.sd-drawer').waitFor({ state: 'visible' });
  if (theme === 'dark') {
    await page.waitForFunction(
      () => document.documentElement.getAttribute('data-theme') === 'dark',
    );
  }
  await settle(page, ['.sd-drawer', '.sd-drawer *', '.sd-scrim']);
  const dTabs = page.locator('.sd-tabs [role="tab"]');
  await dTabs.nth(0).focus();
  await page.keyboard.press('ArrowRight');
  files.push(await shotUnion(page, ['.sd-tabs'], `drawer-tabs-${theme}.png`, 8));
  const drawer = await stripGeometry(page, '.sd-tabs', '[role="tab"]');

  const drawerFlat = await flattenedGeometry(page, '.sd-tabs', '[role="tab"]');
  await page.goto(`${BASE}/search`);
  await page.locator('.tabs [role="tablist"]').waitFor({ state: 'visible' });
  const resultsNested = await stripGeometry(page, '.tabs', '[role="tab"]');
  const resultsFlat = await flattenedGeometry(page, '.tabs', '[role="tab"]');

  await context.close();
  return {
    files,
    geometry: { results, drawer },
    ab: { resultsNested, resultsFlat, drawerNested: drawer, drawerFlat },
  };
}

// Readiness is the CHILD's own "listening" line, never a 200 from the
// port: a harness left by another worktree answers /__ctl/health just as
// happily. Same rule as the `stubOrigin` fixture in fixtures.ts.
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
      () => done(reject, new Error(`capture: stub server did not report listening on ${BASE} within 10s`)),
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
      done(reject, new Error(`capture: stub server exited before listening (code ${code}, signal ${signal}); is ${BASE} already in use?`));
    });
  });
}

async function waitForHealth() {
  for (let i = 0; i < 100; i += 1) {
    try {
      const res = await fetch(`${BASE}/__ctl/health`);
      if (res.ok) return;
    } catch {
      /* not up yet */
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`capture: stub server never became healthy on ${BASE}`);
}

const server = spawn('node', ['harness/server.mjs'], {
  cwd: E2E_DIR,
  env: { ...process.env, E2E_PORT: String(PORT) },
  stdio: ['ignore', 'pipe', 'inherit'],
});

try {
  await waitForOwnedListener(server);
  await waitForHealth();
  await fs.mkdir(OUT, { recursive: true });
  const browser = await chromium.launch();
  const report = {};
  for (const theme of ['light', 'dark']) {
    const out = await capture(browser, theme);
    report[theme] = out;
    console.log(`${theme}: ${out.files.join(', ')}`);
  }
  await browser.close();
  console.log(JSON.stringify(report, null, 2));
} finally {
  server.kill('SIGTERM');
}
