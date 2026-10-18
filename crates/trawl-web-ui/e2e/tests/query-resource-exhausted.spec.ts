// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The resource notice (ADR-0056): what the search page says when the
// server answers a snapshot with 503 `query_resource_exhausted`.
//
// The query ran and used up the memory or scratch space trawld gives
// queries. The text is fine, so nothing is underlined. Nothing ran to
// completion, so an empty table would claim there is nothing to find.
// The same request spends the same budget again, so the page never sends
// it again on its own and offers no Retry: the server's sentence says
// what helps.

import { readFileSync } from 'node:fs';
import { test, expect } from '../fixtures';
import { SEL, COPY } from '../selectors';
import type { Page, Route } from '@playwright/test';

const wire = (name: string) =>
  JSON.parse(readFileSync(`${__dirname}/../harness/wire/${name}.json`, 'utf8'));

/// The server's refusal, as trawld writes it.
/// e2e_wire_fixture_contract.rs pins the sentence.
const EXHAUSTED = wire('query-resource-exhausted');
const EVENTS_URL = '/search?q=service%3Dnginx&r=15m';
const CHART_URL = `/search?q=${encodeURIComponent('* | stats count() by host')}&r=15m`;

/// How long a case waits, after the notice is up, for a request the page
/// must not send. Longer than any retry delay the page uses elsewhere
/// would need to show itself once.
const QUIET_MS = 2_000;

/// Answer every query with the exhausted refusal, counting requests.
async function refuseQueries(page: Page) {
  let sent = 0;
  await page.route('**/api/v1/query', route => {
    sent += 1;
    return route.fulfill({ status: 503, json: EXHAUSTED });
  });
  return () => sent;
}

/// Park every POST /api/v1/query until the case answers it.
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

const notice = (page: Page) => page.locator(SEL.resourceNotice);

/// Nothing on the page underlines the text, claims an empty result, or
/// reads the refusal as a load failure or a query error.
async function expectOnlyTheNotice(page: Page) {
  // No syntax underline: neither the query error notice's caret excerpt
  // nor a lint mark in the editor.
  await expect(page.locator(SEL.queryErrorNotice)).toHaveCount(0);
  await expect(page.locator(SEL.queryErrorCaret)).toHaveCount(0);
  await expect(page.locator('.cm-lintRange')).toHaveCount(0);
  await expect(page.getByRole('img', { name: COPY.lintMarkerName, exact: true })).toHaveCount(0);
  // No empty-result claim.
  await expect(page.locator(SEL.resultsEmptyCell)).toHaveCount(0);
  await expect(page.locator(SEL.resultsPane).locator('table')).toHaveCount(0);
  await expect(page.getByText('No fish in this net yet')).toHaveCount(0);
  await expect(page.getByText(/\bno events\b/i)).toHaveCount(0);
  await expect(page.getByText(/\b0 (events|rows|results)\b/i)).toHaveCount(0);
  await expect(page.getByText('No rows to draw.')).toHaveCount(0);
  // Not a load failure.
  await expect(page.locator(SEL.loadHintError)).toHaveCount(0);
  await expect(page.getByText(COPY.loadHintErrorPrefix, { exact: false })).toHaveCount(0);
  await expect(page.locator(SEL.recoveringNotice)).toHaveCount(0);
}

test.describe('resource notice', () => {
  test('Events quotes the server sentence, sends nothing again and claims no empty result', async ({ page }, testInfo) => {
    const sent = await refuseQueries(page);
    await page.goto(EVENTS_URL);

    const alert = notice(page);
    await expect(alert).toBeVisible();
    await expect(alert).toHaveAttribute('role', 'alert');
    await expect(alert.locator(SEL.resourceNoticeTitle)).toHaveText(COPY.resourceTitle);
    await expect(alert.locator(SEL.resourceNoticeText)).toHaveText(EXHAUSTED.error.message);
    await expectOnlyTheNotice(page);
    // No Retry, and no other control in the region either.
    await expect(page.locator(SEL.resultsPane).getByRole('button')).toHaveCount(0);
    // The footer reports a failed request and counts nothing.
    await expect(page.locator(SEL.statusLabel)).toHaveText('Error');

    // No automatic retry: the one request stays the only one.
    expect(sent()).toBe(1);
    await page.waitForTimeout(QUIET_MS);
    expect(sent()).toBe(1);
    await expect(alert).toBeVisible();

    await alert.screenshot({ path: testInfo.outputPath('events-resource-notice.png') });
    await page.screenshot({ path: testInfo.outputPath('events-resource-page.png') });
  });

  test('Visualization shows the same notice, not the snapshot failure copy', async ({ page }, testInfo) => {
    const sent = await refuseQueries(page);
    await page.goto(CHART_URL);
    await expect(notice(page)).toBeVisible();
    await page.getByRole('tab', { name: 'Visualization', exact: true }).click();

    const alert = notice(page);
    await expect(alert).toBeVisible();
    await expect(alert.locator(SEL.resourceNoticeTitle)).toHaveText(COPY.resourceTitle);
    await expect(alert.locator(SEL.resourceNoticeText)).toHaveText(EXHAUSTED.error.message);
    await expect(page.getByText('Snapshot query failed.', { exact: false })).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'Retry snapshot' })).toHaveCount(0);
    await expect(page.getByText('Run a query to visualize its snapshot.')).toHaveCount(0);
    await expectOnlyTheNotice(page);

    // Switching tabs reads the same answer; it sends nothing.
    await page.waitForTimeout(QUIET_MS);
    expect(sent()).toBe(1);
    await alert.screenshot({ path: testInfo.outputPath('visualization-resource-notice.png') });
  });

  test('a new Haul waits for its own answer instead of the refusal it replaces', async ({ page }) => {
    const queries = await holdQueries(page);
    await page.goto(EVENTS_URL);
    await expect.poll(queries.count).toBe(1);
    await queries.answer(0, 503, EXHAUSTED);
    await expect(notice(page)).toBeVisible();

    // The reader asks again, by hand. While that request is out, the
    // refusal is not its verdict: no notice, and no load failure copy
    // for it either.
    await page.locator(SEL.runButton).click();
    await expect.poll(queries.count).toBe(2);
    await expect(notice(page)).toHaveCount(0);
    await expect(page.locator(SEL.loadHintError)).toHaveCount(0);

    await queries.answer(1, 200, wire('query-rows'));
    await expect(page.locator(SEL.resultsPane).locator('tbody tr').first()).toBeVisible();
    await expect(notice(page)).toHaveCount(0);
    await expect(page.locator(SEL.statusLabel)).toContainText('Connected');
  });
});
