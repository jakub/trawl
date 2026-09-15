// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario, capturedSavedRequests, lastCapturedQuery } from '../fixtures';
import type { APIRequestContext, Page } from '@playwright/test';
import { SEL, COPY } from '../selectors';
import { expectFocusRing } from '../a11y';

test('Settings opens Health and offers only Health and the existing Schema page', async ({ page, request }) => {
  await resetScenario(request, 'health-viewer');
  await page.goto('/settings');
  await expect(page).toHaveURL(/\/settings\/health$/);
  await expect(page.locator(SEL.healthPage)).toBeVisible();

  const rail = page.locator(SEL.paletteRailLink);
  await expect(rail).toHaveText(['Search', 'History', 'Schema', 'Nets', 'Runs', 'Health']);
  await expect(rail.nth(5)).toHaveAttribute('href', '/settings/health');
  await expect(rail.nth(2)).toHaveAttribute('href', '/search/schema');
  await expect(rail.nth(5)).toHaveClass(/\bactive\b/);

  await rail.nth(2).click();
  await expect(page).toHaveURL(/\/search\/schema$/);
  // Scoped to the page: the command bar's crumb is a heading with the
  // same name, which is the point of a breadcrumb.
  await expect(page.getByRole('main').getByRole('heading', { name: 'Schema', exact: true })).toBeVisible();
  await expect(rail.nth(2)).toHaveClass(/\bactive\b/);
});

test('Settings replaces its intermediate history entry and preserves the shell stream', async ({ page, request }) => {
  const state = async () => (await request.get('/__ctl/state')).json();
  await expect.poll(async () => (await state()).dashboard.open).toBe(0);
  const baseline = (await state()).dashboard.opens;
  await resetScenario(request, 'health-admin');
  await page.goto('/search');
  await expect(page.locator(SEL.dslEditor)).toBeVisible();
  await expect(page.locator(SEL.healthFooterHot)).toContainText('731');
  await expect.poll(async () => (await state()).dashboard.open).toBe(1);
  await page.evaluate(() => { (window as any).__settingsNavigation = true; });

  // /settings has no sidebar entry of its own — it redirects to Health.
  // A same-document anchor is what the router intercepts, so the
  // __settingsNavigation marker survives the hop.
  await page.evaluate(() => {
    const a = document.createElement('a');
    a.href = '/settings';
    a.id = '__settings';
    a.textContent = 'settings';
    document.body.append(a);
  });
  await page.click('#__settings');
  await expect(page).toHaveURL(/\/settings\/health$/);
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Live');
  expect(await page.evaluate(() => (window as any).__settingsNavigation)).toBe(true);

  await page.goBack();
  await expect(page).toHaveURL(/\/search$/);
  await expect(page.locator(SEL.dslEditor)).toBeVisible();
  await page.goForward();
  await expect(page).toHaveURL(/\/settings\/health$/);
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Live');
  expect(await page.evaluate(() => (window as any).__settingsNavigation)).toBe(true);

  const dashboard = (await state()).dashboard;
  expect(dashboard.open).toBe(1);
  expect(dashboard.opens - baseline).toBe(1);
  expect(dashboard.max).toBe(1);
});

for (const section of ['sources', 'retention', 'users']) {
  test(`obsolete Settings ${section} route renders the existing NotFound page`, async ({ page }) => {
    await page.goto(`/settings/${section}`);
    await expect(page).toHaveURL(new RegExp(`/settings/${section}$`));
    await expect(page.locator(SEL.notFoundHeading)).toHaveText(COPY.notFoundHeading);
    await expect(page.locator(SEL.notFoundSubtitle)).toHaveText(COPY.notFoundSubtitle);
  });
}

