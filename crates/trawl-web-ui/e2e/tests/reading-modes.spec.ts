// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The two optional reading modes behind the result header's "View"
// disclosure (ADR-0032).

import { test, expect, resetScenario, CORPUS } from '../fixtures';
import { SEL } from '../selectors';
import type { Page } from '@playwright/test';

const CORPUS_URL = `/search?q=service%3D${CORPUS.service}&page=0`;

async function pickMode(page: Page, option: string) {
  await page.locator(SEL.viewControl).click();
  await expect(page.locator(SEL.viewPanel)).toBeVisible();
  await page.locator(SEL.viewPanel).getByRole('button', { name: option, exact: true }).click();
  // The popover stays open on a choice; close it so it cannot cover the
  // header controls the rest of a test presses.
  await page.keyboard.press('Escape');
  await expect(page.locator(SEL.viewPanel)).toHaveCount(0);
}

test('the View disclosure offers two named groups and closes on Escape', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(CORPUS_URL);

  const control = page.locator(SEL.viewControl);
  await expect(control).toHaveAttribute('type', 'button');
  await expect(control).toHaveAttribute('aria-expanded', 'false');
  await expect(page.locator(SEL.viewPanel)).toHaveCount(0);

  await control.click();
  await expect(control).toHaveAttribute('aria-expanded', 'true');
  const panel = page.locator(SEL.viewPanel);
  await expect(panel.getByRole('group', { name: 'Details' })).toBeVisible();
  await expect(panel.getByRole('group', { name: 'Rows' })).toBeVisible();

  await page.keyboard.press('Escape');
  await expect(panel).toHaveCount(0);
  await expect(control).toBeFocused();
});

test('both modes default off and leave the results DOM as it was', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(CORPUS_URL);

  // One row per event, no selection, no inspector, no message-first
  // table: exactly what row-controls.spec.ts and chrome-controls.spec.ts
  // count.
  await expect(page.locator(SEL.resultsRow)).toHaveCount(CORPUS.rowCount);
  await expect(page.locator(SEL.resultsSelectedRow)).toHaveCount(0);
  await expect(page.locator(SEL.inspector)).toHaveCount(0);
  await expect(page.locator('.results-table.msg-first')).toHaveCount(0);

  const caret = page.locator(SEL.resultsExpandControl).nth(1);
  await expect(caret).not.toHaveAttribute('aria-controls', /./);
  await caret.click();
  await expect(page.locator(SEL.resultsDetailCell)).toHaveCount(1);
});

test('inspector mode opens the selected event beside the table', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(CORPUS_URL);
  await pickMode(page, 'Inspector');

  await page.locator(SEL.resultsExpandControl).nth(1).click();

  await expect(page.locator(SEL.resultsSelectedRow)).toHaveCount(1);
  await expect(page.locator(SEL.resultsDetailCell)).toHaveCount(0);
  await expect(page.locator(SEL.resultsRow)).toHaveCount(CORPUS.rowCount);

  const inspector = page.locator(SEL.inspector);
  await expect(inspector).toBeVisible();
  await expect(inspector).toHaveAttribute('role', 'dialog');
  await expect(inspector).toContainText('Event 2');
  for (const column of CORPUS.columns) {
    await expect(inspector).toContainText(column);
  }

  // `_time` keeps only its Copy: the Range picker owns time, so its row
  // offers no value filter.
  await expect(page.locator(SEL.inspectorTag)).toHaveCount(CORPUS.columns.length * 3 - 2);
  await expect(inspector.getByRole('button', { name: 'Include host = web-02' })).toHaveCount(1);
  await expect(inspector.getByRole('button', { name: 'Exclude host = web-02' })).toHaveCount(1);
  await expect(inspector.getByRole('button', { name: /^(Include|Exclude) _time = / })).toHaveCount(0);

  await page.locator(SEL.resultsPane).focus();
  await page.keyboard.press('Escape');
  await expect(page.locator(SEL.inspector)).toHaveCount(0);
  await expect(page.locator(SEL.resultsSelectedRow)).toHaveCount(0);
});

test('closing a docked inspector hands focus back to the control that opened it', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(CORPUS_URL);
  await pickMode(page, 'Inspector');

  const second = page.locator(SEL.resultsExpandControl).nth(1);
  await second.click();
  await expect(page.locator(SEL.inspector)).toContainText('Event 2');

  await page.locator(`${SEL.inspector} ${SEL.drawerClose}`).click();
  await expect(page.locator(SEL.inspector)).toHaveCount(0);
  await expect(second).toBeFocused();

  await page.keyboard.press('j');
  await expect(page.locator(SEL.inspector)).toContainText('Event 1');

  const third = page.locator(SEL.resultsExpandControl).nth(2);
  await third.click();
  await expect(page.locator(SEL.inspector)).toContainText('Event 3');
  await page.locator(`${SEL.inspector} ${SEL.drawerClose}`).focus();
  await page.keyboard.press('Escape');
  await expect(page.locator(SEL.inspector)).toHaveCount(0);
  await expect(third).toBeFocused();
  await page.keyboard.press('j');
  await expect(page.locator(SEL.inspector)).toContainText('Event 1');
});

