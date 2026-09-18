// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import type { Page } from '@playwright/test';
import { test, expect, resetScenario } from '../fixtures';
import { SEL, COPY } from '../selectors';

const identity = { name: 'return-test', roles: [], permissions: ['query', 'saved_query'] };

// Match the native browser-stable encoder, including its apostrophe rule.
const component = (value: string) => encodeURIComponent(value).replaceAll("'", '%27');
const relative = (url: URL) => `${url.pathname}${url.search}${url.hash}`;
async function signIn(page: Page) {
  await page.getByLabel('API key', { exact: true }).fill('disposable-test-key');
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
}

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

test('canonical routes, aliases and one trailing slash retain their complete suffix', async ({ page, request }) => {
  test.setTimeout(90000);
  await resetScenario(request, 'corpus');
  // Health reads queries on mount; the corpus scenario covers Search and Jobs.
  await page.route('**/api/v1/queries', route => route.request().method() === 'GET'
    ? route.fulfill({ json: { active: [], recent: [], retained: [] } })
    : route.fallback());
  await page.route('**/api/auth/login', route => route.fulfill({ json: identity }));
  const suffix = '?note=%252F+a%2Bb&next=https%3A%2F%2Fexample.com%2Fa#row?value';
  for (const [path, canonical] of [
    ['/search', '/search'], ['/search/history', '/search/history'],
    ['/search/schema', '/search/schema'], ['/jobs/nets', '/jobs/nets'],
    ['/jobs/runs', '/jobs/runs'], ['/settings/health', '/settings/health'],
    ['/', '/search'], ['/jobs', '/jobs/nets'], ['/settings', '/settings/health'],
  ]) {
    for (const ending of path === '/' ? [''] : ['', '/']) {
      await page.goto(`/login?return_to=${component(path + ending + suffix)}`);
      await signIn(page);
      await expect(page).toHaveURL(url => relative(url) === canonical + suffix);
      if (canonical === '/settings/health') {
        // Observe the mounted page before the next iteration navigates away.
        await expect(page.locator(SEL.healthQueries)).toContainText('No active or recent queries.');
      }
    }
  }
});

test('hostile return values fall back without attempting foreign or forbidden document requests', async ({ page, baseURL }) => {
  test.setTimeout(120000);
  const attempted: string[] = [];
  // Record before aborting: the global fixture also blocks foreign origins,
  // so a final-URL assertion alone cannot prove no unsafe attempt occurred.
  await page.route('**/*', route => {
    const req = route.request();
    const url = new URL(req.url());
    if (req.isNavigationRequest() && req.frame() === page.mainFrame()) {
      attempted.push(req.url());
      if (url.origin !== new URL(baseURL!).origin || !['/login', '/search'].includes(url.pathname)) {
        return route.abort();
      }
    }
    return route.fallback();
  });
  await page.route('**/api/auth/login', route => route.fulfill({ json: identity }));
  const values = [
    `${baseURL}/search`, 'https://outside.invalid/search', '//outside.invalid/search',
    '//', '/search\\evil', '/search/../login', '/%73earch', '/search%2f',
    'javascript:alert(1)', 'https://user:pass@outside.invalid/search',
    '/api/auth/me', '/unknown', '/login', '/search//', '/search?x=\u0000', '/search#\u007f',
  ];
  const queries = [
    ...values.map(value => `return_to=${component(value)}`),
    '', 'return_to=', 'return_to=%', 'return_to=%GG', 'return_to=%FF',
    'return_to=%ED%A0%80', 'return_to=%252Fsearch',
    'return_to=/jobs/runs&%72eturn_to=/jobs/nets',
    'return_to=/jobs/runs&return_to=/jobs/runs',
    'return_to=/jobs/runs&other=%FF', '%FF=x&return_to=/jobs/runs',
  ];
  for (const query of queries) {
    attempted.length = 0;
    await page.goto(`/login?${query}`);
    await signIn(page);
    await expect(page).toHaveURL(url => relative(url) === '/search');
    expect(attempted.map(value => new URL(value).pathname), query).toEqual(['/login', '/search']);
    expect(attempted.every(value => new URL(value).origin === new URL(baseURL!).origin)).toBe(true);
  }
});