test('Help is a native keyboard link and stays outside the full Settings palette inventory', async ({ page, request }) => {
  await resetScenario(request, 'health-viewer');
  await page.goto('/settings');
  await expect(page.locator(SEL.healthPage)).toBeVisible();
  const help = page.locator(SEL.helpLink);
  await expect(help).toHaveJSProperty('tagName', 'A');
  await expect(help).toHaveAccessibleName('Help');
  await expect(help).toHaveAttribute('href', 'https://trawl.sh');
  await expect(help).toHaveAttribute('target', '_blank');
  await expect(help).toHaveAttribute('rel', 'noopener noreferrer');

  await page.locator(SEL.paletteRailLink).last().focus();
  await page.keyboard.press('Tab');
  await expect(help).toBeFocused();
  await expectFocusRing(help);
  // Observe native Enter activation without contacting the external origin.
  await help.evaluate((link) => {
    link.addEventListener('click', (event) => {
      event.preventDefault();
      link.setAttribute('data-keyboard-activated', 'true');
    }, { once: true });
  });
  await page.keyboard.press('Enter');
  await expect(help).toHaveAttribute('data-keyboard-activated', 'true');
  await expect(page).toHaveURL(/\/settings\/health$/);

  await page.locator(SEL.paletteTrigger).click();
  await expect(page.locator(SEL.paletteDialog)).toBeVisible();
  await expect(page.locator(SEL.paletteLabel)).toHaveText(['Search', 'History', 'Schema', 'Nets', 'Runs', 'Health']);
  expect(await page.locator(SEL.paletteOption).evaluateAll((options) =>
    options.map((option) => option.getAttribute('href')),
  )).toEqual(['/search', '/search/history', '/search/schema', '/jobs/nets', '/jobs/runs', '/settings/health']);
  await expect(page.locator(SEL.paletteLabel).filter({ hasText: /^Help$/ })).toHaveCount(0);
});

test('Settings trailing slash reaches Health', async ({ page, request }) => {
  await resetScenario(request, 'health-viewer');
  await page.goto('/settings/');
  await expect(page).toHaveURL(/\/settings\/health$/);
  await expect(page.locator(SEL.healthPage)).toBeVisible();
});

// Literal URL and independent expected DSL. Neither comes from the app's encoder.
const SAVE_URL = '/search?q=service%3Dnginx&page=0' +
  '&f=v1.W3sib3AiOiIrIiwiZmllbGQiOiJob3N0IiwidmFsdWUiOiJ3ZWItMDEifV0' +
  '&r=2026-01-01T00:00:00Z..now';
const EFFECTIVE_QUERY = 'host="web-01" _time>="2026-01-01T00:00:00Z" service=nginx';
const EDITOR_BUFFER = '  service=apache  | stats count()\n';

async function editBuffer(page: Page, text: string) {
  await page.locator(SEL.cmContent).click();
  await page.keyboard.press('Control+a');
  await page.keyboard.insertText(text);
}

async function prepareSave(page: Page, request: APIRequestContext, scenario = 'saved-success') {
  await resetScenario(request, scenario);
  const answered = page.waitForResponse((response) => response.url().endsWith('/api/v1/query'));
  await page.goto(SAVE_URL);
  await (await answered).finished();
  expect((await lastCapturedQuery(request, 1)).query).toBe(EFFECTIVE_QUERY);
  await editBuffer(page, EDITOR_BUFFER);
  expect(new URL(page.url()).searchParams.get('q'), 'editing must leave the executed URL query unchanged').toBe('service=nginx');
}

function saveEntry(page: Page, entry: 'editor' | 'toolbar') {
  return entry === 'editor'
    ? page.locator(SEL.editorTool).filter({ hasText: /^Save as net$/ })
    : page.locator(SEL.saveAction);
}

async function expectExactPreview(page: Page, expected = EDITOR_BUFFER) {
  await expect(page.locator(SEL.savePreview)).toBeVisible();
  expect(await page.locator(SEL.savePreview).textContent(),
    'Save snapshot preview must equal the exact editor buffer').toBe(expected);
}

async function submitSave(page: Page, status = 200) {
  const answered = page.waitForResponse((response) =>
    response.url().endsWith('/api/v1/saved') && response.request().method() === 'POST',
  );
  // Modal-scoped: the editor tool carries the same name now.
  await page.locator(SEL.modalPanel).getByRole('button', { name: 'Save as net', exact: true }).click();
  const response = await answered;
  expect(response.status()).toBe(status);
  await response.finished();
}

for (const entry of ['editor', 'toolbar'] as const) {
  test(`Save captures editor buffer: ${entry} preview and POST preserve exact text without filters or range`, async ({ page, request }) => {
    await prepareSave(page, request);
    await saveEntry(page, entry).click();
    await expectExactPreview(page);
    await expect(page.locator('.save-scope')).toHaveText([
      'Save captures the editor query text shown above. It omits sidebar filters and the time range control.',
      'To share the full browser search state, cancel and use Share beside the editor. Run any editor changes first.',
    ]);
    await page.getByLabel('Name', { exact: true }).fill('editor snapshot');
    await submitSave(page);
    await expect(page.locator(SEL.modalPanel)).toHaveCount(0);
    expect(await capturedSavedRequests(request),
      'Save snapshot POST must contain the exact editor buffer once').toEqual([
      { name: 'editor snapshot', query: EDITOR_BUFFER },
    ]);
  });
}

