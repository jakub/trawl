// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect } from '../fixtures';

const identity = { name: 'return-test', roles: [], permissions: ['query', 'saved_query'] };

test('initial session rejection returns to the complete requested URL after sign-in', async ({ page }) => {
  let signedIn = false;
  await page.route('**/api/auth/me', route => signedIn
    ? route.continue()
    : route.fulfill({ status: 401, body: '' }));
  await page.route('**/api/auth/login', route => {
    signedIn = true;
    return route.fulfill({ json: identity });
  });
  const destination = '/search/history?note=a%2Bb+%2525#row?value';
  await page.goto(destination);
  await expect(page).toHaveURL(url => url.pathname === '/login'
    && url.search === `?return_to=${encodeURIComponent(destination)}`);
  await page.getByLabel('API key', { exact: true }).fill('disposable-test-key');
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page).toHaveURL(url => `${url.pathname}${url.search}${url.hash}` === destination);
});

for (const failure of ['rejection', 'server', 'network'] as const) {
  test(`sign-in ${failure} and reload preserve the return destination until success`, async ({ page }) => {
    let fail = true;
    await page.route('**/api/auth/login', route => {
      if (!fail) return route.fulfill({ json: identity });
      if (failure === 'network') return route.abort('failed');
      return route.fulfill({ status: failure === 'rejection' ? 401 : 503, body: '' });
    });
    const destination = '/search/history?note=%252F+a%2Bb#row';
    const login = `/login?return_to=${encodeURIComponent(destination)}`;
    await page.goto(login);
    await page.getByLabel('API key', { exact: true }).fill('disposable-test-key');
    await page.getByRole('button', { name: 'Sign in', exact: true }).click();
    await expect(page.getByRole('alert')).toBeVisible();
    await expect(page).toHaveURL(url => `${url.pathname}${url.search}` === login);
    await page.reload();
    await expect(page).toHaveURL(url => `${url.pathname}${url.search}` === login);
    fail = false;
    await page.getByLabel('API key', { exact: true }).fill('disposable-test-key');
    await page.getByRole('button', { name: 'Sign in', exact: true }).click();
    await expect(page).toHaveURL(url => `${url.pathname}${url.search}${url.hash}` === destination);
  });
}
