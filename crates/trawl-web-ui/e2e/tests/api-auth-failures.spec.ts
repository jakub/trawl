// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// A 401 means one thing on every page: the session ended (ADR-0066,
// issue #386). A protected read that sees it sends the page to sign-in
// once, carrying the page's current address as the return destination,
// but only when the read still owns its page: an answer that arrives
// after its page was left, or after a newer request replaced it, does
// nothing. A `login` 401 is a failed sign-in, and a 403 is a permission
// refusal; neither navigates.
//
// Every 401 here is held until the test releases it. The shell's own
// reads (`/me`, the footer's health probe) are answered normally by the
// stub, so while the target's answer is held nothing navigates, and the
// navigation that follows its release is that answer's.

import type { Page, Route } from '@playwright/test';
import { test, expect, resetScenario } from '../fixtures';
import { SEL } from '../selectors';

type Reply = Parameters<Route['fulfill']>[0];

const SESSION_GONE: Reply = { status: 401, json: { error: { code: 'auth_error', message: 'SESSION-ENVELOPE-SENTINEL' } } };

// Match the browser-stable encoder login-return.spec.ts uses.
const component = (value: string) => encodeURIComponent(value).replaceAll("'", '%27');
const relative = (url: URL) => `${url.pathname}${url.search}${url.hash}`;

/** Count every main-frame document request for the sign-in page. */
function countSignIns(page: Page) {
  const seen = { count: 0 };
  page.on('request', req => {
    if (req.isNavigationRequest() && req.frame() === page.mainFrame() && new URL(req.url()).pathname === '/login') {
      seen.count += 1;
    }
  });
  return seen;
}

/** Hold the first `method` request to `pathname` until `release()`,
 * then answer it with `reply`. Later requests pass to the stub. */
async function holdFirst(page: Page, pathname: string, reply: Reply = SESSION_GONE, method = 'GET') {
  let open!: () => void;
  const gate = new Promise<void>(resolve => { open = resolve; });
  const held = { arrived: false, delivered: false, later: 0, release: () => open() };
  await page.route(url => url.pathname === pathname, async route => {
    if (route.request().method() !== method) return route.fallback();
    if (held.arrived) {
      held.later += 1;
      return route.fallback();
    }
    held.arrived = true;
    await gate;
    // A request the page aborted meanwhile has nobody to answer.
    await route.fulfill(reply).catch(() => {});
    held.delivered = true;
  });
  return held;
}

/** A bounded quiet window: long enough for a delivered answer's handler
 * to run and start a navigation if it were going to. */
async function quiet(page: Page) {
  await page.waitForTimeout(300);
}

const eligible = [
  { name: 'health', scenario: 'health-viewer', path: '/api/v1/queries', open: '/settings/health?note=a%2Bb#queries' },
  { name: 'search', scenario: 'default', path: '/api/v1/query', open: '/search?q=service%3Dnginx', method: 'POST' },
  { name: 'jobs nets', scenario: 'corpus', path: '/api/v1/saved', open: '/jobs/nets?note=a%2Bb#receipt' },
  { name: 'jobs runs', scenario: 'corpus', path: '/api/v1/runs', open: '/jobs/runs?note=a%2Bb#receipt' },
  { name: 'schema', scenario: 'default', path: '/api/v1/schema/services', open: '/search/schema?note=a%2Bb#svc' },
  { name: 'investigations', scenario: 'investigations', path: '/api/v1/investigations', open: '/investigations?note=a%2Bb#list' },
] as const;

