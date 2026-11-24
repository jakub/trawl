// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import fs from 'node:fs';
import { test, expect, resetScenario, CORPUS } from '../fixtures';
import { SEL } from '../selectors';
import type { Locator, Page } from '@playwright/test';

const presentation = JSON.parse(
  fs.readFileSync('harness/wire/query-field-presentation.json', 'utf8'),
);
const COLUMNS: string[] = presentation.columns.map((c: { name: string }) => c.name);
const KEYS: string[] = presentation.columns.map((c: { name: string; type: string }) =>
  c.type === 'timestamp' ? `${c.name} UTC` : c.name,
);
const RESERVED = ['_time', '_ingested', '_raw'];
const NULLS = [0, 7, 3];

async function openPresentation(page: Page) {
  await page.route('**/api/v1/query', (route) => route.fulfill({ json: presentation }));
  await page.goto('/search?q=service%3Dapi&page=0');
  await expect(page.locator(SEL.resultsRow)).toHaveCount(presentation.rows.length);
}

async function pickMode(page: Page, option: string) {
  await page.locator(SEL.viewControl).click();
  await expect(page.locator(SEL.viewPanel)).toBeVisible();
  await page.locator(SEL.viewPanel).getByRole('button', { name: option, exact: true }).click();
  await page.keyboard.press('Escape');
  await expect(page.locator(SEL.viewPanel)).toHaveCount(0);
}

function nullLabel(open: boolean, n: number): string {
  return `${open ? 'Hide' : 'Show'} ${n} null field${n === 1 ? '' : 's'}`;
}

async function expectControlsGrid(toggle: Locator, view: Locator) {
  const id = await toggle.getAttribute('aria-controls');
  expect(id, 'the disclosure must name the grid it controls').toBeTruthy();
  await expect(view.locator(`[id="${id}"]`)).toHaveClass(/\bdg\b/);
}

test('reserved rows offer no value filter, inline or in the inspector', async ({ page }) => {
  await openPresentation(page);

  await page.locator(SEL.resultsExpandControl).nth(0).click();
  const detail = page.locator(SEL.resultsDetailCell);
  await expect(detail.locator('.dg')).toBeVisible();
  for (const field of RESERVED) {
    await expect(detail.getByRole('button', { name: new RegExp(`^Include ${field} = `) })).toHaveCount(0);
  }
  await expect(detail).toContainText('2026-09-01 10:00:00');
  await expect(detail).not.toContainText(presentation.rows[0][0]);
  await expect(page.locator(SEL.resultsDetailTag)).toHaveCount(COLUMNS.length - RESERVED.length);
  await expect(
    detail.getByRole('button', { name: 'Include timestamp = 2026-09-01T10:00:00.123Z', exact: true }),
  ).toHaveCount(1);
  await expect(detail.getByRole('button', { name: 'Copy _raw', exact: true })).toBeVisible();

  await pickMode(page, 'Inspector');
  await page.locator(SEL.resultsExpandControl).nth(0).click();
  const inspector = page.locator(SEL.inspector);
  await expect(inspector).toBeVisible();
  for (const field of RESERVED) {
    await expect(
      inspector.getByRole('button', { name: new RegExp(`^(Include|Exclude) ${field} = `) }),
    ).toHaveCount(0);
  }
  await expect(inspector.getByRole('button', { name: 'Copy', exact: true })).toHaveCount(COLUMNS.length);
  await expect(inspector.getByRole('button', { name: /^Include / })).toHaveCount(
    COLUMNS.length - RESERVED.length,
  );
  await expect(inspector.getByRole('button', { name: /^Exclude / })).toHaveCount(
    COLUMNS.length - RESERVED.length,
  );
  await expect(inspector.getByRole('button', { name: 'Copy _raw', exact: true })).toBeVisible();
});

test('the inline expansion folds null fields behind a keyboard disclosure', async ({ page }) => {
  await openPresentation(page);
  const detail = page.locator(SEL.resultsDetailCell);
  const keys = page.locator(SEL.resultsDetailFieldName);
  const toggle = page.locator(SEL.resultsDetailNullToggle);

  await page.locator(SEL.resultsExpandControl).nth(0).click();
  await expect(keys).toHaveCount(COLUMNS.length);
  await expect(toggle).toHaveCount(0);

  await page.locator(SEL.resultsExpandControl).nth(1).click();
  await expect(keys).toHaveCount(COLUMNS.length - NULLS[1]);
  await expect(toggle).toHaveText(nullLabel(false, NULLS[1]));
  await expect(toggle).toHaveAttribute('aria-expanded', 'false');
  await expect(toggle).toHaveJSProperty('tagName', 'BUTTON');
  await expectControlsGrid(toggle, detail);
  const tagsClosed = await page.locator(SEL.resultsDetailTag).count();

  await toggle.focus();
  await page.keyboard.press('Enter');
  await expect(toggle).toHaveAttribute('aria-expanded', 'true');
  await expect(toggle).toHaveText(nullLabel(true, NULLS[1]));
  await expect(keys).toHaveText(KEYS);
  await expect(detail.getByRole('button', { name: /^Include user = / })).toHaveCount(0);
  await expect(page.locator(SEL.resultsDetailTag)).toHaveCount(tagsClosed);

  await page.keyboard.press('Enter');
  await expect(toggle).toHaveAttribute('aria-expanded', 'false');
  await expect(keys).toHaveCount(COLUMNS.length - NULLS[1]);
  await page.keyboard.press(' ');
  await expect(toggle).toHaveAttribute('aria-expanded', 'true');
  await expect(keys).toHaveCount(COLUMNS.length);

  await page.locator(SEL.resultsExpandControl).nth(2).click();
  await expect(toggle).toHaveText(nullLabel(true, NULLS[2]));
  await expect(toggle).toHaveAttribute('aria-expanded', 'true');
  await expect(keys).toHaveCount(COLUMNS.length);
});