test('Save cancel sends no POST and reopening captures the new buffer', async ({ page, request }) => {
  await prepareSave(page, request);
  await saveEntry(page, 'editor').click();
  await expectExactPreview(page);
  await page.getByRole('button', { name: 'Cancel', exact: true }).click();
  await expect(page.locator(SEL.modalPanel)).toHaveCount(0);
  const next = ' service=postgres | stats count()  ';
  await editBuffer(page, next);
  await saveEntry(page, 'toolbar').click();
  await expectExactPreview(page, next);
  await page.keyboard.press('Escape');
  await expect(page.locator(SEL.modalPanel)).toHaveCount(0);
  await page.waitForTimeout(300);
  expect(await capturedSavedRequests(request)).toEqual([]);
});

async function changeReadableUrl(page: Page) {
  const next = '/search?q=service%3Dpostgres&page=0&r=2026-02-01T00:00:00Z..now';
  // pushState does not notify the router. Forward supplies a real popstate.
  await page.evaluate((url) => history.pushState({}, '', url), next);
  await page.goBack();
  const answered = page.waitForResponse((response) => response.url().endsWith('/api/v1/query'));
  await page.goForward();
  await (await answered).finished();
  await expect(page).toHaveURL(new RegExp('q=service%3Dpostgres'));
}

test('Save keeps one modal and its captured buffer through a readable URL change', async ({ page, request }) => {
  await prepareSave(page, request);
  await saveEntry(page, 'editor').click();
  await expectExactPreview(page);
  await page.getByLabel('Name', { exact: true }).fill('unchanged name');
  const originalModal = await page.locator(SEL.modalPanel).elementHandle();
  expect(originalModal).not.toBeNull();
  await changeReadableUrl(page);
  expect(await originalModal!.evaluate((element) => element.isConnected), 'the original modal must remain mounted').toBe(true);
  await expect(page.locator(SEL.modalPanel)).toHaveCount(1);
  await expect(page.getByLabel('Name', { exact: true })).toHaveValue('unchanged name');
  await expectExactPreview(page);
  await submitSave(page);
  await expect(page.locator(SEL.modalPanel)).toHaveCount(0);
  expect(await capturedSavedRequests(request), 'Save snapshot POST must survive readable URL changes').toEqual([
    { name: 'unchanged name', query: EDITOR_BUFFER },
  ]);
});

test('Save retries an explicit POST failure with the original snapshot', async ({ page, request }) => {
  await prepareSave(page, request, 'saved-retry');
  await saveEntry(page, 'toolbar').click();
  await expectExactPreview(page);
  await page.getByLabel('Name', { exact: true }).fill('retry snapshot');
  await submitSave(page, 503);
  await expect(page.locator(SEL.toastError)).toContainText("Couldn't save");
  await expect(page.locator(SEL.modalPanel)).toBeVisible();
  await expect(page.locator(SEL.modalPanel).getByRole('button', { name: 'Save as net', exact: true })).toBeEnabled();
  await changeReadableUrl(page);
  await expectExactPreview(page);
  await submitSave(page);
  await expect(page.locator(SEL.modalPanel)).toHaveCount(0);
  expect(await capturedSavedRequests(request), 'Save snapshot POST retries must retain the original editor buffer').toEqual([
    { name: 'retry snapshot', query: EDITOR_BUFFER },
    { name: 'retry snapshot', query: EDITOR_BUFFER },
  ]);
});

test('Save controls refuse malformed URLs and an open Save closes without a POST', async ({ page, request }) => {
  await prepareSave(page, request);
  await saveEntry(page, 'editor').click();
  await expectExactPreview(page);
  await page.evaluate(() => history.pushState({}, '', '/search?q=service%3Dnginx&f=v1.!'));
  await page.goBack();
  await page.goForward();
  await expect(page.locator(SEL.urlNotice)).toBeVisible();
  await expect(page.locator(SEL.modalPanel)).toHaveCount(0);
  for (const entry of ['editor', 'toolbar'] as const) {
    await expect(saveEntry(page, entry)).toBeDisabled();
    await saveEntry(page, entry).click({ force: true });
    await expect(page.locator(SEL.modalPanel)).toHaveCount(0);
  }
  await page.waitForTimeout(300);
  expect(await capturedSavedRequests(request)).toEqual([]);
});