test('direct sign-in stays open with an existing session, empty validation retains its destination, and Unicode survives', async ({ page }) => {
  let loginCalls = 0;
  await page.route('**/api/auth/login', route => { loginCalls++; return route.fulfill({ json: identity }); });
  const destination = '/search/history?text=%E6%97%A5+a%2Bb&quote=%27#%C3%A9';
  const login = `/login?%72eturn_to=${component(destination)}&ignored=valid+data`;
  await page.goto(login);
  await expect(page.getByRole('button', { name: 'Sign in', exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByLabel('API key', { exact: true })).toHaveAttribute('aria-invalid', 'true');
  await expect(page).toHaveURL(url => relative(url) === login);
  expect(loginCalls).toBe(0);
  await signIn(page);
  await expect(page).toHaveURL(url => relative(url) === destination);
});

test('explicit logout has no return parameter and the next sign-in opens Search', async ({ page }) => {
  await page.route('**/api/auth/login', route => route.fulfill({ json: identity }));
  await page.goto('/search/history?note=previous#row');
  await page.locator(SEL.topbarUser).click();
  await page.getByRole('menuitem', { name: 'Sign Out', exact: true }).click();
  await expect(page).toHaveURL(url => relative(url) === '/login');
  await signIn(page);
  await expect(page).toHaveURL(url => relative(url) === '/search');
});

test('a denied new identity remains at its destination without a sign-in loop', async ({ page }) => {
  await page.route('**/api/auth/login', route => route.fulfill({ json: { ...identity, permissions: [] } }));
  let checks = 0;
  await page.route('**/api/auth/me', route => { checks++; return route.fulfill({ status: 403, body: '' }); });
  const destination = '/jobs/runs?offset=20#receipt';
  await page.goto(`/login?return_to=${component(destination)}`);
  await signIn(page);
  await expect(page.getByRole('alert')).toContainText('does not have access to Trawl');
  await expect(page).toHaveURL(url => relative(url) === destination);
  expect(checks).toBe(1);
});

test('automatic sign-in and success replace history so Back reaches the preceding page', async ({ page }) => {
  await page.goto('/search/history?sentinel=1');
  await expect(page.locator(SEL.historyPage)).toBeVisible();
  let signedIn = false;
  await page.route('**/api/auth/me', route => signedIn ? route.continue() : route.fulfill({ status: 401, body: '' }));
  await page.route('**/api/auth/login', route => { signedIn = true; return route.fulfill({ json: identity }); });
  await page.goto('/search/schema?note=interrupted#field');
  await expect(page).toHaveURL(url => url.pathname === '/login');
  await signIn(page);
  await expect(page).toHaveURL(url => relative(url) === '/search/schema?note=interrupted#field');
  await page.goBack();
  await expect(page).toHaveURL(url => relative(url) === '/search/history?sentinel=1');
});

test('a 32 KiB Search query makes an actual encoded sign-in request and returns unchanged without replay', async ({ page }) => {
  let signedIn = false;
  let loginLength = 0;
  const mutations: string[] = [];
  page.on('request', req => {
    const url = new URL(req.url());
    if (req.isNavigationRequest() && url.pathname === '/login') loginLength = url.search.length - 1;
    if (req.method() !== 'GET' && url.pathname.startsWith('/api/') && !url.pathname.startsWith('/api/auth/')) mutations.push(req.url());
  });
  await page.route('**/api/auth/me', route => signedIn ? route.continue() : route.fulfill({ status: 401, body: '' }));
  let attempts = 0;
  await page.route('**/api/auth/login', route => {
    attempts++;
    if (attempts === 1) return route.fulfill({ status: 401, body: '' });
    signedIn = true;
    return route.fulfill({ json: identity });
  });
  const tail = '&r=garbage';
  const query = `q=${'+'.repeat(32 * 1024 - 2 - tail.length)}${tail}`;
  expect(query.length).toBe(32 * 1024);
  const destination = `/search?${query}#large%23fragment`;
  await page.goto(destination);
  await expect(page).toHaveURL(url => url.search === `?return_to=${component(destination)}`);
  expect(loginLength).toBeGreaterThan(96 * 1024 - 100);
  await signIn(page);
  await expect(page.getByRole('alert')).toContainText('Invalid API key');
  await signIn(page);
  await expect(page).toHaveURL(url => relative(url) === destination);
  await expect(page.locator(SEL.urlNotice)).toBeVisible();
  await expect(page.locator(SEL.urlNotice)).toContainText(COPY.urlNoticeRangePrefix);
  await expect(page.locator(SEL.urlNoticeRaw)).toHaveText('garbage');
  expect(mutations).toEqual([]);
});

for (const kind of ['nets', 'runs', 'drawer'] as const) {
  test(`${kind} polling captures the current destination and restores its URL state`, async ({ page, request }) => {
    await resetScenario(request, 'corpus');
    await page.clock.install();
    const endpoint = kind === 'nets' ? '**/api/v1/saved'
      : kind === 'runs' ? '**/api/v1/runs?*' : '**/api/v1/saved/1/runs?*';
    let expire = false;
    let reads = 0;
    await page.route(endpoint, route => {
      reads++;
      return expire ? route.fulfill({ status: 401, body: '' }) : route.continue();
    });
    await page.route('**/api/auth/login', route => { expire = false; return route.fulfill({ json: identity }); });
    const path = kind === 'runs' ? '/jobs/runs' : '/jobs/nets';
    const suffix = kind === 'drawer' ? '?net=1&ntab=runs' : '?note=initial';
    await page.goto(path + suffix);
    await expect.poll(() => reads).toBeGreaterThan(0);
    await expect(page.locator(kind === 'runs' ? '.runs-table' : '.nets-table')).toBeVisible();
    const destination = `${path}${suffix}&current=a%2Bb+%2525#receipt?row`;
    // Change only URL metadata after mount; the polling redirect must read
    // this current address rather than a component's initial snapshot.
    await page.evaluate(value => history.replaceState(history.state, '', value), destination);
    expire = true;
    await page.clock.fastForward(5001);
    await expect(page).toHaveURL(url => url.pathname === '/login' && url.search === `?return_to=${component(destination)}`);
    await signIn(page);
    await expect(page).toHaveURL(url => relative(url) === destination);
  });
}

test('the first of competing Jobs 401 responses owns one automatic sign-in transition', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.clock.install();
  let expire = false;
  const releases: Array<() => void> = [];
  for (const endpoint of ['**/api/v1/saved', '**/api/v1/saved/1/runs?*']) {
    await page.route(endpoint, async route => {
      if (!expire) return route.continue();
      await new Promise<void>(resolve => releases.push(resolve));
      await route.fulfill({ status: 401, body: '' });
    });
  }
  let loginRequests = 0;
  let releaseLogin: (() => void) | undefined;
  await page.route('**/login?*', async route => {
    loginRequests++;
    await new Promise<void>(resolve => { releaseLogin = resolve; });
    await route.continue();
  });
  await page.route('**/api/auth/login', route => { expire = false; return route.fulfill({ json: identity }); });
  const destination = '/jobs/nets?net=1&ntab=runs&note=first#receipt';
  await page.goto(destination);
  await expect(page.locator(SEL.drawerPanel)).toBeVisible();
  // Wait for both helpers' first read to populate before expiring polling.
  await expect(page.locator('.nets-table')).toBeVisible();
  await expect(page.locator(SEL.netRunRow).first()).toBeVisible();
  expire = true;
  await page.clock.fastForward(5001);
  await expect.poll(() => releases.length).toBe(2);
  releases[0]();
  await expect.poll(() => Boolean(releaseLogin)).toBe(true);
  releases[1]();
  // The old document remains alive while its login request is held. Allow
  // the second callback to run before permitting the document transition.
  await page.waitForTimeout(150);
  expect(loginRequests).toBe(1);
  releaseLogin!();
  await expect(page).toHaveURL(url => url.search === `?return_to=${component(destination)}`);
  await signIn(page);
  await expect(page).toHaveURL(url => relative(url) === destination);
  expect(loginRequests).toBe(1);
});

test('a delayed Jobs 401 cannot redirect after its page owner is disposed', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.clock.install();
  let expire = false;
  let release: (() => void) | undefined;
  let completed = false;
  await page.route('**/api/v1/saved', async route => {
    if (!expire) return route.continue();
    await new Promise<void>(resolve => { release = resolve; });
    await route.fulfill({ status: 401, body: '' });
    completed = true;
  });
  let loginRequests = 0;
  page.on('request', req => { if (req.isNavigationRequest() && new URL(req.url()).pathname === '/login') loginRequests++; });
  await page.goto('/jobs/nets');
  await expect(page.locator('.nets-table')).toBeVisible();
  expire = true;
  await page.clock.fastForward(5001);
  await expect.poll(() => Boolean(release)).toBe(true);
  await page.locator('nav.rail a[title="Schema"]').click();
  await expect(page).toHaveURL(url => url.pathname === '/search/schema');
  release!();
  await expect.poll(() => completed).toBe(true);
  await page.waitForTimeout(150);
  await expect(page).toHaveURL(url => url.pathname === '/search/schema');
  expect(loginRequests).toBe(0);
});

