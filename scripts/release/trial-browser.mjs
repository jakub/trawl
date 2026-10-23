// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Follow the browser steps of getting-started/first-query.md in real
// Chromium against a running `trawl trial`, once at each ORIGIN, each in a
// fresh browser context: sign in with the operator key, select the 7d range,
// run the documented query with Haul, and open the Health page.
// test-trial.sh runs it at http://localhost:<web-port>, the acceptance
// criterion's address, and at http://127.0.0.1:<web-port>, the address
// `trawl trial up`, `status`, and the tutorial print.
//
// Usage: node trial-browser.mjs TOKEN-FILE OUTPUT QUERY ROW ORIGIN...
//
// The token is read from TOKEN-FILE and typed into the sign-in form; it is
// never logged. The browser records no trace, HAR, or video, and no storage
// state is saved: the only files written to OUTPUT are screenshots taken
// after sign-in, named after the origin's host, and a JSON report of the
// phases that passed.
// TRIAL_E2E_DIR names the directory whose node_modules holds @playwright/test.
import fs from 'node:fs/promises';
import path from 'node:path';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const [tokenFile, output, query, row, ...origins] = process.argv.slice(2);
if (origins.length === 0 || !process.env.TRIAL_E2E_DIR) {
  console.error('usage: TRIAL_E2E_DIR=DIR node trial-browser.mjs TOKEN-FILE OUTPUT QUERY ROW ORIGIN...');
  process.exit(2);
}
const require = createRequire(path.join(process.env.TRIAL_E2E_DIR, 'package.json'));
const { chromium, expect } = require('@playwright/test');
const expected = JSON.parse(row);

const browser = await chromium.launch({ headless: true });
const phases = [];
const errors = [];

// The result-column vocabulary (reference/api.md, Result columns). A column
// or a mixed cell's tag is a descriptor `{type, items}`, where only `list`
// carries `items`, itself a descriptor. Checked before any cell is read, so
// a response with no rows still has to name known types.
const SCALARS = new Set(['integer', 'float', 'boolean', 'string', 'timestamp', 'date', 'time', 'mixed']);
function descriptor({ type, items } = {}) {
  if (type === 'list') {
    assert.ok(items && typeof items === 'object', `list carries no items descriptor: ${JSON.stringify({ type, items })}`);
    return { type, items: descriptor(items) };
  }
  assert.ok(SCALARS.has(type), `column type ${JSON.stringify(type)} is not in the result-column vocabulary`);
  assert.equal(items, undefined, `${type} carries an items descriptor`);
  return { type };
}

// The wire shapes of the cells that are strings: a canonical decimal
// integer within the 64-bit ranges, RFC 3339 with six fraction digits and
// an offset (or a signed expanded year outside 0000..9999, or an
// infinity), a date, a time with any fraction.
const INTEGER = /^(0|-?[1-9][0-9]*)$/;
const INTEGER_MIN = -(2n ** 63n);
const INTEGER_MAX = 2n ** 64n - 1n;
const TIMESTAMP = /^(?:(?:\d{4}|-\d{4,}|\+\d{5,})-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{6}(?:Z|[+-]\d{2}:\d{2})|-?infinity)$/;
const DATE = /^\d{4}-\d{2}-\d{2}$/;
const TIME = /^\d{2}:\d{2}:\d{2}(?:\.\d+)?$/;

// One result cell as a JavaScript value, read under a checked descriptor.
// An `integer` cell is a decimal string on the wire; the documented rows
// hold small counts, so Number() is exact. A cell outside the contract is
// an error, so an old or malformed response cannot produce the documented
// row by accident.
function cell({ type, items }, value) {
  if (value === null) return null;
  const shaped = (ok, what) => assert.ok(ok, `${what} cell ${JSON.stringify(value)} is not on the wire contract`);
  switch (type) {
    case 'integer': {
      shaped(typeof value === 'string' && INTEGER.test(value), 'integer');
      const integer = BigInt(value);
      shaped(integer >= INTEGER_MIN && integer <= INTEGER_MAX, 'integer');
      return Number(value);
    }
    case 'float':
      shaped(typeof value === 'number' || ['NaN', 'Infinity', '-Infinity'].includes(value), 'float');
      return value;
    case 'boolean':
      shaped(typeof value === 'boolean', 'boolean');
      return value;
    case 'string':
      shaped(typeof value === 'string', 'string');
      return value;
    case 'timestamp':
      shaped(typeof value === 'string' && TIMESTAMP.test(value), 'timestamp');
      return value;
    case 'date':
      shaped(typeof value === 'string' && DATE.test(value), 'date');
      return value;
    case 'time':
      shaped(typeof value === 'string' && TIME.test(value), 'time');
      return value;
    case 'list':
      shaped(Array.isArray(value), 'list');
      return value.map(item => cell(items, item));
    case 'mixed':
      shaped(value !== null && typeof value === 'object' && !Array.isArray(value), 'mixed');
      return cell(descriptor({ type: value.type, items: value.items }), value.value);
    default:
      throw new Error(`column type ${JSON.stringify(type)} is not in the result-column vocabulary`);
  }
}