test('j and k walk the sorted order without moving the open event', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(CORPUS_URL);
  await pickMode(page, 'Inspector');

  await page.locator(SEL.resultsExpandControl).first().click();
  await expect(page.locator(SEL.inspector)).toContainText('Event 1');

  await page.locator(SEL.resultsPane).focus();
  await page.keyboard.press('j');
  await expect(page.locator(SEL.inspector)).toContainText('Event 2');
  await page.keyboard.press('j');
  await expect(page.locator(SEL.inspector)).toContainText('Event 3');
  await page.keyboard.press('k');
  await expect(page.locator(SEL.inspector)).toContainText('Event 2');
  await expect(page.locator(SEL.resultsSelectedRow)).toHaveCount(1);

  await page.locator(SEL.resultsSortControl).nth(1).click();
  await expect(page.locator(SEL.inspector)).toContainText('Event 2');
  await expect(page.locator(SEL.resultsSelectedRow)).toHaveCount(1);
});

test('a page turn closes the inspector rather than repointing it', async ({ page, request }) => {
  await resetScenario(request, 'pagination');
  await page.goto('/search?q=service%3Dnginx');
  await pickMode(page, 'Inspector');

  await page.locator(SEL.resultsExpandControl).nth(1).click();
  await expect(page.locator(SEL.inspector)).toContainText('Event 2');

  await page.locator('.results .results-footer').getByRole('button', { name: 'Next' }).click();
  await expect(page).toHaveURL(/[?&]page=1(?:&|$)/);

  await expect(page.locator(SEL.inspector)).toHaveCount(0);
  await expect(page.locator(SEL.resultsSelectedRow)).toHaveCount(0);
});

test('below 900px the inspector stacks under the table behind a jump link', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.setViewportSize({ width: 720, height: 900 });
  await page.goto(CORPUS_URL);
  await pickMode(page, 'Inspector');

  await page.locator(SEL.resultsExpandControl).nth(1).click();
  const jump = page.locator('a.jump-details');
  await expect(jump).toHaveCount(1);
  await expect(jump).toBeVisible();
  await expect(jump).toHaveAttribute('href', '#search-inspector');

  // The link is what carries a keyboard reader down to the panel, which
  // is `tabindex="-1"` and would not take focus from the href alone.
  await jump.click();
  await expect(page.locator(SEL.inspector)).toBeFocused();
});

test('message-first rows lead with the message and keep the rest reachable', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(CORPUS_URL);
  await pickMode(page, 'Message first');

  const table = page.locator('.results-table.msg-first');
  await expect(table).toHaveCount(1);
  await expect(page.locator(SEL.resultsRow)).toHaveCount(CORPUS.rowCount);

  // The corpus response declares no severity column, so the row leads
  // with `_time` and `message`; `status` leaves the row entirely.
  const headers = table.locator('thead th.sortable');
  await expect(headers).toHaveCount(2);
  await expect(headers.nth(0)).toContainText('_time');
  await expect(headers.nth(1)).toContainText('message');

  await expect(table.locator('tbody .mf-meta').first()).toHaveText(`host ${CORPUS.firstHost}`);

  await page.locator(SEL.resultsExpandControl).first().click();
  await expect(page.locator(SEL.resultsDetailCell)).toContainText('status');
});

for (const width of [720, 1100]) {
  test(`Runs Drawer close and Escape restore its opener at ${width}px`, async ({ page, request }) => {
    await resetScenario(request, 'corpus');
    await page.setViewportSize({ width, height: 900 });
    await page.goto('/jobs/runs');
    const opener = page.locator('.runs-table .row-stretch').first();
    await opener.click();
    const detail = page.locator(SEL.runDetail);
    await expect(detail).toHaveRole('dialog');
    await expect(detail).not.toHaveAttribute('aria-modal', 'true');
    await expect(detail).toHaveAccessibleName(new RegExp(CORPUS.netName));
    await expect(detail.locator('.sd-tabs')).toBeHidden();
    await detail.getByRole('button', { name: 'Close', exact: true }).click();
    await expect(detail).toHaveCount(0);
    await expect(page).toHaveURL(/\/jobs\/runs$/);
    await expect(opener).toBeFocused();
    await opener.press('Enter');
    await expect(detail).toBeVisible();
    await detail.getByRole('button', { name: 'Close', exact: true }).focus();
    await page.keyboard.press('Escape');
    await expect(detail).toHaveCount(0);
    await expect(opener).toBeFocused();
  });
}
