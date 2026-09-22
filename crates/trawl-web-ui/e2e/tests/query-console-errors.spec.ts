// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Query errors (ADR-0039): what the page says when a query is wrong.
//
// Two owners, never crossed. The server's verdict on a SENT query is the
// query error notice in the results region; it quotes the effective text
// that request carried. The local parser's verdict on the DRAFT is the
// draft diagnostic under the editor. Every case here asserts on one of
// the two, and the identity cases assert the notice never quotes a text
// other than the one the failing request sent.

import { readFileSync } from 'node:fs';
import { test, expect } from '../fixtures';
import { SEL, COPY } from '../selectors';
import type { Page, Route } from '@playwright/test';

/// A query error body under harness/wire/. Each is the envelope the server
/// writes for the text its case sends; e2e_wire_fixture_contract.rs checks
/// the parse bodies against the parser and the validation bodies against
/// the emitter.
const wire = (name: string) =>
  JSON.parse(readFileSync(`${__dirname}/../harness/wire/${name}.json`, 'utf8'));

/// The issue's sample, as typed into the editor. The local parser's first
/// error is at byte 34, the `h` of `host`.
const SAMPLE = 'service=kubelet | stats count( by host';
/// What the page sends for SAMPLE under the 15-minute range: the text the
/// server's span indexes.
const SAMPLE_SENT = 'last=15m service=kubelet | stats count( by host';
const SAMPLE_URL = `/search?q=${encodeURIComponent(SAMPLE)}&r=15m`;

/// Park every POST /api/v1/query until the case releases it, recording
/// the text each carried. `release(i, body)` answers the i-th request.
async function holdQueries(page: Page) {
  const parked: { route: Route; sent: string }[] = [];
  await page.route('**/api/v1/query', route => {
    parked.push({ route, sent: route.request().postDataJSON().query });
  });
  return {
    count: () => parked.length,
    sent: (i: number) => parked[i].sent,
    release: (i: number, status: number, json: unknown) => parked[i].route.fulfill({ status, json }),
  };
}

/// Record, from now on, whether a query error notice was ever put in the
/// page, however briefly. A notice that renders and is replaced within a
/// frame is still a notice the reader was shown.
async function watchForNotice(page: Page) {
  await page.evaluate(() => {
    const w = window as unknown as { __noticeSeen: boolean };
    w.__noticeSeen = document.querySelector('.query-error') !== null;
    new MutationObserver(() => {
      if (document.querySelector('.query-error')) w.__noticeSeen = true;
    }).observe(document.body, { childList: true, subtree: true });
  });
  return () => page.evaluate(() => (window as unknown as { __noticeSeen: boolean }).__noticeSeen);
}

async function settled(page: Page) {
  await page.evaluate(() => new Promise<void>(resolve => {
    requestAnimationFrame(() => requestAnimationFrame(() => resolve()));
  }));
}

async function typeDraft(page: Page, text: string) {
  await page.locator(SEL.cmContent).click();
  await page.keyboard.press('Control+a');
  await page.keyboard.insertText(text);
}

