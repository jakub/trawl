// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// How the two detail views present an event's fields (issue 238).
//
// 1. An instant or the raw event (`_time`, `_ingested`, `_raw`) is never
//    a value filter. The Range picker owns time, and a filter on the
//    whole raw event is a text search. Copy stays: per field in the
//    inspector, and as the Copy _raw action in both views.
// 2. A null field is folded behind one "Show N null fields" button. It
//    is a real button, so Enter and Space both work, and its open state
//    belongs to the results view: selecting another event keeps it, and
//    an event with no null field shows no button at all.
//
// The page is `wire/query-field-presentation.json`, pinned in
// tests/e2e_wire_fixture_contract.rs: event 1 has no null field, event 2
// has seven, event 3 has three.

import fs from 'node:fs';
import { test, expect, resetScenario, CORPUS } from '../fixtures';
import { SEL } from '../selectors';
import type { Locator, Page } from '@playwright/test';

const presentation = JSON.parse(
  fs.readFileSync('harness/wire/query-field-presentation.json', 'utf8'),
);
const COLUMNS: string[] = presentation.columns.map((c: { name: string }) => c.name);
const RESERVED = ['_time', '_ingested', '_raw'];
/** Null fields per event, by row index. */
const NULLS = [0, 7, 3];

async function openPresentation(page: Page) {
  await page.route('**/api/v1/query', (route) => route.fulfill({ json: presentation }));
  await page.goto('/search?q=service%3Dapi&page=0');
  await expect(page.locator(SEL.resultsRow)).toHaveCount(presentation.rows.length);
}

/** Open the View popover and press one option by its visible label. */
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

/** The disclosure names the grid it controls, and that grid is the one in this view. */
async function expectControlsGrid(toggle: Locator, view: Locator) {
  const id = await toggle.getAttribute('aria-controls');
  expect(id, 'the disclosure must name the grid it controls').toBeTruthy();
  await expect(view.locator(`[id="${id}"]`)).toHaveClass(/\bdg\b/);
}

test('reserved rows offer no value filter, inline or in the inspector', async ({ page }) => {
  await openPresentation(page);

  // Inline: the reserved values render as text, not as Include tags.
  await page.locator(SEL.resultsExpandControl).nth(0).click();
  const detail = page.locator(SEL.resultsDetailCell);
  await expect(detail.locator('.dg')).toBeVisible();
  for (const field of RESERVED) {
    await expect(detail.getByRole('button', { name: new RegExp(`^Include ${field} = `) })).toHaveCount(0);
  }
  await expect(detail).toContainText(presentation.rows[0][0]);
  // Every other field keeps its tag, the sender's own `timestamp` among them.
  await expect(page.locator(SEL.resultsDetailTag)).toHaveCount(COLUMNS.length - RESERVED.length);
  await expect(
    detail.getByRole('button', { name: 'Include timestamp = 2026-09-01T10:00:00.123Z', exact: true }),
  ).toHaveCount(1);
  await expect(detail.getByRole('button', { name: 'Copy _raw', exact: true })).toBeVisible();

  // Inspector: Copy on every row, Include and Exclude on all but the three.
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

  // An event with no null field has no disclosure.
  await page.locator(SEL.resultsExpandControl).nth(0).click();
  await expect(keys).toHaveCount(COLUMNS.length);
  await expect(toggle).toHaveCount(0);

  // The sparse event: its valued fields, then the count of the rest.
  await page.locator(SEL.resultsExpandControl).nth(1).click();
  await expect(keys).toHaveCount(COLUMNS.length - NULLS[1]);
  await expect(toggle).toHaveText(nullLabel(false, NULLS[1]));
  await expect(toggle).toHaveAttribute('aria-expanded', 'false');
  await expect(toggle).toHaveJSProperty('tagName', 'BUTTON');
  await expectControlsGrid(toggle, detail);
  const tagsClosed = await page.locator(SEL.resultsDetailTag).count();

  // Enter opens it: every field, in wire order, the nulls in their places.
  await toggle.focus();
  await page.keyboard.press('Enter');
  await expect(toggle).toHaveAttribute('aria-expanded', 'true');
  await expect(toggle).toHaveText(nullLabel(true, NULLS[1]));
  await expect(keys).toHaveText(COLUMNS);
  // A revealed null row is text, never a filter.
  await expect(detail.getByRole('button', { name: /^Include user = / })).toHaveCount(0);
  await expect(page.locator(SEL.resultsDetailTag)).toHaveCount(tagsClosed);

  // Enter closes it, and Space opens it again.
  await page.keyboard.press('Enter');
  await expect(toggle).toHaveAttribute('aria-expanded', 'false');
  await expect(keys).toHaveCount(COLUMNS.length - NULLS[1]);
  await page.keyboard.press(' ');
  await expect(toggle).toHaveAttribute('aria-expanded', 'true');
  await expect(keys).toHaveCount(COLUMNS.length);

  // Selecting another event keeps it open.
  await page.locator(SEL.resultsExpandControl).nth(2).click();
  await expect(toggle).toHaveText(nullLabel(true, NULLS[2]));
  await expect(toggle).toHaveAttribute('aria-expanded', 'true');
  await expect(keys).toHaveCount(COLUMNS.length);
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

  // Space opens it.
  await toggle.focus();
  await page.keyboard.press(' ');
  await expect(toggle).toHaveAttribute('aria-expanded', 'true');
  await expect(toggle).toHaveText(nullLabel(true, NULLS[1]));
  await expect(keys).toHaveText(COLUMNS);
  await expect(inspector.getByRole('button', { name: /^(Include|Exclude) user = / })).toHaveCount(0);

  // Enter closes and reopens it.
  await page.keyboard.press('Enter');
  await expect(toggle).toHaveAttribute('aria-expanded', 'false');
  await expect(keys).toHaveCount(COLUMNS.length - NULLS[1]);
  await page.keyboard.press('Enter');
  await expect(toggle).toHaveAttribute('aria-expanded', 'true');

  // Another event: still open.
  await page.locator(SEL.resultsExpandControl).nth(2).click();
  await expect(inspector).toContainText('Event 3');
  await expect(toggle).toHaveText(nullLabel(true, NULLS[2]));
  await expect(keys).toHaveCount(COLUMNS.length);

  // An event with no null field has no disclosure, and visiting it does
  // not reset the choice.
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
