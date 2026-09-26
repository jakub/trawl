// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Follow the browser steps of getting-started/first-query.md in real
// Chromium against a running `trawl trial`: sign in with the operator key,
// select the 7d range, run the documented query with Haul, and open the
// Health page. test-trial.sh runs it.
//
// Usage: node trial-browser.mjs ORIGIN TOKEN-FILE OUTPUT QUERY ROW
//
// The token is read from TOKEN-FILE and typed into the sign-in form; it is
// never logged. The browser records no trace, HAR, or video, and no storage
// state is saved: the only files written to OUTPUT are screenshots taken
// after sign-in and a JSON report of the phases that passed.
// TRIAL_E2E_DIR names the directory whose node_modules holds @playwright/test.
import fs from 'node:fs/promises';
import path from 'node:path';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const [origin, tokenFile, output, query, row] = process.argv.slice(2);
if (!row || !process.env.TRIAL_E2E_DIR) {
  console.error('usage: TRIAL_E2E_DIR=DIR node trial-browser.mjs ORIGIN TOKEN-FILE OUTPUT QUERY ROW');
  process.exit(2);
}
const require = createRequire(path.join(process.env.TRIAL_E2E_DIR, 'package.json'));
const { chromium, expect } = require('@playwright/test');
const expected = JSON.parse(row);

const browser = await chromium.launch({ headless: true });
const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, reducedMotion: 'reduce' });
const page = await context.newPage();
page.setDefaultTimeout(30000);
const phases = [];
const errors = [];
page.on('pageerror', e => errors.push(e.message));

// One query response as an object per row, keyed by column name.
async function rows(response) {
  assert.equal(response.status(), 200);
  const body = await response.json();
  return body.rows.map(values => Object.fromEntries(body.columns.map((c, i) => [c.name, values[i]])));
}

try {
  await page.goto(`${origin}/login`);
  const token = (await fs.readFile(tokenFile, 'utf8')).trim();
  await page.getByLabel('API key', { exact: true }).fill(token);
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await page.waitForURL('**/search**');
  phases.push('signed-in-with-the-operator-key');

  await expect(page.getByRole('heading', { name: 'Quick start', exact: true })).toBeVisible();
  phases.push('quick-start-shown');

  // 7d covers every sample, so the row is the whole-trial answer.
  await page.locator('.daterange .dr-trigger').click();
  await page.locator('.dr-pop .opt').filter({ hasText: 'Last 7d' }).click();
  await expect.poll(() => new URL(page.url()).searchParams.get('r')).toBe('7d');
  await page.locator('.dsl-editor .cm-content').click();
  await page.keyboard.press('Control+a');
  await page.keyboard.insertText(query);
  const response = page.waitForResponse(r => new URL(r.url()).pathname === '/api/v1/query'
    && r.request().method() === 'POST' && r.request().postDataJSON().query.includes(query));
  await page.getByRole('button', { name: 'Haul', exact: true }).click();
  const found = await rows(await response);
  assert.deepEqual(found, [expected]);
  const results = page.locator('#search-results');
  for (const value of Object.values(expected)) {
    await expect(results).toContainText(String(value));
  }
  phases.push('documented-query-exact-row');
  await page.screenshot({ animations: 'disabled', path: path.join(output, 'documented-query.png') });

  await page.goto(`${origin}/settings/health`);
  await expect(page.getByRole('heading', { level: 1, name: 'Health', exact: true })).toBeVisible();
  // The capacity card is served to server_manage only (ADR-0025).
  await expect(page.locator('.health-capacity')).toBeVisible();
  await expect(page.locator('.health-check').first()).toBeVisible();
  phases.push('health-page-with-server-manage');
  await page.screenshot({ animations: 'disabled', path: path.join(output, 'health.png') });

  assert.deepEqual(errors, []);
  await fs.writeFile(path.join(output, 'browser-report.json'), JSON.stringify({ status: 'passed', phases, errors }, null, 2));
  console.log('browser: ' + phases.join(', '));
} catch (error) {
  await fs.writeFile(path.join(output, 'browser-report.json'), JSON.stringify({ status: 'failed', phases, errors, error: error.message }, null, 2));
  throw error;
} finally {
  await context.close();
  await browser.close();
}