test.describe('draft diagnostic', () => {
  test('names the first error in full and clears when the paren closes', async ({ page }) => {
    await page.goto('/search?r=15m');
    await typeDraft(page, SAMPLE);

    const line = page.locator(SEL.draftDiagnosticFirst);
    await expect(line).toBeVisible();
    await expect(line).toHaveText(/^Line 1:35 — found 'h', expected /);
    // Full text: the rendered line is not clipped by an ellipsis.
    const clipped = await line.evaluate(el => el.scrollWidth > el.clientWidth);
    expect(clipped).toBe(false);
    await expect(line).not.toHaveAttribute('aria-live');
    await expect(page.locator(SEL.draftDiagnostic).locator('[aria-live]')).toHaveCount(0);
    // One error: no disclosure.
    await expect(page.locator(SEL.draftDiagnosticMore)).toHaveCount(0);

    await typeDraft(page, 'service=kubelet | stats count() by host');
    await expect(page.locator(SEL.draftDiagnostic)).toHaveCount(0);
  });

  test('keeps the second error behind a closed +1 more that closes again on edit', async ({ page }) => {
    await page.goto('/search?r=15m');
    await typeDraft(page, 'f=#a,#b');

    // Two local errors, one per unquoted `#`.
    const more = page.locator(SEL.draftDiagnosticMore);
    await expect(page.locator(SEL.draftDiagnosticFirst)).toHaveText(/^Line 1:3 — '#' inside an unquoted value/);
    await expect(more).toBeVisible();
    await expect(more.locator('summary')).toHaveText('+1 more');
    await expect(more).not.toHaveAttribute('open');
    const second = more.locator('li');
    await expect(second).toHaveCount(1);
    await expect(second).toBeHidden();

    await more.locator('summary').click();
    await expect(more).toHaveAttribute('open', '');
    await expect(second).toBeVisible();
    await expect(second).toHaveText(/^Line 1:6 — '#' inside an unquoted value/);

    // An edit that keeps the errors closes the disclosure.
    await page.locator(SEL.cmContent).click();
    await page.keyboard.press('End');
    await page.keyboard.type(' ');
    await expect(more).toBeVisible();
    await expect(more).not.toHaveAttribute('open');
  });

  test('F8 moves to the first diagnostic and the gutter marker is named', async ({ page }) => {
    await page.goto('/search?r=15m');
    await typeDraft(page, SAMPLE);
    await expect(page.locator(SEL.draftDiagnosticFirst)).toBeVisible();

    // Named from the accessibility tree, which honours aria-hidden: a
    // marker under a hidden ancestor would not be found by role. The
    // editor's linter runs after a pause, so this also waits for it.
    const marker = page.getByRole('img', { name: COPY.lintMarkerName, exact: true });
    await expect(marker).toHaveCount(1);
    await expect(marker).toHaveAccessibleName(COPY.lintMarkerName);
    await expect(page.locator(SEL.dslEditor)).toMatchAriaSnapshot(`
      - img "${COPY.lintMarkerName}"
    `);
    // Line numbers stay out of the tree.
    const lineNumbersHidden = await page
      .locator(`${SEL.dslEditor} .cm-lineNumbers`)
      .evaluate(el => el.closest('[aria-hidden="true"]') !== null);
    expect(lineNumbersHidden).toBe(true);

    // The cursor sits at the end after typing; F8 wraps to the first
    // diagnostic and selects its span, the `h` of `host`.
    await expect.poll(() => page.evaluate(() => window.getSelection()?.toString())).toBe('');
    await page.keyboard.press('F8');
    await expect.poll(() => page.evaluate(() => window.getSelection()?.toString())).toBe('h');
  });
});

test.describe('query error identity', () => {
  test('a Haul of the same query while it is pending waits for its own verdict', async ({ page }) => {
    const queries = await holdQueries(page);
    await page.goto(SAMPLE_URL);
    await expect.poll(queries.count).toBe(1);
    expect(queries.sent(0)).toBe(SAMPLE_SENT);

    // Re-send the same text while the first request is still out. The
    // Haul button is disabled while a query runs; the editor's own
    // submit is not, and it is the path a repeated Haul takes.
    await page.locator(SEL.cmContent).click();
    await page.keyboard.press('Control+Enter');
    const noticeSeen = await watchForNotice(page);

    // The first answer lands after the second Haul was asked for, so it
    // is not the second Haul's verdict and must not be shown as one.
    await queries.release(0, 400, wire('query-parse-error'));
    await expect.poll(queries.count).toBe(2);
    expect(queries.sent(1)).toBe(SAMPLE_SENT);
    await settled(page);
    expect(await noticeSeen()).toBe(false);
    await expect(page.locator(SEL.queryErrorNotice)).toHaveCount(0);

    await queries.release(1, 400, wire('query-parse-error'));
    await expect(page.locator(SEL.queryErrorNotice)).toBeVisible();
  });
});
