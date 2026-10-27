// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The person's half of the #311 two-principal loop: Chromium on an iPhone
// 13 viewport, signed in through trawl-web with the writer key, works the
// investigation agent.py created while agent.py's one `trawl mcp` process
// stays open. The two hand over through files in <signals> (agent.py's
// module doc lists them). In order:
//
// 1. AC1: the board is open and scrolled so the agent's finding starts at
//    the top edge of the page's scroller, where the board's own rule takes
//    it as the element that holds the reader's place; the agent then runs
//    an attached search, which adds a line to the activity above the
//    finding and a step below it. The new step must arrive within 10 s
//    (two polls) and the finding must not move (±1 px). READ_AT in the
//    environment reads the finding that many px lower instead.
// 2. AC2: the person opens the agent's first result in the evidence viewer
//    at the first 502 (row 9), keeps it with a reason, and tells the agent.
// 3. AC3: Search with an absolute range over the seed is sent to the
//    investigation, then the board shows the person's step and its viewer
//    pages the rows the Search table showed. Then a quick range (r=1h) is
//    pinned, run again and sent.
// 4. AC4: the read-only person's session shows the board, a viewer row and
//    Search's trailing actions without Keep, Close, Reopen or Send.
//
// Usage: node board.mjs <run-dir> <out-dir> <video-dir>
//
// Signs in on a context that records nothing, keeps the session's storage
// state in <run-dir>/private, and records the working contexts' video into
// <video-dir>. Writes <out-dir>/board.jsonl (every observation, request
// body and assertion, each with the UTC instant), the PNG captures,
// writer-aria.txt and reader-aria.txt. Exits non-zero on any failed
// assertion.