// One query response as an object per row, keyed by column name. Every
// column descriptor is checked first, rows or no rows.
async function rows(response) {
  assert.equal(response.status(), 200);
  const body = await response.json();
  const columns = body.columns.map(c => ({ name: c.name, ...descriptor({ type: c.type, items: c.items }) }));
  return body.rows.map(values => Object.fromEntries(columns.map((c, i) => [c.name, cell(c, values[i])])));
}

// The whole tutorial at one origin, in a context that shares nothing with
// another origin's run.
async function tutorial(origin) {
  const host = new URL(origin).hostname;
  const passed = phase => phases.push(`${host}: ${phase}`);
  const shot = name => path.join(output, `${host}-${name}.png`);
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, reducedMotion: 'reduce' });
  try {
    const page = await context.newPage();
    page.setDefaultTimeout(30000);
    page.on('pageerror', e => errors.push(`${host}: ${e.message}`));

    await page.goto(`${origin}/login`);
    const token = (await fs.readFile(tokenFile, 'utf8')).trim();
    await page.getByLabel('API key', { exact: true }).fill(token);
    await page.getByRole('button', { name: 'Sign in', exact: true }).click();
    await page.waitForURL(url => url.origin === origin && url.pathname.startsWith('/search'));
    passed('signed-in-with-the-operator-key');

    await expect(page.getByRole('heading', { name: 'Quick start', exact: true })).toBeVisible();
    passed('quick-start-shown');

    // 7d covers every sample, so the row is the whole-trial answer.
    await page.locator('.daterange .dr-trigger').click();
    await page.locator('.dr-pop .opt').filter({ hasText: 'Last 7d' }).click();
    await expect.poll(() => new URL(page.url()).searchParams.get('r')).toBe('7d');
    await page.locator('.dsl-editor .cm-content').click();
    await page.keyboard.press('Control+a');
    await page.keyboard.insertText(query);
    const response = page.waitForResponse(r => new URL(r.url()).origin === origin
      && new URL(r.url()).pathname === '/api/v1/query'
      && r.request().method() === 'POST' && r.request().postDataJSON().query.includes(query));
    await page.getByRole('button', { name: 'Haul', exact: true }).click();
    const found = await rows(await response);
    assert.deepEqual(found, [expected]);
    const results = page.locator('#search-results');
    for (const value of Object.values(expected)) {
      await expect(results).toContainText(String(value));
    }
    passed('documented-query-exact-row');
    await page.screenshot({ animations: 'disabled', path: shot('documented-query') });

    await page.goto(`${origin}/settings/health`);
    await expect(page.getByRole('heading', { level: 1, name: 'Health', exact: true })).toBeVisible();
    // The capacity card is served to server_manage only (ADR-0025).
    await expect(page.locator('.health-capacity')).toBeVisible();
    await expect(page.locator('.health-check').first()).toBeVisible();
    passed('health-page-with-server-manage');
    await page.screenshot({ animations: 'disabled', path: shot('health') });
  } finally {
    await context.close();
  }
}

try {
  for (const origin of origins) {
    await tutorial(origin);
  }
  assert.deepEqual(errors, []);
  await fs.writeFile(path.join(output, 'browser-report.json'), JSON.stringify({ status: 'passed', origins, phases, errors }, null, 2));
  console.log('browser: ' + phases.join(', '));
} catch (error) {
  await fs.writeFile(path.join(output, 'browser-report.json'), JSON.stringify({ status: 'failed', origins, phases, errors, error: error.message }, null, 2));
  throw error;
} finally {
  await browser.close();
}
