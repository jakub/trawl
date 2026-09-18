// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, identityFor } from './auth-server';
import { SEL } from '../selectors';
import { writeFile } from 'node:fs/promises';

type Lifecycle = { kind: string; persisted: boolean; id: string; path: string; visibility: string; oldIdentityPresent: boolean };

for (const departure of ['explicit logout', 'automatic 401'] as const) {
  for (const session of ['expired', 'changed identity'] as const) {
    test(`Back after ${departure} checks the current session for ${session}`, async ({ page, authServer }, testInfo) => {
      const lifecycle: Lifecycle[] = [];
      const errors: string[] = [];
      const cacheRejections: unknown[] = [];
      const cdp = await page.context().newCDPSession(page);
      await cdp.send('Page.enable');
      cdp.on('Page.backForwardCacheNotUsed', event => cacheRejections.push(event));
      page.on('pageerror', error => errors.push(error.message));
      page.on('console', message => {
        if (message.text().startsWith('AUTH-BFCACHE:')) lifecycle.push(JSON.parse(message.text().slice('AUTH-BFCACHE:'.length)));
      });
      // This observer runs before the WASM listener. On persisted pageshow
      // the root must ALREADY be hidden, before any restore callback runs.
      await page.addInitScript(() => {
        const id = crypto.randomUUID();
        (window as unknown as { authDocumentId: string }).authDocumentId = id;
        for (const kind of ['pagehide', 'pageshow']) {
          window.addEventListener(kind, event => {
            console.log('AUTH-BFCACHE:' + JSON.stringify({
              kind, persisted: (event as PageTransitionEvent).persisted, id,
              path: location.pathname,
              visibility: getComputedStyle(document.documentElement).visibility,
              oldIdentityPresent: document.body?.textContent?.includes('previous-identity') ?? false,
            }));
          });
        }
      });
      try {
        await page.goto(authServer.origin + (departure === 'automatic 401' ? '/jobs/nets' : '/jobs/runs'));
        await expect(page.locator(departure === 'automatic 401' ? '.nets-table' : '.runs-table')).toBeVisible();
        await expect(page.locator(SEL.topbarUser)).toContainText('previous-identity');
        const oldId = await page.evaluate(() => (window as unknown as { authDocumentId: string }).authDocumentId);
        if (departure === 'automatic 401') {
          // The automatic transition replaces the latest entry. An earlier
          // real SPA navigation entry is needed to restore that document.
          await page.locator('nav.rail a[title="Runs"]').click();
          await expect(page.locator('.runs-table')).toBeVisible();
          expect(await page.evaluate(() => (window as unknown as { authDocumentId: string }).authDocumentId)).toBe(oldId);
          authServer.identity = null;
        } else {
          await page.locator(SEL.topbarUser).click();
          await page.getByRole('menuitem', { name: 'Sign Out', exact: true }).click();
        }
        await expect(page).toHaveURL(url => url.pathname === '/login');
        await expect(page.getByRole('button', { name: 'Sign in', exact: true })).toBeVisible();
        authServer.identity = session === 'expired' ? null : identityFor('current-identity');
        authServer.holdMe = true;
        const readsBefore = authServer.requests.filter(req => req.path === '/api/auth/me').length;
        await page.goBack({ waitUntil: 'commit' });
        if (departure === 'explicit logout') {
          // These cases are the mandatory cached-heap regression proof.
          await expect.poll(() => lifecycle.some(event => event.kind === 'pageshow' && event.persisted && event.id === oldId)).toBe(true);
        }
        await expect.poll(() => authServer.pendingMe).toBe(1);
        const restored = lifecycle.find(event => event.kind === 'pageshow' && event.persisted && event.id === oldId);
        if (restored) {
          expect(restored.visibility).toBe('hidden');
          expect(restored.oldIdentityPresent).toBe(true);
        }
        // Chromium may decline to cache the automatic replace navigation.
        // Either path must reach a fresh document and its real session gate.
        expect(authServer.requests.filter(req => req.path === '/api/auth/me').length).toBe(readsBefore + 1);
        expect(await page.evaluate(() => (window as unknown as { authDocumentId: string }).authDocumentId)).not.toBe(oldId);
        // Fresh document, session gate still pending: no previous rows or
        // identity can appear while the current cookie is being checked.
        await expect(page.locator('.runs-table, .nets-table')).toHaveCount(0);
        await expect(page.getByText('previous-identity', { exact: true })).toHaveCount(0);
        await expect(page.getByText('Checking your session…', { exact: true })).toBeVisible();
        authServer.releaseMe();
        if (session === 'expired') {
          await expect(page).toHaveURL(url => url.pathname === '/login');
          await expect(page.getByRole('button', { name: 'Sign in', exact: true })).toBeVisible();
        } else {
          await expect(page.locator(SEL.topbarUser)).toContainText('current-identity');
          await expect(page.locator(departure === 'automatic 401' ? '.nets-table' : '.runs-table')).toBeVisible();
          await expect(page.getByText('previous-identity', { exact: true })).toHaveCount(0);
        }
        expect(errors).toEqual([]);
      } finally {
        const navigation = await page.evaluate(() => {
          const entry = performance.getEntriesByType('navigation')[0] as PerformanceNavigationTiming & { notRestoredReasons?: { toJSON(): unknown } };
          return { url: location.href, id: (window as unknown as { authDocumentId: string }).authDocumentId, notRestoredReasons: entry?.notRestoredReasons?.toJSON() };
        }).catch(() => null);
        const evidencePath = testInfo.outputPath('auth-bfcache-evidence.json');
        await writeFile(evidencePath, JSON.stringify({ lifecycle, requests: authServer.requests, errors, navigation, cacheRejections }, null, 2));
        await testInfo.attach('auth-bfcache-evidence', {
          path: evidencePath,
          contentType: 'application/json',
        });
      }
    });
  }
}