test('initial session expiry captures URL changes made while the session check is pending', async ({ page }) => {
  let release: (() => void) | undefined;
  await page.route('**/api/auth/me', async route => {
    await new Promise<void>(resolve => { release = resolve; });
    await route.fulfill({ status: 401, body: '' });
  });
  await page.goto('/search/history?note=mounted');
  await expect.poll(() => Boolean(release)).toBe(true);
  const destination = '/search/history?note=current%27value#changed?row';
  await page.evaluate(value => history.replaceState(history.state, '', value), destination);
  release!();
  await expect(page).toHaveURL(url => url.search === `?return_to=${component(destination)}`);
});

test('a superseded drawer poll cannot redirect after its Runs tab becomes inactive', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.clock.install();
  let expire = false;
  let release: (() => void) | undefined;
  let completed = false;
  await page.route('**/api/v1/saved/1/runs?*', async route => {
    if (!expire) return route.continue();
    await new Promise<void>(resolve => { release = resolve; });
    await route.fulfill({ status: 401, body: '' });
    completed = true;
  });
  await page.goto('/jobs/nets?net=1&ntab=runs');
  await expect(page.locator(SEL.netRunRow).first()).toBeVisible();
  expire = true;
  await page.clock.fastForward(5001);
  await expect.poll(() => Boolean(release)).toBe(true);
  await page.locator(SEL.drawerPanel).getByRole('tab', { name: 'Query + Schedule', exact: true }).click();
  release!();
  await expect.poll(() => completed).toBe(true);
  await page.waitForTimeout(150);
  await expect(page).toHaveURL(url => url.pathname === '/jobs/nets' && url.searchParams.get('ntab') === 'query');
});