import { createRequire } from 'node:module';
import { appendFileSync, existsSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const [runDir, outDir, videoDir] = process.argv.slice(2);
const here = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(path.join(here, '../../../crates/trawl-web-ui/e2e/package.json'));
const { chromium, devices, expect } = require('@playwright/test');

const ORIGIN = 'http://127.0.0.1:18461';
const SIGNALS = path.join(runDir, 'signals');
const LOG = path.join(outDir, 'board.jsonl');
const WAIT_MS = 600_000;
const READ_AT = Number(process.env.READ_AT ?? 0);
writeFileSync(LOG, '');

const { defaultBrowserType: _webkit, ...iphone } = devices['iPhone 13'];
const contextOptions = { ...iphone, locale: 'en-US', timezoneId: 'UTC', reducedMotion: 'reduce' };

/** Every key this run typed, so no failure message can carry one out of `private/`. */
const typedKeys = [];

/** `message` with every typed key replaced. */
function redact(message) {
  return typedKeys.reduce((text, key) => text.split(key).join('[key]'), String(message));
}

function record(event, body = {}) {
  appendFileSync(LOG, `${redact(JSON.stringify({ at: new Date().toISOString(), event, ...body }))}\n`);
  console.log(event, redact(JSON.stringify(body)).slice(0, 400));
}

function signal(name, body = {}) {
  const file = path.join(SIGNALS, name);
  writeFileSync(`${file}.tmp`, redact(JSON.stringify({ at: new Date().toISOString(), ...body })));
  renameSync(`${file}.tmp`, file);
}

async function waitFor(name) {
  const file = path.join(SIGNALS, name);
  const deadline = Date.now() + WAIT_MS;
  while (!existsSync(file)) {
    if (existsSync(path.join(SIGNALS, 'agent-failed.json'))) throw new Error(`the agent failed while the board waited for ${name}`);
    if (Date.now() > deadline) throw new Error(`no ${name} from the agent within ${WAIT_MS} ms`);
    await new Promise((r) => setTimeout(r, 200));
  }
  const body = JSON.parse(readFileSync(file, 'utf8'));
  record(`waited for ${name}`, body);
  return body;
}

/**
 * Sign in on a context that records nothing; return its storage state's path.
 * Playwright puts a `fill()` argument in its error message, so a failed
 * sign-in throws a message of its own and drops Playwright's.
 */
async function signIn(browser, keyFile, name) {
  const context = await browser.newContext(contextOptions);
  const page = await context.newPage();
  await page.goto(`${ORIGIN}/login`);
  const key = readFileSync(keyFile, 'utf8').trim();
  typedKeys.push(key);
  try {
    await page.getByLabel('API key', { exact: true }).fill(key);
    await page.getByRole('button', { name: 'Sign in', exact: true }).click();
    await page.waitForURL('**/search');
  } catch {
    await context.close();
    throw new Error(`the ${name} key did not sign in`);
  }
  const state = path.join(runDir, 'private', `${name}-state.json`);
  await context.storageState({ path: state });
  await context.close();
  return state;
}

async function workingPage(browser, state, label) {
  const context = await browser.newContext({ ...contextOptions, storageState: state, recordVideo: { dir: videoDir, size: iphone.viewport } });
  const page = await context.newPage();
  page.setDefaultTimeout(15_000);
  page.on('pageerror', (e) => record('pageerror', { label, message: e.message }));
  return { context, page };
}

/** The rows a results table shows, cell by cell, without its controls. */
async function tableRows(rows) {
  return rows.evaluateAll((trs) => trs.map((tr) =>
    Array.from(tr.querySelectorAll('td:not(.exp-col):not(.inv-keep-col)'), (td) => (td.textContent ?? '').trim())));
}

/** A results table's header cells, with their classes. */
async function headers(table) {
  return table.locator('thead th').evaluateAll((ths) => ths.map((th) => [(th.textContent ?? '').trim(), th.className]));
}

const searchRows = (page) => page.locator('#search-results table.results-table > tbody > tr:not(:has(td.detail))');

/** Every row the open evidence viewer pages, from its first page on. */
async function viewerRows(viewer, short) {
  const rows = viewer.getByRole('region', { name: `Rows of result ${short}` });
  const all = [];
  const pages = [];
  let head;
  for (;;) {
    const summary = (await viewer.locator('.results-summary').textContent()).trim();
    pages.push(summary);
    if (pages.length === 1) head = await headers(rows);
    all.push(...await tableRows(rows.locator('tbody tr')));
    const next = viewer.getByRole('button', { name: 'Next →' });
    if (await next.count() === 0 || await next.isDisabled()) break;
    await next.click();
    await expect(viewer.locator('.results-summary')).not.toHaveText(summary);
  }
  return { rows: all, pages, headers: head };
}

function deployTime() {
  for (const line of readFileSync(path.join(runDir, 'seed.ndjson'), 'utf8').split('\n')) {
    if (line && JSON.parse(line).message === 'deploy 4812 started') return new Date(JSON.parse(line)._time);
  }
  throw new Error('the seed has no deploy');
}

const second = (d) => d.toISOString().replace(/\.\d{3}Z$/, 'Z');

/** Record each query Search runs, each capture it posts and each keep. */
function watchWrites(page) {
  page.on('response', async (response) => {
    const url = new URL(response.url());
    const method = response.request().method();
    if (method !== 'POST') return;
    try {
      if (url.pathname === '/api/v1/query') {
        const body = await response.json();
        record('search query', { query: response.request().postDataJSON().query, status: response.status(), execution: body.execution, returned: body.pagination?.returned });
      } else if (url.pathname === '/api/v1/results') {
        record('capture', { request: response.request().postDataJSON(), status: response.status(), response: await response.json() });
      } else if (/^\/api\/v1\/investigations\/[0-9a-f]{32}\/entries$/.test(url.pathname)) {
        record('entry write', { request: response.request().postDataJSON(), status: response.status(), response: await response.json() });
      }
    } catch (e) {
      record('response unread', { path: url.pathname, message: e.message });
    }
  });
}

const browser = await chromium.launch({ headless: true, channel: 'chromium' });
let failed = false;
try {
  const created = await waitFor('investigation.json');
  const id = created.id;
  const r1 = created.first_result;
  const title = '502s on checkout';
  const writerState = await signIn(browser, path.join(runDir, 'private', 'writer-key'), 'writer');
  const { context, page } = await workingPage(browser, writerState, 'writer');
  watchWrites(page);

  // 1. AC1: the board grows while the person reads it.
  await page.goto(`${ORIGIN}/investigations/${id}`);
  const main = page.locator('.inv-page');
  const live = main.locator('.inv-live');
  const steps = main.getByRole('list', { name: 'Steps' }).locator('> li');
  const findings = main.getByRole('list', { name: 'Findings' }).locator('> li');
  const activity = main.getByRole('list', { name: 'Activity' }).locator('> li');
  await expect(main.getByText('Every entry is loaded.')).toBeVisible();
  await expect(live).toContainText('Live');
  await expect(steps).toHaveCount(2);
  await expect(findings.nth(0).locator('.inv-row-scroll')).toHaveCount(2);
  const reading = findings.nth(0);
  // Where the reader is, measured as the page measures it: the keyed
  // elements on screen with their tops below the scroller's top edge, and
  // the one that holds the reader's place by the board's own rule
  // (`pick_anchor` in crates/trawl-web-ui/src/investigation_live.rs): the
  // first in document order that still shows below the edge, passing over
  // a section that starts above the edge and holds entries.
  const place = () => reading.evaluate((el) => {
    const scroller = el.closest('.inv-page');
    const line = scroller.getBoundingClientRect().top;
    const keyed = Array.from(scroller.querySelectorAll('[data-anchor]'));
    const placed = keyed.map((k, i) => {
      const r = k.getBoundingClientRect();
      return { key: k.dataset.anchor, top: r.top - line, bottom: r.bottom - line, holdsNext: i + 1 < keyed.length && k.contains(keyed[i + 1]) };
    });
    const anchor = placed.find((p) => p.bottom > p.top && p.bottom > 0 && !(p.holdsNext && p.top < 0));
    return {
      reading: el.dataset.anchor,
      offset: el.getBoundingClientRect().top - line,
      top: el.getBoundingClientRect().top,
      scrollTop: scroller.scrollTop,
      scrollHeight: scroller.scrollHeight,
      clientHeight: scroller.clientHeight,
      anchor: anchor && { key: anchor.key, offset: anchor.top },
      keyed: placed.filter((p) => p.bottom > 0 && p.top < scroller.clientHeight).map((p) => [p.key, Math.round(p.top * 100) / 100]),
    };
  });
  await reading.evaluate((el, at) => {
    const scroller = el.closest('.inv-page');
    scroller.scrollTop += el.getBoundingClientRect().top - scroller.getBoundingClientRect().top - at;
  }, READ_AT);
  const before = await place();
  record('ac1 before', { ...before, activity: await activity.count(), steps: await steps.count() });
  assert.ok(Math.abs(before.offset - READ_AT) <= 1, `the board is too short to read its finding ${READ_AT} px down: ${JSON.stringify(before)}`);
  await page.screenshot({ path: path.join(outDir, 'board-phone-live-before.png'), animations: 'disabled', caret: 'hide' });
  const opened = Date.now();
  signal('board-open');
  await expect(steps).toHaveCount(3, { timeout: 10_000 });
  const arrivedMs = Date.now() - opened;
  record('ac1 step arrived', { ms_after_board_open: arrivedMs, step: (await steps.nth(2).innerText()).split('\n').slice(0, 3) });
  await waitFor('wrote.json');
  await expect(steps.nth(2)).toContainText('Succeeded', { timeout: 10_000 });
  const after = await place();
  const said = (await main.locator('.inv-hd [role="status"]').textContent()).trim();
  record('ac1 after', { ...after, activity: await activity.count(), steps: await steps.count(), announced: said });
  assert.ok(arrivedMs <= 10_000, `the step took ${arrivedMs} ms`);
  assert.equal(before.anchor?.key, READ_AT === 0 ? before.reading : before.anchor?.key, 'the finding does not hold the reader\'s place');
  assert.equal(after.anchor?.key, before.anchor?.key, 'the reader\'s place moved to another element');
  assert.ok(Math.abs(after.anchor.offset - before.anchor.offset) <= 1, `the reader's place moved from ${before.anchor.offset} to ${after.anchor.offset}`);
  assert.ok(Math.abs(after.top - before.top) <= 1, `the finding the person reads moved from ${before.top} to ${after.top}`);
  await page.screenshot({ path: path.join(outDir, 'board-phone-live-after.png'), animations: 'disabled', caret: 'hide' });
  record('ac1 pass', { anchor: after.anchor.key, moved_px: after.top - before.top, scrolled_by_px: after.scrollTop - before.scrollTop, arrived_ms: arrivedMs });

  // 2. AC2: keep the first 502 from the board.
  const short1 = r1.slice(0, 8);
  await findings.nth(0).getByRole('button', { name: `Open Row 9 of result ${short1} among its rows` }).click();
  const viewer = page.getByRole('dialog', { name: `Result ${short1}` });
  const viewerRegion = viewer.getByRole('region', { name: `Rows of result ${short1}` });
  await expect(viewerRegion).toBeVisible();
  const marked = viewerRegion.locator('tr[aria-current="true"]');
  await expect(marked.locator('th')).toHaveText('9');
  const keptCells = (await tableRows(marked))[0];
  record('ac2 viewer at row 9', { summary: (await viewer.locator('.results-summary').textContent()).trim(), cells: keptCells });
  await viewerRegion.getByRole('button', { name: 'Keep row 9' }).click();
  const reason = 'The first checkout 502, 40 ms after shop-api first ran out of pool connections';
  const field = viewer.getByRole('form', { name: 'Keep row 9' }).getByRole('textbox', { name: 'Why keep row 9?' });
  await expect(field).toBeFocused();
  await field.fill(reason);
  await page.keyboard.press('Enter');
  await expect(viewer.locator('.inv-write-note [role="status"]')).toHaveText('Kept row 9. The board shows it in the activity.');
  await page.screenshot({ path: path.join(outDir, 'board-phone-kept.png'), animations: 'disabled', caret: 'hide' });
  await page.keyboard.press('Escape');
  await expect(viewer).toHaveCount(0);
  signal('kept.json', { result_id: r1, row: 9, reason });
  await waitFor('saw-keep.json');

  // 3. AC3: send an absolute-range search, then a pinned quick range.
  const deploy = deployTime();
  const from = second(deploy);
  const to = second(new Date(deploy.getTime() + 10 * 60_000));
  const sends = [];
  async function send(url, purpose, moving) {
    await page.goto(`${ORIGIN}${url}`);
    await expect(searchRows(page).first()).toBeVisible();
    let shown = await tableRows(searchRows(page));
    let searchHeaders = await headers(page.locator('#search-results table.results-table'));
    await page.locator('.tabs-actions').getByRole('button', { name: 'Send to investigation' }).click();
    const dialog = page.getByRole('dialog', { name: 'Send to investigation' });
    const target = dialog.getByRole('radio', { name: `${title} ${id.slice(0, 8)}` });
    await expect(target).toBeVisible();
    await expect(dialog.getByRole('radio', { checked: true })).toHaveCount(0);
    const offered = moving
      ? { moving: (await dialog.locator('.send-moving').textContent()).trim() }
      : { query: (await dialog.locator('.send-query').textContent()).trim(), window: (await dialog.locator('.send-window').textContent()).trim() };
    await target.check();
    await dialog.getByLabel('Purpose (optional)').fill(purpose);
    await page.screenshot({ path: path.join(outDir, moving ? 'send-phone-pin.png' : 'send-phone-absolute.png'), animations: 'disabled', caret: 'hide' });
    await dialog.getByRole('button', { name: moving ? 'Pin and re-run' : 'Send', exact: true }).click();
    const toast = page.locator('.toast').filter({ hasText: 'Sent to investigation' });
    await expect(toast).toBeVisible({ timeout: 30_000 });
    const sentUrl = page.url().slice(ORIGIN.length);
    if (moving) {
      shown = await tableRows(searchRows(page));
      searchHeaders = await headers(page.locator('#search-results table.results-table'));
    }
    record('ac3 sent', { url, sent_url: sentUrl, purpose, offered, toast: (await toast.innerText()).trim(), shown_rows: shown.length });
    await toast.getByRole('link', { name: 'Open the board' }).click();
    await page.waitForURL(`**/investigations/${id}`);
    // On a phone the toast covers the board's foot, and it stays while its
    // link holds focus.
    await toast.getByRole('button', { name: 'Dismiss notification' }).click();
    await expect(toast).toHaveCount(0);
    const step = steps.filter({ hasText: purpose });
    await expect(step).toHaveCount(1);
    await step.locator('summary').click();
    const executed = (await step.locator('.inv-executed').textContent()).trim();
    const meta = (await step.locator('.inv-meta').last().textContent()).trim();
    const viewButton = step.getByRole('button', { name: /^View the rows of result / });
    const short = (await viewButton.getAttribute('aria-label')).trim().split(' ').pop();
    await viewButton.click();
    const stepViewer = page.getByRole('dialog', { name: `Result ${short}` });
    await expect(stepViewer.getByRole('region', { name: `Rows of result ${short}` })).toBeVisible();
    const paged = await viewerRows(stepViewer, short);
    // The same events, shown the same way: one `_raw` per event, and every
    // displayed cell of every row equal as a set of rows, `_severity`'s
    // token included. Order is recorded beside it: the capture sorts
    // newest first and Search shows its own order (README, Observations).
    const data = (hs) => hs.filter(([, cls]) => !/\b(exp-col|inv-row-n|inv-keep-col)\b/.test(cls))
      .map(([text]) => text.replace(/[\s·▲▼↑↓]+$/u, ''));
    const raws = (rows, hs) => rows.map((r) => r[data(hs).indexOf('_raw')]).sort();
    const sameEvents = JSON.stringify(raws(paged.rows, paged.headers)) === JSON.stringify(raws(shown, searchHeaders));
    const rowSet = (rows) => rows.map((r) => JSON.stringify(r)).sort();
    const sameCells = JSON.stringify(rowSet(paged.rows)) === JSON.stringify(rowSet(shown));
    const sameOrder = JSON.stringify(paged.rows) === JSON.stringify(shown);
    assert.deepEqual(data(paged.headers), data(searchHeaders), 'the viewer and Search name different columns');
    record('ac3 board step', {
      purpose, executed, meta, result: short, viewer_pages: paged.pages, columns: data(paged.headers),
      viewer_headers: paged.headers, search_headers: searchHeaders,
      viewer_rows: paged.rows, search_rows: shown, same_events: sameEvents, same_cells: sameCells, same_cells_in_order: sameOrder,
    });
    assert.ok(data(paged.headers).includes('_raw'), 'the viewer shows no _raw column');
    assert.equal(paged.rows.length, shown.length, `the viewer pages ${paged.rows.length} rows and Search showed ${shown.length}`);
    assert.ok(sameEvents, `the viewer's events differ from the Search table's for "${purpose}"`);
    assert.ok(sameCells, `the viewer's rows differ from the Search table's, cell for cell, for "${purpose}"`);
    await page.keyboard.press('Escape');
    sends.push({ purpose, executed });
  }
  await send(`/search?q=${encodeURIComponent('service=nginx status=502')}&r=${from}..${to}`,
    'The 502s in the ten minutes after deploy 4812', false);
  await send(`/search?q=${encodeURIComponent('service=shop-api')}&r=1h`, 'The last hour of shop-api, pinned', true);
  signal('sent.json', { sends: sends.length });
  await waitFor('done.json');

  // The board after the loop, at its top, and the writer's controls.
  await page.goto(`${ORIGIN}/investigations/${id}`);
  await expect(main.getByText('Every entry is loaded.')).toBeVisible();
  await expect(steps).toHaveCount(7);
  await page.screenshot({ path: path.join(outDir, 'board-phone-final.png'), animations: 'disabled', caret: 'hide' });
  const writerAria = await ariaMatrix(page, id, r1, from, to);
  writeFileSync(path.join(outDir, 'writer-aria.txt'), writerAria.text);
  assert.ok(writerAria.has.close && writerAria.has.keep && writerAria.has.send, 'the writer lacks a control');
  const writerVideo = await page.video().path();
  await context.close();

  // 4. AC4: the read-only person's session.
  const readerState = await signIn(browser, path.join(runDir, 'private', 'reader-key'), 'reader');
  const reader = await workingPage(browser, readerState, 'reader');
  const readerAria = await ariaMatrix(reader.page, id, r1, from, to);
  writeFileSync(path.join(outDir, 'reader-aria.txt'), readerAria.text);
  record('ac4', { writer: writerAria.has, reader: readerAria.has });
  assert.deepEqual(readerAria.has, { close: false, reopen: false, keep: false, send: false }, 'the reader holds a write control');
  const readerVideo = await reader.page.video().path();
  await reader.context.close();
  // Relative to the checkout, so the log names no home directory.
  const top = path.resolve(here, '../../..');
  record('videos', { writer: path.relative(top, writerVideo), reader: path.relative(top, readerVideo) });
} catch (e) {
  failed = true;
  record('failed', { message: e.message });
  signal('board-failed.json', { message: e.message });
} finally {
  await browser.close();
}
process.exit(failed ? 1 : 0);

/** The board header, a viewer row and Search's trailing actions as the
 * session sees them, as ariaSnapshot text, and which write controls it has. */
async function ariaMatrix(page, id, r1, from, to) {
  const short1 = r1.slice(0, 8);
  await page.goto(`${ORIGIN}/investigations/${id}`);
  const main = page.locator('.inv-page');
  await expect(main.getByText('Every entry is loaded.')).toBeVisible();
  const header = await main.locator('.page-hd').ariaSnapshot();
  const has = {
    close: await main.getByRole('button', { name: 'Close the investigation' }).count() > 0,
    reopen: await main.getByRole('button', { name: 'Reopen the investigation' }).count() > 0,
  };
  await main.getByRole('list', { name: 'Findings' }).locator('> li').nth(0)
    .getByRole('button', { name: `Open Row 9 of result ${short1} among its rows` }).click();
  const viewer = page.getByRole('dialog', { name: `Result ${short1}` });
  const region = viewer.getByRole('region', { name: `Rows of result ${short1}` });
  await expect(region).toBeVisible();
  const row = await region.locator('tr[aria-current="true"]').ariaSnapshot();
  has.keep = await viewer.getByRole('button', { name: /^Keep row / }).count() > 0;
  await page.keyboard.press('Escape');
  await page.goto(`${ORIGIN}/search?q=${encodeURIComponent('service=nginx status=502')}&r=${from}..${to}`);
  await expect(searchRows(page).first()).toBeVisible();
  const trailing = await page.locator('.tabs-actions').ariaSnapshot();
  has.send = await page.locator('.tabs-actions').getByRole('button', { name: 'Send to investigation' }).count() > 0;
  const text = [
    `# board header (.page-hd) of /investigations/${id}`, header, '',
    `# the viewer's row 9 of result ${short1}, opened from the finding`, row, '',
    '# Search trailing actions (.tabs-actions) on an accepted absolute-range search', trailing, '',
  ].join('\n');
  return { text, has };
}
