// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The recovering notice (ADR-0041): what the search page says when the
// server refuses a snapshot as 503 `corpus_recovering`.

import { readFileSync } from 'node:fs';
import { test, expect } from '../fixtures';
import { SEL, COPY } from '../selectors';
import type { Page, Route } from '@playwright/test';

const wire = (name: string) =>
  JSON.parse(readFileSync(`${__dirname}/../harness/wire/${name}.json`, 'utf8'));

const RECOVERING = wire('query-corpus-recovering');
const EVENTS_URL = '/search?q=service%3Dnginx&r=15m';
const CHART_URL = `/search?q=${encodeURIComponent('* | stats count() by host')}&r=15m`;

async function holdQueries(page: Page) {
  const parked: Route[] = [];
  await page.route('**/api/v1/query', route => {
    parked.push(route);
  });
  return {
    count: () => parked.length,
    answer: (i: number, status: number, json: unknown) => parked[i].fulfill({ status, json }),
  };
}

async function refuseQueries(page: Page) {
  let sent = 0;
  await page.route('**/api/v1/query', route => {
    sent += 1;
    return route.fulfill({ status: 503, json: RECOVERING });
  });
  return () => sent;
}

const notice = (page: Page) => page.locator(SEL.recoveringNotice);

async function expectNoOtherState(page: Page) {
  await expect(page.locator(SEL.resultsEmptyCell)).toHaveCount(0);
  await expect(page.locator(SEL.resultsPane).locator('table')).toHaveCount(0);
  await expect(page.locator(SEL.loadHintError)).toHaveCount(0);
  await expect(page.getByText(COPY.loadHintErrorPrefix, { exact: false })).toHaveCount(0);
  await expect(page.locator(SEL.queryErrorNotice)).toHaveCount(0);
}

test.describe('recovering notice', () => {
  test('Events names the state with the server sentence, never an empty result', async ({ page }, testInfo) => {
    const sent = await refuseQueries(page);
    await page.goto(EVENTS_URL);

    const alert = notice(page);
    await expect(alert).toBeVisible();
    expect(sent()).toBe(1);
    await expect(alert).toHaveAttribute('role', 'alert');
    await expect(alert.locator(SEL.recoveringNoticeTitle)).toHaveText(COPY.recoveringTitle);
    await expect(alert.locator(SEL.recoveringNoticeText)).toHaveText(RECOVERING.error.message);
    await expectNoOtherState(page);
    await expect(page.locator(SEL.statusLabel)).toHaveText('Recovering');
    const buttons = page.locator(SEL.resultsPane).getByRole('button');
    await expect(buttons).toHaveCount(1);
    await expect(buttons).toHaveAccessibleName('Retry');
    await alert.screenshot({ path: testInfo.outputPath('events-recovering-notice.png') });
    await page.screenshot({ path: testInfo.outputPath('events-recovering-page.png') });
  });

  test('Retry sends the request again and waits for its answer', async ({ page }) => {
    const queries = await holdQueries(page);
    await page.goto(EVENTS_URL);
    await expect.poll(queries.count).toBe(1);
    await queries.answer(0, 503, RECOVERING);
    await expect(notice(page)).toBeVisible();
    await expect(page.locator(SEL.statusLabel)).toHaveText('Recovering');

    await page.locator(SEL.resultsPane).getByRole('button', { name: 'Retry', exact: true }).click();
    await expect.poll(queries.count).toBe(2);
    await expect(notice(page)).toHaveCount(0);
    await expect(page.locator(SEL.loadHintError)).toHaveCount(0);
    await expect(page.locator(SEL.statusLabel)).toHaveText('Hauling');

    await queries.answer(1, 200, wire('query-rows'));
    await expect(page.locator(SEL.resultsPane).locator('tbody tr').first()).toBeVisible();
    await expect(notice(page)).toHaveCount(0);
    await expect(page.locator(SEL.statusLabel)).toContainText('Connected');
  });

  test('Visualization shows the same notice, not the snapshot failure copy', async ({ page }, testInfo) => {
    await refuseQueries(page);
    await page.goto(CHART_URL);
    await expect(notice(page)).toBeVisible();
    await page.getByRole('tab', { name: 'Visualization', exact: true }).click();

    const alert = notice(page);
    await expect(alert).toBeVisible();
    await expect(alert.locator(SEL.recoveringNoticeTitle)).toHaveText(COPY.recoveringTitle);
    await expect(alert.locator(SEL.recoveringNoticeText)).toHaveText(RECOVERING.error.message);
    await expect(page.getByText('Snapshot query failed.', { exact: false })).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'Retry snapshot' })).toHaveCount(0);
    await expect(page.getByText('Run a query to visualize its snapshot.')).toHaveCount(0);
    await expectNoOtherState(page);
    await alert.screenshot({ path: testInfo.outputPath('visualization-recovering-notice.png') });
  });

  test('a plain service_unavailable keeps the load failure copy', async ({ page }) => {
    await page.route('**/api/v1/query', route =>
      route.fulfill({
        status: 503,
        json: { error: { code: 'service_unavailable', message: 'service unavailable' } },
      }),
    );
    await page.goto(EVENTS_URL);

    await expect(page.locator(SEL.loadHintError)).toHaveText(/Couldn't load results: service unavailable/);
    await expect(notice(page)).toHaveCount(0);
    await expect(page.locator(SEL.statusLabel)).toHaveText('Search failed');
  });
});