test('outer query byte boundary accepts the limit and falls back one byte above it', async ({ page }) => {
  await page.route('**/api/auth/login', route => route.fulfill({ json: identity }));
  const prefix = 'return_to=/search/history&ignored=';
  const at = prefix + 'x'.repeat(3 * 64 * 1024 + 64 - prefix.length);
  for (const [query, expected] of [[at, '/search/history'], [at + 'x', '/search']]) {
    await page.goto(`/login?${query}`);
    await signIn(page);
    await expect(page).toHaveURL(url => relative(url) === expected);
  }
});

test('inner malformed Search filters, relative range and URL-shaped data remain untouched', async ({ page }) => {
  await page.route('**/api/auth/login', route => route.fulfill({ json: identity }));
  const destination = '/search?q=service%3Dnginx&f=v1.!&r=1h&page=2&mode=live&next=https://outside.invalid/a%3Fb#row%23x';
  await page.goto(`/login?return_to=${component(destination)}`);
  await signIn(page);
  await expect(page).toHaveURL(url => relative(url) === destination);
  await expect(page.locator(SEL.urlNotice)).toBeVisible();
  await expect(page.locator(SEL.urlNotice)).toContainText(COPY.urlNoticeFiltersPrefix);
  await expect(page.locator(SEL.urlNoticeRaw)).toHaveText('v1.!');
});

