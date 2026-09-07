// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Evidence tooling, not a test: photographs every surface issue #161
// converted, at rest and with one control focused, against the same stub
// harness the suite uses and the `corpus` scenario that has data in it.
//
// A second script rather than an extension of
// `capture-native-controls.mjs`. That one is the ADR-0028 chrome, light
// and dark, with the tab-strip geometry numbers beside it; this one is a
// single light viewport over app surfaces and it has to run against TWO
// builds, the pre-#161 one included. Folding the two together would mean
// one script whose every step is conditional on which question it is
// answering.
//
// The before build is the point. `--dist` picks the `dist/` the stub
// serves, so the same steps run against a checkout that predates the
// conversion: the tables and panels are there, the controls are not, and
// a step that cannot find its control records that rather than failing.
// `focusVisible` in the report is the whole claim in one boolean per
// shot.
//
// Usage, from the repo root:
//   node crates/trawl-web-ui/e2e/scripts/capture-native-controls-161.mjs \
//     --label after --out visual-evidence/issue-161/captures/after
//   node crates/trawl-web-ui/e2e/scripts/capture-native-controls-161.mjs \
//     --label before --out visual-evidence/issue-161/captures/before \
//     --dist /tmp/trawl-161-before/crates/trawl-web-ui/dist
//
// The report goes to stdout and to `report.json` in the output dir.

import { spawn } from 'node:child_process';
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from '@playwright/test';

const SCRIPT_DIR = path.dirname(fileURLToPath(import.meta.url));
const E2E_DIR = path.resolve(SCRIPT_DIR, '..');
const ROOT_DIR = path.resolve(E2E_DIR, '..', '..', '..');

function arg(name, fallback) {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? fallback : process.argv[i + 1];
}

const LABEL = arg('label', 'after');
const OUT = path.resolve(arg('out', path.join(ROOT_DIR, 'visual-evidence', 'issue-161', 'captures', LABEL)));
const DIST = arg('dist', null);
// A spare port, away from the suite's 8123 and the #159 capture's 8129.
const PORT = Number(process.env.E2E_PORT ?? 8131);
const BASE = `http://127.0.0.1:${PORT}`;
// One viewport, one device pixel per CSS pixel. A retina pass would
// double every file for nothing: none of this is about hairlines.
const VIEWPORT = { width: 1440, height: 900 };

const SERVICE = 'nginx';
const NET = 1;
const DEGRADED_FIELD = 'duration';
// A malformed filters payload, copied from search-url.spec.ts: the
// notice it raises is where `.url-notice-repair` lives.
const MALFORMED = '/search?q=service%3Dnginx&mode=live&f=v1.!';

/** Screenshot a rectangle covering every element named, with padding, so
 * a panel that sits outside its wrapper's box is still in frame. Missing
 * selectors are skipped rather than fatal: the before build does not
 * have all of them. */
async function shotUnion(page, selectors, file, pad = 12) {
  const rect = await page.evaluate((sels) => {
    const boxes = sels
      .map((sel) => document.querySelector(sel))
      .filter(Boolean)
      .map((el) => el.getBoundingClientRect());
    if (boxes.length === 0) return null;
    return {
      x: Math.min(...boxes.map((b) => b.left)),
      y: Math.min(...boxes.map((b) => b.top)),
      right: Math.max(...boxes.map((b) => b.right)),
      bottom: Math.max(...boxes.map((b) => b.bottom)),
    };
  }, selectors);
  if (!rect) {
    await page.screenshot({ path: path.join(OUT, file) });
    return { file, clipped: false };
  }
  await page.screenshot({
    path: path.join(OUT, file),
    clip: {
      x: Math.max(0, rect.x - pad),
      y: Math.max(0, rect.y - pad),
      width: Math.min(VIEWPORT.width, rect.right - rect.x + pad * 2),
      height: Math.min(VIEWPORT.height, rect.bottom - rect.y + pad * 2),
    },
  });
  return { file, clipped: true };
}

/** Focus a control the way a keyboard user arrives at it, and report
 * whether the ring is actually painted.
 *
 * The Tab first is not decoration. Chromium paints `:focus-visible` on a
 * programmatically focused element only when focus was already visible
 * on the element it came from, so a bare `.focus()` from a fresh page
 * lands a focused control with no ring on it. One Tab into the document
 * establishes that, and the `.focus()` after it inherits.
 *
 * Returns `null` when the control is not in the page at all, which is
 * what the before build answers for every control #161 created. */
