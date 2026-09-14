// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario } from '../fixtures';
import { SEL } from '../selectors';

for (const failure of ['server', 'network'] as const) {
  test(`auth ${failure} failure retains the destination and retries without credentials`, async ({ page, request }) => {
    await resetScenario(request, 'corpus');
    let failed = true;
    await page.route('**/api/auth/me', async route => {
      if (!failed) return route.continue();
      if (failure === 'network') return route.abort('failed');
      return route.fulfill({ status: 502, body: 'upstream unavailable' });
    });
    await page.goto('/jobs/nets?net=1&ntab=runs');
    await expect(page.getByRole('alert')).toContainText('Unable to check your session');
    await expect(page).toHaveURL(/\/jobs\/nets\?net=1&ntab=runs$/);
    await expect(page.locator('.sd-drawer')).toHaveCount(0);
    failed = false;
    await page.getByRole('button', { name: 'Retry session check' }).click();
    await expect(page.locator('.sd-drawer')).toBeVisible();
    await expect(page).toHaveURL(/\/jobs\/nets\?net=1&ntab=runs$/);
  });
}

test('auth rejection redirects, while lack of permission has its own state', async ({ page }) => {
  await page.route('**/api/auth/me', route => route.fulfill({ status: 403, body: '' }));
  await page.goto('/search/history');
  await expect(page.getByRole('alert')).toContainText('does not have access to Trawl');
  await expect(page).toHaveURL(/\/search\/history$/);
  await page.unroute('**/api/auth/me');
  await page.route('**/api/auth/me', route => route.fulfill({ status: 401, body: '' }));
  await page.getByRole('button', { name: 'Retry session check' }).click();
  await expect(page).toHaveURL(/\/login$/);
});

for (const failure of ['server', 'network'] as const) {
  test(`logout ${failure} failure remains visible until a successful retry`, async ({ page }) => {
    let fail = true;
    let calls = 0;
    await page.route('**/api/auth/logout', route => {
      calls += 1;
      if (!fail) return route.fulfill({ status: 204 });
      return failure === 'network'
        ? route.abort('failed')
        : route.fulfill({ status: 502, body: 'upstream unavailable' });
    });
    await page.goto('/search');
    await page.locator(SEL.topbarUser).click();
    await page.getByRole('menuitem', { name: 'Sign Out', exact: true }).click();
    await expect(page.getByRole('alert')).toContainText('Your session may still be active');
    await expect(page).toHaveURL(/\/search$/);
    await expect(page.locator(SEL.dslEditor)).toBeVisible();
    expect(calls).toBe(1);
    fail = false;
    await page.getByRole('button', { name: 'Retry sign out' }).click();
    await expect(page).toHaveURL(/\/login$/);
    expect(calls).toBe(2);
  });
}

test('login validation identifies and focuses the invalid API key', async ({ page }) => {
  await page.goto('/login');
  const key = page.getByLabel('API key');
  await page.getByRole('button', { name: 'Sign In', exact: true }).click();
  await expect(key).toHaveAttribute('aria-invalid', 'true');
  await expect(key).toBeFocused();
  const errorId = await key.getAttribute('aria-describedby');
  expect(errorId).toBeTruthy();
  await expect(page.locator(`[id="${errorId}"]`)).toContainText('API key is required');
  await key.fill('invalid-test-key');
  await expect(key).toHaveAttribute('aria-invalid', 'false');
});

for (const status of [401, 503]) {
  test(`login HTTP ${status} associates the error with the key and distinguishes rejection`, async ({ page }) => {
    await page.route('**/api/auth/login', route => route.fulfill({ status, body: '' }));
    await page.goto('/login');
    const key = page.getByLabel('API key');
    await key.fill('disposable-invalid-key');
    await page.getByRole('button', { name: 'Sign In', exact: true }).click();
    await expect(page.getByRole('alert')).toBeVisible();
    await expect(key).toHaveAttribute('aria-invalid', String(status === 401));
    await expect(key).toHaveAttribute('aria-describedby', 'fleet-login-error');
    if (status === 401) await expect(key).toBeFocused();
  });
}
