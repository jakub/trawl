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

/// The query error notice in the results region.
const notice = (page: Page) => page.locator(SEL.queryErrorNotice);

/// Answer every query with `body` at `status`, recording what each sent.
async function answerQueries(page: Page, status: number, body: unknown) {
  const sent: string[] = [];
  await page.route('**/api/v1/query', route => {
    sent.push(route.request().postDataJSON().query);
    return route.fulfill({ status, json: body });
  });
  return sent;
}

/// Assert one excerpt block quotes `sent` with its caret under byte
/// `start` (ASCII text, so bytes are characters) and nothing else.
async function expectExcerpt(block: ReturnType<Page['locator']>, sent: string, start: number) {
  const excerpt = block.locator(SEL.queryErrorExcerpt);
  await expect(excerpt).toHaveCount(1);
  await expect(excerpt.locator(SEL.queryErrorCaption)).toHaveText(COPY.queryErrorCaption);
  const caret = excerpt.locator(SEL.queryErrorCaret);
  await expect(caret).toHaveText(`${' '.repeat(start)}^`);
  await expect(caret).toHaveAttribute('aria-hidden', 'true');
  // The quoted line is the sent text, then the caret line under it.
  const text = await excerpt.locator(SEL.queryErrorText).evaluate(pre => pre.textContent);
  expect(text).toBe(`${sent}\n${' '.repeat(start)}^`);
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
  test('names the first error in full and clears when the paren closes', async ({ page }, testInfo) => {
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
    await page.locator(SEL.draftDiagnostic).locator('xpath=..').screenshot({
      path: testInfo.outputPath('draft-diagnostic.png'),
    });

    await typeDraft(page, 'service=kubelet | stats count() by host');
    await expect(page.locator(SEL.draftDiagnostic)).toHaveCount(0);
  });

  test('keeps the second error behind a closed +1 more that closes again on edit', async ({ page }, testInfo) => {
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
    await page.locator(SEL.draftDiagnostic).locator('xpath=..').screenshot({
      path: testInfo.outputPath('draft-diagnostic-more-open.png'),
    });

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

test.describe('query error notice', () => {
  test('Events shows the server verdict with the sent text and no Retry', async ({ page }, testInfo) => {
    const sent = await answerQueries(page, 400, wire('query-parse-error'));
    await page.goto(SAMPLE_URL);

    const alert = notice(page);
    await expect(alert).toBeVisible();
    expect(sent).toEqual([SAMPLE_SENT]);
    await expect(alert).toHaveAttribute('role', 'alert');
    await expect(alert.locator(SEL.queryErrorLead)).toHaveText(/^Couldn't run the query: found 'h', expected /);
    await expect(alert.locator(SEL.queryErrorBlock)).toHaveCount(1);
    // One detail: its message is the headline, so the block is the excerpt only.
    await expect(alert.locator(SEL.queryErrorMessage)).toHaveCount(0);
    await expectExcerpt(alert.locator(SEL.queryErrorBlock), SAMPLE_SENT, 43);
    expect(SAMPLE_SENT[43]).toBe('h');
    await expect(page.locator(SEL.resultsPane).getByRole('button')).toHaveCount(0);
    await expect(page.locator(SEL.loadHintError)).toHaveCount(0);
    await alert.screenshot({ path: testInfo.outputPath('events-query-error-notice.png') });
    await page.screenshot({ path: testInfo.outputPath('events-query-error-page.png') });
  });

  test('Visualization shows the same notice and no Retry snapshot', async ({ page }) => {
    await answerQueries(page, 400, wire('query-parse-error'));
    await page.goto(SAMPLE_URL);
    await expect(notice(page)).toBeVisible();
    await page.getByRole('tab', { name: 'Visualization', exact: true }).click();

    const alert = notice(page);
    await expect(alert).toBeVisible();
    await expect(alert.locator(SEL.queryErrorLead)).toHaveText(/^Couldn't run the query: found 'h', expected /);
    await expectExcerpt(alert.locator(SEL.queryErrorBlock), SAMPLE_SENT, 43);
    await expect(page.getByRole('button', { name: 'Retry snapshot' })).toHaveCount(0);
    await expect(page.getByText('Snapshot query failed.', { exact: false })).toHaveCount(0);
    await expect(page.locator(SEL.resultsPane).getByRole('button')).toHaveCount(0);
  });

  test('two details are counted and listed in server order', async ({ page }) => {
    const body = wire('query-parse-errors-two');
    await answerQueries(page, 400, body);
    await page.goto(`/search?q=${encodeURIComponent('f=#a,#b')}&r=15m`);

    const alert = notice(page);
    await expect(alert.locator(SEL.queryErrorLead)).toHaveText("Couldn't run the query: 2 errors");
    const blocks = alert.locator(SEL.queryErrorBlock);
    await expect(blocks).toHaveCount(2);
    const [first, second] = body.error.details;
    await expect(blocks.nth(0).locator(SEL.queryErrorMessage)).toHaveText(`${first.message} — ${first.hint}`);
    await expect(blocks.nth(1).locator(SEL.queryErrorMessage)).toHaveText(`${second.message} — ${second.hint}`);
    await expect(blocks.nth(0).locator(SEL.queryErrorMessage)).toContainText('"#a"');
    await expect(blocks.nth(1).locator(SEL.queryErrorMessage)).toContainText('"#b"');
    await expectExcerpt(blocks.nth(0), 'last=15m f=#a,#b', 11);
    await expectExcerpt(blocks.nth(1), 'last=15m f=#a,#b', 14);
  });

  test('a detail whose span does not index the sent text keeps its message and drops its caret', async ({ page }) => {
    const body = wire('query-parse-errors-two');
    body.error.details[1].span = { start: 400, end: 401 };
    await answerQueries(page, 400, body);
    await page.goto(`/search?q=${encodeURIComponent('f=#a,#b')}&r=15m`);

    const blocks = notice(page).locator(SEL.queryErrorBlock);
    await expect(blocks).toHaveCount(2);
    await expectExcerpt(blocks.nth(0), 'last=15m f=#a,#b', 11);
    await expect(blocks.nth(1).locator(SEL.queryErrorMessage)).toContainText('"#b"');
    await expect(blocks.nth(1).locator(SEL.queryErrorExcerpt)).toHaveCount(0);
    await expect(blocks.nth(1).locator(SEL.queryErrorCaret)).toHaveCount(0);
  });

  test('a validation error with a hint shows it once and no excerpt', async ({ page }) => {
    await answerQueries(page, 400, wire('query-validation-hint'));
    await page.goto(`/search?q=${encodeURIComponent('* | stats countt(x) by host')}&r=15m`);

    const alert = notice(page);
    await expect(alert.locator(SEL.queryErrorLead)).toHaveText(
      "Couldn't run the query: unknown function: countt — did you mean 'count'?",
    );
    expect((await alert.innerText()).split('did you mean').length - 1).toBe(1);
    await expect(alert.locator(SEL.queryErrorExcerpt)).toHaveCount(0);
    await expect(page.locator(SEL.resultsPane).getByRole('button')).toHaveCount(0);
  });

  test('a validation error without a hint shows its message and no excerpt', async ({ page }) => {
    await answerQueries(page, 400, wire('query-validation-no-hint'));
    await page.goto(`/search?q=${encodeURIComponent('* | stats nosuchfunc(x) by host')}&r=15m`);

    const alert = notice(page);
    await expect(alert.locator(SEL.queryErrorLead)).toHaveText("Couldn't run the query: unknown function: nosuchfunc");
    await expect(alert.locator(SEL.queryErrorExcerpt)).toHaveCount(0);
    await expect(alert).not.toContainText('did you mean');
  });

  test('an execution error keeps the load failure copy and its Retry', async ({ page }) => {
    await answerQueries(page, 500, wire('query-execution-error'));
    await page.goto('/search?q=service%3Dnginx&r=15m');

    await expect(page.locator(SEL.loadHintError)).toHaveText(/Couldn't load results: query execution failed/);
    await expect(page.locator(SEL.resultsPane).getByRole('button', { name: 'Retry', exact: true })).toBeVisible();
    await expect(notice(page)).toHaveCount(0);
  });

  test('a message carrying markup renders as text', async ({ page }) => {
    const body = wire('query-parse-error');
    body.error.message = '<b>x</b>';
    body.error.details[0].message = '<b>x</b>';
    await answerQueries(page, 400, body);
    await page.goto(SAMPLE_URL);

    const alert = notice(page);
    await expect(alert.locator(SEL.queryErrorLead)).toHaveText("Couldn't run the query: <b>x</b>");
    await expect(alert.locator('b')).toHaveCount(0);
  });
});

test.describe('query error identity, edits', () => {
  test('an edit made while the query is out does not change the text the notice quotes', async ({ page }) => {
    const queries = await holdQueries(page);
    await page.goto('/search?r=15m');
    await typeDraft(page, SAMPLE);
    await page.locator(SEL.runButton).click();
    await expect.poll(queries.count).toBe(1);
    expect(queries.sent(0)).toBe(SAMPLE_SENT);

    // B is typed, not Hauled, while A is still out.
    const b = 'service=kubelet | stats count() by pod';
    await typeDraft(page, b);
    await queries.release(0, 400, wire('query-parse-error'));

    const alert = notice(page);
    await expect(alert).toBeVisible();
    await expectExcerpt(alert.locator(SEL.queryErrorBlock), SAMPLE_SENT, 43);
    await expect(alert).not.toContainText('pod');
    // The draft is B; the draft diagnostic is about B, which parses.
    await expect(page.locator(SEL.draftDiagnostic)).toHaveCount(0);
    expect(queries.count()).toBe(1);
  });
});

test.describe('live', () => {
  test('a stream refused before it opened names the local syntax error, with no Retry', async ({ page }) => {
    const opened: string[] = [];
    await page.route('**/api/v1/stream?*', route => {
      opened.push(new URL(route.request().url()).searchParams.get('query')!);
      return route.fulfill({ status: 400, json: wire('query-parse-error') });
    });
    await page.goto(`/search?q=${encodeURIComponent(SAMPLE)}&mode=live`);

    const alert = notice(page);
    await expect(alert).toBeVisible();
    // A live stream carries no range, so the sent text is the draft.
    expect(opened[0]).toBe(SAMPLE);
    await expect(alert.locator(SEL.queryErrorLead)).toHaveText(
      /^Couldn't start the live stream\. The query has a syntax error: found 'h', expected /,
    );
    await expectExcerpt(alert.locator(SEL.queryErrorBlock), SAMPLE, 34);
    await expect(page.getByRole('button', { name: 'Retry live stream' })).toHaveCount(0);
    await expect(page.getByText(COPY.streamUnavailable, { exact: true })).toHaveCount(0);
  });

  test('a valid query refused with 401 keeps the generic copy and its Retry', async ({ page }) => {
    await page.route('**/api/v1/stream?*', route => route.fulfill({ status: 401, json: { error: 'unauthorized' } }));
    await page.goto('/search?q=service%3Dnginx&mode=live');

    await expect(page.getByText(COPY.streamUnavailable, { exact: true })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Retry live stream' })).toBeVisible();
    await expect(notice(page)).toHaveCount(0);
  });
});