async function focusControl(page, selector) {
  await page.keyboard.press('Tab');
  const el = page.locator(selector).first();
  if ((await el.count()) === 0) return null;
  try {
    await el.focus({ timeout: 2000 });
  } catch {
    return { focused: false, focusVisible: false, boxShadow: null, tag: null };
  }
  return el.evaluate((node) => ({
    focused: document.activeElement === node,
    focusVisible: node.matches(':focus-visible'),
    boxShadow: getComputedStyle(node).boxShadow,
    // The five controls that kept an app-side `:focus-visible { outline }`
    // before #161 read a real outline here and `none` after it, which is
    // the difference a picture of a ring cannot spell out.
    outline: getComputedStyle(node).outline,
    tag: node.tagName,
    type: node.getAttribute('type'),
  }));
}

/** Wait out entrance animations by their own promises rather than a
 * sleep: drawers, dialogs and toasts all fade, and a frame grabbed
 * mid-fade shows the page through a half-opaque panel. */
async function settle(page, selectors) {
  await page.evaluate(async (sels) => {
    const els = sels.flatMap((sel) => [...document.querySelectorAll(sel)]);
    await Promise.all(els.flatMap((el) => el.getAnimations().map((a) => a.finished)));
  }, selectors);
}

async function reset(context, body) {
  await context.request.post(`${BASE}/__ctl/reset`, { data: body });
}

/** Click a control if it is there, and say whether it was. Every
 * interaction in this script is optional in exactly the same way its
 * screenshots are: the before build reaches some of these surfaces with
 * nothing to press. */
async function clickIfPresent(page, selector, timeout = 4000) {
  const el = page.locator(selector).first();
  try {
    await el.waitFor({ state: 'visible', timeout });
    await el.click({ timeout });
    return true;
  } catch {
    return false;
  }
}

/** Wait for a selector, but never fail the run over it: the before build
 * legitimately lacks some of these. */
async function present(page, selector, timeout = 4000) {
  try {
    await page.locator(selector).first().waitFor({ state: 'visible', timeout });
    return true;
  } catch {
    return false;
  }
}

