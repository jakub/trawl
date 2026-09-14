import fs from 'node:fs/promises';
import path from 'node:path';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
const require = createRequire(path.join(process.cwd(), 'crates/trawl-web-ui/e2e/package.json'));
const { chromium, expect } = require('@playwright/test');
const [origin, keyFile, output] = process.argv.slice(2);
const browser = await chromium.launch({ headless: true });
const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, reducedMotion: 'reduce', permissions: ['clipboard-read', 'clipboard-write'] });
const page = await context.newPage();
page.setDefaultTimeout(20000);
const phases = [];
const errors = [];
page.on('pageerror', e => errors.push(e.message));
const queryResponse = () => page.waitForResponse(r => new URL(r.url()).pathname === '/api/v1/query' && r.request().method() === 'POST');
async function result(response) {
  assert.equal(response.status(), 200);
  const json = await response.json();
  await response.finished();
  return json;
}
async function run(query) {
  await page.locator('.dsl-editor .cm-content').click();
  await page.keyboard.press('Control+a');
  await page.keyboard.insertText(query);
  const response = queryResponse();
  await page.locator('button.run').click();
  return result(await response);
}
try {
  await page.goto(`${origin}/login`);
  await expect(page.getByText('Ask your Trawl operator for a personal API key.')).toBeVisible();
  await page.setViewportSize({ width: 375, height: 667 });
  await expect(page.getByRole('link', { name: 'Create your first API key' })).toBeInViewport();
  await expect(page.getByRole('button', { name: 'Sign In', exact: true })).toBeInViewport();
  await page.screenshot({ animations: 'disabled', path: path.join(output, 'login-help.png') });
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.getByLabel('API key', { exact: true }).fill((await fs.readFile(keyFile, 'utf8')).trim());
  await page.getByRole('button', { name: 'Sign In', exact: true }).click();
  await page.waitForURL('**/search');
  phases.push('personal-key-login');
  const exampleResponse = queryResponse();
  await page.getByRole('button', { name: 'Run example', exact: true }).click();
  const example = await result(await exampleResponse);
  assert.equal(example.rows.length, 3);
  assert.equal(example.pagination.returned, 3);
  await expect(page.locator('.results table tbody tr')).toHaveCount(3);
  phases.push('bounded-example-three-real-events');
  const count = await run('service=tutorial last=1h | stats count() by service');
  assert.deepEqual(count.rows, [['tutorial', 3]]);
  const error = await run('service=tutorial _severity>=error last=1h | table message, duration');
  assert.deepEqual(error.rows, [['connection refused', 1500]]);
  phases.push('both-exact-tutorial-queries');
  await page.screenshot({ animations: 'disabled', path: path.join(output, 'tutorial-query.png') });
  const empty = await run('service=launch-test-absent last=1h');
  assert.equal(empty.rows.length, 0);
  await expect(page.getByText('No events match this query. Check the time range and filters.')).toBeVisible();
  phases.push('true-zero-match-guidance');
  const filters = 'v1.' + Buffer.from(JSON.stringify([{ op: '+', field: 'host', value: 'tutorial-host' }])).toString('base64url');
  const suffix = `?q=service%3Dtutorial&page=0&f=${filters}&r=2026-01-01T00:00:00Z..now`;
  const filteredResponse = queryResponse();
  await page.goto(`${origin}/search${suffix}`);
  const response = await filteredResponse;
  const submitted = response.request().postDataJSON();
  assert.equal(submitted.query, 'host="tutorial-host" _time>="2026-01-01T00:00:00Z" service=tutorial');
  const filtered = await result(response);
  assert.equal(filtered.rows.length, 3);
  await page.locator('.editor-tools button.tool').filter({ hasText: /^Save$/ }).click();
  await expect(page.locator('.save-scope')).toHaveText([
    'Save captures the editor query text shown above. It omits sidebar filters and the time range control.',
    'To share the full browser search state, cancel and use Share beside the editor. Run any editor changes first.',
  ]);
  await expect(page.locator('.modal .preview')).toHaveText('service=tutorial');
  await expect(page.getByRole('dialog', { name: 'Save query as net' })).toBeVisible();
  await expect(page.getByLabel('Name', { exact: true })).toHaveAccessibleDescription('Use only A-Z, a-z, 0-9, hyphens, or underscores.');
  await page.getByLabel('Name', { exact: true }).fill('launch-tutorial-save');
  await page.screenshot({ animations: 'disabled', path: path.join(output, 'save-scope.png') });
  const savedResponse = page.waitForResponse(r => new URL(r.url()).pathname === '/api/v1/saved' && r.request().method() === 'POST');
  await page.getByRole('button', { name: 'Save as net', exact: true }).click();
  const saved = await savedResponse;
  assert.equal(saved.status(), 200, await saved.text());
  assert.deepEqual(saved.request().postDataJSON(), { name: 'launch-tutorial-save', query: 'service=tutorial' });
  const persisted = await saved.json();
  assert.equal(persisted.query, 'service=tutorial');
  await expect(page.locator('.modal .preview')).toHaveCount(0);
  phases.push('save-editor-only-with-real-database');
  await page.getByRole('button', { name: 'Share', exact: true }).click();
  const shared = await page.evaluate(() => navigator.clipboard.readText());
  const parsed = new URL(shared);
  assert.equal(parsed.origin, origin);
  assert.equal(parsed.searchParams.get('q'), 'service=tutorial');
  assert.equal(parsed.searchParams.get('f'), filters);
  assert.equal(parsed.searchParams.get('r'), '2026-01-01T00:00:00Z..now');
  const roundtripResponse = queryResponse();
  await page.goto(shared);
  const roundtrip = await roundtripResponse;
  assert.deepEqual(roundtrip.request().postDataJSON(), submitted);
  assert.equal((await result(roundtrip)).rows.length, 3);
  phases.push('share-executed-state-roundtrip');
  assert.deepEqual(errors, []);
  await fs.writeFile(path.join(output, 'browser-report.json'), JSON.stringify({ status: 'passed', phases, errors }, null, 2));
  console.log('Browser tutorial and first-use checks passed: ' + phases.join(', '));
} catch (error) {
  await fs.writeFile(path.join(output, 'browser-report.json'), JSON.stringify({ status: 'failed', phases, errors, error: error.message }, null, 2));
  throw error;
} finally {
  await context.close();
  await browser.close();
}