for (const phase of ['login document pending', 'logout response pending'] as const) {
  test(`explicit logout wins over a late Jobs 401 with ${phase}`, async ({ page, request }) => {
    await resetScenario(request, 'corpus');
    await page.clock.install();
    let expire = false;
    let releasePoll: (() => void) | undefined;
    let pollDelivered = false;
    await page.route('**/api/v1/saved', async route => {
      if (!expire) return route.continue();
      await new Promise<void>(resolve => { releasePoll = resolve; });
      await route.fulfill({ status: 401, body: '' });
      pollDelivered = true;
    });

    let releaseLogout: (() => void) | undefined;
    await page.route('**/api/auth/logout', async route => {
      // The server has already completed logout. Optionally withhold its
      // response from the old document while that document receives a 401.
      const response = await route.fetch();
      if (phase === 'logout response pending') {
        await new Promise<void>(resolve => { releaseLogout = resolve; });
      }
      await route.fulfill({ response });
    });

    const attempts: string[] = [];
    const releaseDocuments: Array<() => void> = [];
    page.on('request', req => {
      const url = new URL(req.url());
      if (req.isNavigationRequest() && req.frame() === page.mainFrame() && url.pathname === '/login') {
        attempts.push(relative(url));
      }
    });
    await page.route('**/login*', async route => {
      if (!route.request().isNavigationRequest()) return route.continue();
      await new Promise<void>(resolve => { releaseDocuments.push(resolve); });
      // A competing navigation can cancel this request. Either way, retain
      // its attempted URL above so cancellation cannot conceal the defect.
      await route.continue().catch(() => {});
    });
    await page.route('**/api/auth/login', route => {
      expire = false;
      return route.fulfill({ json: identity });
    });

    await page.goto('/jobs/nets?note=explicit-logout#receipt');
    await expect(page.locator('.nets-table')).toBeVisible();
    expire = true;
    await page.clock.fastForward(5001);
    await expect.poll(() => Boolean(releasePoll)).toBe(true);
    await page.locator(SEL.topbarUser).click();
    await page.getByRole('menuitem', { name: 'Sign Out', exact: true }).click({ noWaitAfter: true });
    if (phase === 'login document pending') {
      await expect.poll(() => releaseDocuments.length).toBe(1);
      expect(attempts).toEqual(['/login']);
    } else {
      await expect.poll(() => Boolean(releaseLogout)).toBe(true);
      expect(attempts).toEqual([]);
    }

    releasePoll!();
    await expect.poll(() => pollDelivered).toBe(true);
    // An absence assertion needs a bounded quiet window after the response
    // reaches the browser. All document requests remain held during it.
    await page.waitForTimeout(250);
    if (releaseLogout) {
      releaseLogout();
      await expect.poll(() => attempts.includes('/login')).toBe(true);
    }
    await expect.poll(() => releaseDocuments.length).toBeGreaterThan(0);
    await expect.poll(() => releaseDocuments.length).toBe(attempts.length);
    for (const release of releaseDocuments) release();
    // Settle the real document navigation before checking the complete trace.
    await expect(page.getByRole('button', { name: 'Sign in', exact: true })).toBeVisible();
    expect(attempts).toEqual(['/login']);
    await expect(page).toHaveURL(url => relative(url) === '/login');
    await signIn(page);
    await expect(page).toHaveURL(url => relative(url) === '/search');
  });
}

for (const failure of ['server', 'network'] as const) {
  test(`failed explicit logout ${failure} releases suppression and preserves Jobs polling`, async ({ page, request }) => {
    await resetScenario(request, 'corpus');
    await page.clock.install();
    let expire = false;
    let rejectedReads = 0;
    await page.route('**/api/v1/saved', route => {
      if (!expire) return route.continue();
      rejectedReads++;
      return route.fulfill({ status: 401, body: '' });
    });
    let releaseLogout: (() => void) | undefined;
    await page.route('**/api/auth/logout', async route => {
      await new Promise<void>(resolve => { releaseLogout = resolve; });
      if (failure === 'network') await route.abort('failed');
      else await route.fulfill({ status: 502, body: '' });
    });
    await page.route('**/api/auth/login', route => {
      expire = false;
      return route.fulfill({ json: identity });
    });
    const destination = '/jobs/nets?note=failed-logout#receipt';
    await page.goto(destination);
    await expect(page.locator('.nets-table')).toBeVisible();
    await page.locator(SEL.topbarUser).click();
    await page.getByRole('menuitem', { name: 'Sign Out', exact: true }).click();
    await expect.poll(() => Boolean(releaseLogout)).toBe(true);
    expire = true;
    await page.clock.fastForward(5001);
    await expect.poll(() => rejectedReads).toBe(1);
    await page.waitForTimeout(250);
    await expect(page).toHaveURL(url => relative(url) === destination);
    releaseLogout!();
    await expect(page.getByRole('alert')).toContainText('Your session may still be active');
    await page.clock.fastForward(5001);
    await expect.poll(() => rejectedReads).toBe(2);
    await expect(page).toHaveURL(url => relative(url) === `/login?return_to=${component(destination)}`);
    await signIn(page);
    await expect(page).toHaveURL(url => relative(url) === destination);
  });
}