async function run() {
  const browser = await chromium.launch();
  const context = await browser.newContext({
    viewport: VIEWPORT,
    colorScheme: 'light',
    locale: 'en-US',
    timezoneId: 'UTC',
  });
  // The corpus query fixture carries no `degraded_fields`, and that one
  // wire field is the only thing that renders the results notice whose
  // dismiss button (`.deg-x`) lost its outline override. Adding it at
  // the network edge keeps the fixture (and every spec reading it)
  // alone; both sides of the comparison get the same treatment.
  await context.route('**/api/v1/query', async (route) => {
    const res = await route.fetch();
    let body;
    try {
      body = await res.json();
    } catch {
      await route.fulfill({ response: res });
      return;
    }
    if (Array.isArray(body?.rows) && body.rows.length > 0) {
      body.degraded_fields = [DEGRADED_FIELD];
    }
    await route.fulfill({ response: res, body: JSON.stringify(body) });
  });
  const page = await context.newPage();
  const shots = [];

  /** One entry in the report and one PNG on disk. */
  async function shot({ name, clip, focus, note }) {
    const focused = focus ? await focusControl(page, focus) : null;
    const out = await shotUnion(page, clip, `${name}.png`);
    shots.push({
      name,
      file: out.file,
      clipped: out.clipped,
      focusSelector: focus ?? null,
      focus: focused,
      note: note ?? null,
    });
  }

  // ---- schema services table ---------------------------------------
  await reset(context, { scenario: 'corpus' });
  await page.goto(`${BASE}/search/schema`);
  await present(page, '.tbl-body .tbl-row');
  await shot({ name: 'schema-table-rest', clip: ['.tbl'] });
  await shot({
    name: 'schema-row-focused',
    clip: ['.tbl'],
    focus: '.tbl-body .tbl-row .row-stretch',
    note: 'the row link focused; `.row-act` reveals the two quick actions on :focus-within',
  });

  // ---- nets table ---------------------------------------------------
  await page.goto(`${BASE}/jobs/nets`);
  await present(page, '.tbl-body .tbl-row');
  await shot({ name: 'nets-table-rest', clip: ['.tbl'] });
  // The fixture has ONE net, so there is no second row to open a menu
  // over. The menu goes over the row it belongs to instead.
  await clickIfPresent(page, '.actions-wrap button.btn-icon');
  await present(page, '.actions-menu [role="menuitem"]');
  await settle(page, ['.actions-menu', '.actions-menu *']);
  await shot({
    name: 'nets-menu-open',
    clip: ['.tbl', '.actions-menu'],
    note: 'one net in the fixture, so the menu opens over its own row',
  });
  await page.keyboard.press('Escape');

  // ---- runs and history --------------------------------------------
  await page.goto(`${BASE}/jobs/runs`);
  await present(page, '.tbl-body .tbl-row');
  await shot({ name: 'runs-table-rest', clip: ['.tbl'] });
  await shot({ name: 'runs-row-focused', clip: ['.tbl'], focus: '.tbl-body .tbl-row .row-stretch' });

  await page.goto(`${BASE}/search/history`);
  await present(page, '.tbl-body .tbl-row');
  await shot({ name: 'history-table-rest', clip: ['.tbl'] });
  await shot({
    name: 'history-row-focused',
    clip: ['.tbl'],
    focus: '.tbl-body .tbl-row .row-stretch',
    note: 'a button, not a link: the rerun goes through the navigator, which refuses the second entry',
  });

  // ---- results table -------------------------------------------------
  await page.goto(`${BASE}/search?q=service%3D${SERVICE}&page=0`);
  await present(page, '.results-table tbody tr');
  await shot({ name: 'results-table-rest', clip: ['.results-table'] });
  await shot({
    name: 'results-row-focused',
    clip: ['.results-table'],
    focus: '.results-table td.exp-col button.row-stretch',
  });
  // Expand from the keyboard so the detail row is up in the same frame.
  await page.keyboard.press('Enter');
  if (!(await present(page, '.results-table td.detail', 2000))) {
    // The before build has no caret button, so the row's own handler is
    // the only way in.
    await clickIfPresent(page, '.results-table tbody tr');
    await present(page, '.results-table td.detail', 2000);
  }
  await shot({ name: 'results-row-expanded', clip: ['.results-table'] });
  await shot({
    name: 'results-sort-header-focused',
    clip: ['.results-table'],
    focus: '.results-table th.sortable button.th-sort',
  });

  // ---- facets --------------------------------------------------------
  if (await present(page, '.facets', 8000)) {
    // A value far longer than the row is wide, written into the DOM
    // rather than the fixture, exactly as facets.spec.ts does it: the
    // geometry is the subject and a 200-character hostname would change
    // every other picture here.
    const firstValue = page.locator('.facets .g .vals .v .n').first();
    if ((await firstValue.count()) > 0) {
      await firstValue.evaluate((el) => {
        el.textContent = 'x'.repeat(200);
      });
    }
    await shot({ name: 'facets-rest', clip: ['.facets'] });
    await shot({
      name: 'facets-include-focused',
      clip: ['.facets'],
      focus: '.facets .g .vals .v .act button.op',
      note: 'long value in the first row: the action area is out of flow with its own opaque floor',
    });
  }

  // ---- editor tools and the range dialog ------------------------------
  await page.goto(`${BASE}/search`);
  await present(page, '.dsl-editor');
  await shot({ name: 'editor-tools-rest', clip: ['.editor-tools', '.daterange'] });
  await shot({
    name: 'editor-tools-focused',
    clip: ['.editor-tools', '.daterange'],
    focus: '.editor-tools button.tool',
  });
  // Opened from the keyboard: the dialog's initial focus is the claim,
  // and a mouse-opened panel would show it without a ring.
  const trigger = await focusControl(page, '.daterange .dr-trigger');
  if (trigger?.focused) {
    await page.keyboard.press('Enter');
  } else {
    await clickIfPresent(page, '.daterange .dr-trigger');
  }
  if (await present(page, '.dr-pop')) {
    await settle(page, ['.dr-pop', '.dr-pop *']);
    await shot({
      name: 'range-dialog-open',
      clip: ['.daterange', '.dr-pop'],
      note: 'opened from the keyboard; after #161 the hook has already moved focus to the first control',
    });
    await page.keyboard.press('Escape');
  }

  // ---- status bar -----------------------------------------------------
  await shot({
    name: 'status-bar-theme-focused',
    clip: ['.statusbar'],
    focus: '.statusbar button.grp.clickable',
  });

  // ---- service drawer -------------------------------------------------
  await page.goto(`${BASE}/search/schema?svc=${SERVICE}&stab=overview`);
  await present(page, '.sd-drawer');
  await settle(page, ['.sd-drawer', '.sd-drawer *']);
  await shot({
    name: 'service-drawer-top-fields',
    clip: ['.sd-card.top'],
    focus: '.topfields .tf button.fn',
    note: 'the `.topfields` wrapper is what makes main.css style these rows at all',
  });
  await page.goto(`${BASE}/search/schema?svc=${SERVICE}&stab=fields`);
  await present(page, '.sd-fields .sf-row');
  await settle(page, ['.sd-drawer', '.sd-drawer *']);
  await shot({ name: 'service-drawer-fields-rest', clip: ['.sd-fields'] });
  await shot({
    name: 'service-drawer-field-header-focused',
    clip: ['.sd-fields'],
    focus: '.sf-hd .th.sortable button',
  });
  await shot({
    name: 'service-drawer-degraded-badge-focused',
    clip: ['.sd-fields'],
    focus: 'button.deg-btn',
    note: 'one of the five controls whose app-side outline override was deleted',
  });

  // ---- field case drawer, with a repin running on ANOTHER field -------
  // That running job is the only thing that renders `.fc-link`, and
  // arriving from the service drawer is the only thing that renders
  // `.fc-back`. Both lost their outline overrides.
  await reset(context, { scenario: 'corpus', repinField: 'status' });
  await page.goto(`${BASE}/search/schema?svc=${SERVICE}&field=${DEGRADED_FIELD}`);
  if (await present(page, '.fc-case')) {
    await settle(page, ['.sd-drawer', '.sd-drawer *']);
    await shot({ name: 'field-case-back-focused', clip: ['.sd-ttl'], focus: 'button.fc-back' });
    if (await present(page, 'button.fc-link', 4000)) {
      await shot({ name: 'field-case-link-focused', clip: ['.fc-note'], focus: 'button.fc-link' });
    } else {
      shots.push({ name: 'field-case-link-focused', file: null, note: 'no other-running note rendered' });
    }
  }

  // ---- the malformed-link notice --------------------------------------
  await reset(context, { scenario: 'corpus' });
  await page.goto(`${BASE}${MALFORMED}`);
  await present(page, '.url-notice');
  await shot({
    name: 'url-notice-repair-focused',
    clip: ['.url-notice'],
    focus: '.url-notice-repair',
    note: 'reached by a hand-written malformed link, as search-url.spec.ts does',
  });

  // ---- degraded results notice ----------------------------------------
  await page.goto(`${BASE}/search?q=service%3D${SERVICE}&page=0`);
  if (await present(page, '.deg-notice')) {
    await shot({ name: 'degraded-notice-dismiss-focused', clip: ['.deg-notice'], focus: 'button.deg-x' });
  } else {
    shots.push({ name: 'degraded-notice-dismiss-focused', file: null, note: 'no degraded notice rendered' });
  }

  // ---- net drawer ------------------------------------------------------
  await page.goto(`${BASE}/jobs/nets?net=${NET}&ntab=query`);
  await present(page, '.sd-drawer');
  await settle(page, ['.sd-drawer', '.sd-drawer *']);
  await shot({ name: 'net-drawer-rename-focused', clip: ['.sd-ttl'], focus: '.sd-ttl button.name' });
  await page.goto(`${BASE}/jobs/nets?net=${NET}&ntab=schedule`);
  await present(page, '.sd-drawer');
  await clickIfPresent(page, 'button:has-text("+ Add Schedule")');
  if (await present(page, '.interval-chips')) {
    await shot({
      name: 'net-drawer-presets-focused',
      clip: ['.interval-chips'],
      focus: '.interval-chips button.interval-chip',
    });
  }
  await page.goto(`${BASE}/jobs/nets?net=${NET}&ntab=runs`);
  await present(page, '.sd-drawer');
  await settle(page, ['.sd-drawer', '.sd-drawer *']);
  await shot({
    name: 'net-drawer-run-row-focused',
    clip: ['.sd-drawer .tbl'],
    focus: '.sd-drawer .tbl-body .row-stretch',
  });

  await context.close();
  await browser.close();
  return shots;
}

