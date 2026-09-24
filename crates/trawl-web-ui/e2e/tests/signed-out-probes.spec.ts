// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// A signed-out visit to a protected route sends no request that is certain
// to fail. The trawl-web proxy puts `/api/v1/health` behind a session, so the
// status bar must not mount (and probe health) before `/me` confirms one.
// The stub harness answers health without auth, so the test counts requests
// instead of waiting for a 401.

import { test, expect, resetScenario } from '../fixtures';

test('a signed-out deep link redirects to login without probing health', async ({ page, request }) => {
  await resetScenario(request, 'unauth');
  // Client-side oracle: also catches a request the redirect aborts before
  // the harness counts it.
  const healthRequests: string[] = [];
  page.on('request', r => {
    if (new URL(r.url()).pathname === '/api/v1/health') healthRequests.push(r.url());
  });
  await page.goto('/jobs/nets');
  await expect(page).toHaveURL(url =>
    url.pathname === '/login' && url.search === '?return_to=%2Fjobs%2Fnets'
  );
  // Allow any late effect on the login page to run before proving silence.
  await page.waitForTimeout(350);
  const hits = (await (await request.get('/__ctl/state')).json()).healthHits;
  expect(hits.health ?? 0, 'harness counted a health request').toBe(0);
  expect(healthRequests, 'browser sent a health request').toEqual([]);
});