test('the null-field disclosure stays open across a re-haul of the same query', async ({ page }) => {
  let queries = 0;
  await page.route('**/api/v1/query', (route) => {
    queries += 1;
    return route.fulfill({ json: presentation });
  });
  await page.goto('/search?q=service%3Dapi&page=0');
  await expect(page.locator(SEL.resultsRow)).toHaveCount(presentation.rows.length);
  const keys = page.locator(SEL.resultsDetailFieldName);
  const toggle = page.locator(SEL.resultsDetailNullToggle);

  await page.locator(SEL.resultsExpandControl).nth(1).click();
  await toggle.click();
  await expect(toggle).toHaveText(nullLabel(true, NULLS[1]));
  await expect(keys).toHaveCount(COLUMNS.length);

  const before = queries;
  await page.locator(SEL.runButton).click();
  await expect.poll(() => queries).toBeGreaterThan(before);
  await expect(page.locator(SEL.resultsRow)).toHaveCount(presentation.rows.length);

  await expect(page.locator(SEL.resultsDetailCell)).toHaveCount(0);
  await page.locator(SEL.resultsExpandControl).nth(1).click();
  await expect(toggle).toHaveText(nullLabel(true, NULLS[1]));
  await expect(toggle).toHaveAttribute('aria-expanded', 'true');
  await expect(keys).toHaveText(KEYS);
});

test('the inspector folds null fields and keeps the choice across events', async ({ page }) => {
  await openPresentation(page);
  await pickMode(page, 'Inspector');
  const inspector = page.locator(SEL.inspector);
  const keys = page.locator(SEL.inspectorFieldName);
  const toggle = page.locator(SEL.inspectorNullToggle);

  await page.locator(SEL.resultsExpandControl).nth(1).click();
  await expect(inspector).toBeVisible();
  await expect(keys).toHaveCount(COLUMNS.length - NULLS[1]);
  await expect(toggle).toHaveText(nullLabel(false, NULLS[1]));
  await expect(toggle).toHaveAttribute('aria-expanded', 'false');
  await expectControlsGrid(toggle, inspector);

  await toggle.focus();
  await page.keyboard.press(' ');
  await expect(toggle).toHaveAttribute('aria-expanded', 'true');
  await expect(toggle).toHaveText(nullLabel(true, NULLS[1]));
  await expect(keys).toHaveText(KEYS);
  await expect(inspector.getByRole('button', { name: /^(Include|Exclude) user = / })).toHaveCount(0);

  await page.keyboard.press('Enter');
  await expect(toggle).toHaveAttribute('aria-expanded', 'false');
  await expect(keys).toHaveCount(COLUMNS.length - NULLS[1]);
  await page.keyboard.press('Enter');
  await expect(toggle).toHaveAttribute('aria-expanded', 'true');

  await page.locator(SEL.resultsExpandControl).nth(2).click();
  await expect(inspector).toContainText('Event 3');
  await expect(toggle).toHaveText(nullLabel(true, NULLS[2]));
  await expect(keys).toHaveCount(COLUMNS.length);

  await page.locator(SEL.resultsExpandControl).nth(0).click();
  await expect(inspector).toContainText('Event 1');
  await expect(keys).toHaveCount(COLUMNS.length);
  await expect(toggle).toHaveCount(0);
  await page.locator(SEL.resultsExpandControl).nth(1).click();
  await expect(inspector).toContainText('Event 2');
  await expect(toggle).toHaveText(nullLabel(true, NULLS[1]));
});

// Schema samples come from the parquet footers, and the server renders a
// timestamp column's bounds as text by the file's logical type. Compaction
// writes `_time` as non-UTC microseconds, so the text is fixed-width with
// six fraction digits and no `Z`, never the raw epoch-microsecond integer.
test('the schema Fields pane shows _time bounds as formatted times', async ({ page, request }) => {
  const min = '2026-09-01T00:00:00.000000';
  const max = '2026-09-01T23:59:59.999999';
  await resetScenario(request, 'corpus');
  await page.goto(`/search/schema?svc=${CORPUS.service}&stab=fields`);

  const row = page.locator(SEL.serviceFieldRow).filter({ hasText: '_time' });
  await expect(row).toHaveCount(1);
  await expect(row.locator(SEL.serviceFieldSample)).toHaveText(`${min} … ${max}`);

  await row.locator(SEL.rowStretch).click();
  const range = page.locator(SEL.serviceFieldStat).filter({ hasText: 'Range' });
  await expect(range).toHaveText(`Range${min} → ${max}`);
  await expect(range).not.toContainText(/\b\d{16}\b/);
});