/** Readiness is the CHILD's own listening line, never a 200 from the
 * port: a harness left running by another worktree answers
 * `/__ctl/health` just as happily, and this script would then photograph
 * THAT checkout's dist while its own child had died with EADDRINUSE. */
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

const env = { ...process.env, E2E_PORT: String(PORT) };
if (DIST) env.TRAWL_E2E_DIST = path.resolve(DIST);
const server = spawn('node', ['harness/server.mjs'], {
  cwd: E2E_DIR,
  env,
  stdio: ['ignore', 'pipe', 'inherit'],
});

try {
  await waitForOwnedListener(server);
  await fs.mkdir(OUT, { recursive: true });
  const shots = await run();
  const report = {
    label: LABEL,
    dist: DIST ? path.resolve(DIST) : path.join(E2E_DIR, '..', 'dist'),
    viewport: VIEWPORT,
    scenario: 'corpus',
    shots,
  };
  await fs.writeFile(path.join(OUT, 'report.json'), `${JSON.stringify(report, null, 2)}\n`);
  console.log(JSON.stringify(report, null, 2));
  const missing = shots.filter((s) => s.focusSelector && !s.focus?.focusVisible).map((s) => s.name);
  console.log(`\n${LABEL}: ${shots.length} shots, ${missing.length} without a focus ring: ${missing.join(', ') || 'none'}`);
} finally {
  server.kill('SIGTERM');
}
