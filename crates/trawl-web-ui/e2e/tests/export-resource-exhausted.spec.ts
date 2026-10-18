// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The export modal and the resource refusal (ADR-0056): what the reader
// sees when the server answers an export with 503 `query_resource_exhausted`.
//
// The export ran the query on the same memory and scratch budget the
// search does, so it can be refused the same way. The modal quotes the
// server's sentence through the search page's notice, offers no automatic
// retry (the same request spends the same budget again), and does not
// raise the generic "Export failed" toast.

import { readFileSync } from 'node:fs';
import { test, expect } from '../fixtures';
import { SEL, COPY } from '../selectors';

const wire = (name: string) =>
  JSON.parse(readFileSync(`${__dirname}/../harness/wire/${name}.json`, 'utf8'));

/// The server's refusal, as trawld writes it.
/// e2e_wire_fixture_contract.rs pins the sentence.
const EXHAUSTED = wire('query-resource-exhausted');

/// How long the case waits, after the notice is up, for a request the
/// modal must not send.
const QUIET_MS = 2_000;

test('an export refused for resources shows the server sentence in the modal and sends nothing again', async ({ page }) => {
  let sent = 0;
  await page.route('**/api/v1/export*', route => {
    sent += 1;
    return route.fulfill({ status: 503, json: EXHAUSTED });
  });
  await page.goto('/search?q=service%3Dnginx');
  await page.locator(SEL.exportAction).click();
  const modal = page.locator(SEL.modalPanel);
  await expect(modal).toBeVisible();
  await modal.getByRole('button', { name: 'Download', exact: true }).click();

  const alert = page.locator(SEL.modalResourceNotice);
  await expect(alert).toBeVisible();
  await expect(alert.locator(SEL.resourceNoticeTitle)).toHaveText(COPY.resourceTitle);
  await expect(alert.locator(SEL.resourceNoticeText)).toHaveText(EXHAUSTED.error.message);
  // The modal stays open for the reader to change the range or the query,
  // and the refusal is not announced twice as the generic failure.
  await expect(modal).toBeVisible();
  await expect(page.locator(SEL.toastError)).toHaveCount(0);
  await expect(page.getByText('server returned 503')).toHaveCount(0);
  await expect(modal.getByRole('button', { name: 'Download', exact: true })).toBeEnabled();

  expect(sent).toBe(1);
  await page.waitForTimeout(QUIET_MS);
  expect(sent).toBe(1);
});

test('the notice gives way to the next attempt', async ({ page }) => {
  let sent = 0;
  await page.route('**/api/v1/export*', route => {
    sent += 1;
    if (sent === 1) return route.fulfill({ status: 503, json: EXHAUSTED });
    return route.fulfill({
      status: 200,
      body: 'service,count\nnginx,1\n',
      headers: { 'content-type': 'text/csv', 'content-disposition': 'attachment; filename="after.csv"' },
    });
  });
  await page.goto('/search?q=service%3Dnginx');
  await page.locator(SEL.exportAction).click();
  const modal = page.locator(SEL.modalPanel);
  await modal.getByRole('button', { name: 'Download', exact: true }).click();
  await expect(page.locator(SEL.modalResourceNotice)).toBeVisible();

  await modal.getByRole('button', { name: 'Download', exact: true }).click();
  await expect(page.locator(SEL.toastSuccess)).toContainText('Exported');
  await expect(page.locator(SEL.modalResourceNotice)).toHaveCount(0);
  expect(sent).toBe(2);
});