for (const target of eligible) {
  test(`a delayed ${target.name} 401 sends the page to sign-in once with its current address`, async ({ page, request }) => {
    await resetScenario(request, target.scenario);
    const signIns = countSignIns(page);
    const held = await holdFirst(page, target.path, SESSION_GONE, 'method' in target ? target.method : 'GET');
    await page.goto(target.open);
    await expect.poll(() => held.arrived).toBe(true);
    await expect(page.getByRole('main')).toBeVisible();
    // Held: the shell's own reads have answered, and nothing navigated.
    await quiet(page);
    expect(signIns.count).toBe(0);
    const destination = relative(new URL(page.url()));
    expect(destination.startsWith(target.open.split(/[?#]/)[0])).toBe(true);

    held.release();
    await expect(page).toHaveURL(url =>
      url.pathname === '/login' && url.search === `?return_to=${component(destination)}`);
    await expect(page.getByRole('button', { name: 'Sign in', exact: true })).toBeVisible();
    await quiet(page);
    expect(signIns.count).toBe(1);
    await expect(page.locator('body')).not.toContainText('SESSION-ENVELOPE-SENTINEL');
  });
}

test('a login 401 is a failed sign-in and stays on the sign-in page', async ({ page }) => {
  const signIns = countSignIns(page);
  let attempts = 0;
  await page.route('**/api/auth/login', route => {
    attempts += 1;
    return route.fulfill(SESSION_GONE);
  });
  const login = `/login?return_to=${component('/jobs/nets?note=kept')}`;
  await page.goto(login);
  await page.getByLabel('API key', { exact: true }).fill('disposable-test-key');
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByRole('alert')).toContainText('Invalid API key');
  await quiet(page);
  await expect(page).toHaveURL(url => relative(url) === login);
  expect(signIns.count).toBe(1);
  expect(attempts).toBe(1);
  await expect(page.locator('body')).not.toContainText('SESSION-ENVELOPE-SENTINEL');
});

const FORBIDDEN: Reply = { status: 403, json: { error: { code: 'forbidden', message: 'insufficient permissions' } } };

test('a protected 403 on Jobs is a permission refusal, not a sign-in', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  const signIns = countSignIns(page);
  const held = await holdFirst(page, '/api/v1/saved', FORBIDDEN);
  await page.goto('/jobs/nets?note=forbidden');
  await expect.poll(() => held.arrived).toBe(true);
  held.release();
  await expect.poll(() => held.delivered).toBe(true);
  await expect(page.locator('.load-hint.error')).toContainText("Couldn't load nets: insufficient permissions");
  await quiet(page);
  await expect(page).toHaveURL(url => relative(url) === '/jobs/nets?note=forbidden');
  expect(signIns.count).toBe(0);
});

test('a protected 403 on Health is a permission refusal, not a sign-in', async ({ page, request }) => {
  await resetScenario(request, 'health-viewer');
  const signIns = countSignIns(page);
  const held = await holdFirst(page, '/api/v1/queries', FORBIDDEN);
  await page.goto('/settings/health');
  await expect.poll(() => held.arrived).toBe(true);
  held.release();
  await expect(page.locator(SEL.healthQueries).getByRole('alert'))
    .toHaveText('You do not have permission to read this report.');
  await quiet(page);
  await expect(page).toHaveURL(url => relative(url) === '/settings/health');
  expect(signIns.count).toBe(0);
});

test('a Health 401 that arrives after the page was left does not navigate', async ({ page, request }) => {
  await resetScenario(request, 'health-viewer');
  const signIns = countSignIns(page);
  const held = await holdFirst(page, '/api/v1/queries');
  await page.goto('/settings/health');
  await expect.poll(() => held.arrived).toBe(true);
  await page.locator(SEL.healthAwayLink).click();
  await expect(page).toHaveURL(url => url.pathname === '/search/schema');
  await expect(page.getByRole('heading', { name: 'Schema', level: 1 })).toBeVisible();
  held.release();
  await expect.poll(() => held.delivered).toBe(true);
  await quiet(page);
  await expect(page).toHaveURL(url => url.pathname === '/search/schema');
  expect(signIns.count).toBe(0);
});

test('a Health 401 for a request a newer read replaced does not navigate', async ({ page, request }) => {
  await resetScenario(request, 'health-viewer');
  const signIns = countSignIns(page);
  const held = await holdFirst(page, '/api/v1/queries');
  await page.goto('/settings/health');
  await expect.poll(() => held.arrived).toBe(true);
  // The newer read passes to the stub and lands first.
  await page.locator(SEL.healthQueriesRefresh).click();
  await expect.poll(() => held.later).toBe(1);
  await expect(page.locator(SEL.healthRow)).toHaveCount(4);
  held.release();
  await expect.poll(() => held.delivered).toBe(true);
  await quiet(page);
  await expect(page).toHaveURL(url => relative(url) === '/settings/health');
  await expect(page.locator(SEL.healthRow)).toHaveCount(4);
  expect(signIns.count).toBe(0);
});

test('a Search 401 for a run a newer one replaced does not navigate', async ({ page }) => {
  const signIns = countSignIns(page);
  const held = await holdFirst(page, '/api/v1/query', SESSION_GONE, 'POST');
  await page.goto('/search?q=service%3Dnginx');
  await expect.poll(() => held.arrived).toBe(true);
  // A newer search replaces the held one: an in-app link to another
  // query, while Run is still disabled by the held one.
  await page.evaluate(href => {
    const link = document.createElement('a');
    link.href = href;
    document.body.append(link);
    link.click();
    link.remove();
  }, '/search?q=service%3Dapi');
  await expect(page).toHaveURL(url => url.searchParams.get('q') === 'service=api');
  held.release();
  await expect.poll(() => held.delivered).toBe(true);
  // Whether the newer run waited for the held one or not, it is sent and
  // answered.
  await expect.poll(() => held.later).toBeGreaterThanOrEqual(1);
  await expect(page.locator(SEL.statusLabel)).toContainText('Connected');
  await quiet(page);
  await expect(page).toHaveURL(url => url.pathname === '/search' && url.searchParams.get('q') === 'service=api');
  expect(signIns.count).toBe(0);
});
