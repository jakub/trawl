// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Every failed answer is read by one classifier (ADR-0066, issue #386).
// A refusal that carries the server's envelope shows the server's
// sentence, on endpoints that used to drop it for a bare status. A body
// that is not the envelope (an HTML error page) or an envelope whose code
// this build does not know shows generic copy, and its raw text never
// reaches the page. The investigation writes keep their own reading: an
// answer that may follow a commit is an unknown outcome and is never sent
// again, and `request_limit_reached` is a definite refusal.
//
// Each failure is a `page.route` override on the one endpoint under test,
// so the stub's own answer for every other read stays in place.

import type { Locator, Page, Route } from '@playwright/test';
import { test, expect, resetScenario } from '../fixtures';
import { SEL, COPY } from '../selectors';

type Reply = Parameters<Route['fulfill']>[0];

/** The server's sentence, which an enveloped refusal must show verbatim. */
const REFUSAL = 'Trawl is rebuilding its catalog; try again in a minute.';
const RAW_HTML = 'RAW-HTML-SENTINEL upstream stack at /srv/trawl/worker.rs';
const RAW_UNKNOWN = 'RAW-UNKNOWN-CODE-SENTINEL';

const envelope = (status: number, code: string, message: string): Reply =>
  ({ status, json: { error: { code, message } } });
const refusal = envelope(503, 'service_unavailable', REFUSAL);
const htmlPage: Reply = {
  status: 500,
  contentType: 'text/html; charset=utf-8',
  body: `<!doctype html><html><body><h1>Internal Server Error</h1><p>${RAW_HTML}</p></body></html>`,
};
const unknownCode = envelope(503, 'flux_capacitor_offline', RAW_UNKNOWN);

/** Neither raw body ever reaches the document, as text or as markup. */
async function expectNoRawText(page: Page) {
  for (const raw of [RAW_HTML, RAW_UNKNOWN, 'Internal Server Error', 'flux_capacitor_offline']) {
    await expect(page.locator('body')).not.toContainText(raw);
    expect(await page.content()).not.toContain(raw);
  }
}

/** Answer every GET of `pathname` with `reply`; anything else passes. */
async function failReads(page: Page, pathname: string, reply: Reply) {
  await page.route(url => url.pathname === pathname, route =>
    route.request().method() === 'GET' ? route.fulfill(reply) : route.fallback());
}

// One row per read whose failure used to be a bare status. `generic` is
// what a status-only failure reads as on that surface.
const reads = [
  {
    name: 'history',
    path: '/api/v1/history',
    open: '/search/history',
    scenario: 'default',
    shown: (page: Page) => page.locator(SEL.historyPage).locator('.load-hint.error'),
    says: (detail: string) => `Couldn't load history: ${detail}`,
  },
  {
    name: 'nets list',
    path: '/api/v1/saved',
    open: '/jobs/nets',
    scenario: 'default',
    shown: (page: Page) => page.locator('.load-hint.error'),
    says: (detail: string) => `Couldn't load nets: ${detail}`,
  },
  {
    name: 'schema services',
    path: '/api/v1/schema/services',
    open: '/search/schema',
    scenario: 'default',
    shown: (page: Page) => page.locator('.load-hint.error'),
    says: (detail: string) => `Couldn't load schema: ${detail}`,
  },
  {
    // `telemetry_get`, as the Health page's capacity card reads it.
    name: 'health capacity',
    path: '/api/v1/stats',
    open: '/settings/health',
    scenario: 'health-admin',
    shown: (page: Page) => page.locator(SEL.healthCapacity).getByRole('alert'),
    says: (detail: string) => detail === REFUSAL
      ? `Report unavailable: ${detail}`
      : `Report unavailable, ${detail}. Refresh to try again.`,
  },
  {
    // `telemetry_get`, as the Health page's query list reads it.
    name: 'health queries',
    path: '/api/v1/queries',
    open: '/settings/health',
    scenario: 'health-viewer',
    shown: (page: Page) => page.locator(SEL.healthQueries).getByRole('alert'),
    says: (detail: string) => detail === REFUSAL
      ? `Report unavailable: ${detail}`
      : `Report unavailable, ${detail}. Refresh to try again.`,
  },
] as const;

for (const read of reads) {
  test(`a ${read.name} refusal shows the server's sentence`, async ({ page, request }) => {
    await resetScenario(request, read.scenario);
    await failReads(page, read.path, refusal);
    await page.goto(read.open);
    await expect(read.shown(page)).toContainText(read.says(REFUSAL));
    await expect(page).toHaveURL(url => url.pathname === read.open);
  });

  for (const [kind, reply, status] of [['an HTML error page', htmlPage, 500], ['an unknown code', unknownCode, 503]] as const) {
    test(`a ${read.name} failure with ${kind} reads as generic copy without its raw text`, async ({ page, request }) => {
      await resetScenario(request, read.scenario);
      await failReads(page, read.path, reply);
      await page.goto(read.open);
      await expect(read.shown(page)).toContainText(read.says(`server returned ${status}`));
      await expectNoRawText(page);
    });
  }
}

// A write that toasts its failure: Clear history.
async function clearHistory(page: Page) {
  await page.getByRole('button', { name: COPY.historyClear, exact: true }).click();
  await page.locator(SEL.healthConfirm)
    .getByRole('button', { name: COPY.historyClearConfirm, exact: true }).click();
}

for (const [kind, reply, detail] of [
  ['an enveloped refusal', refusal, REFUSAL],
  ['an HTML error page', htmlPage, 'server returned 500'],
  ['an unknown code', unknownCode, 'server returned 503'],
] as const) {
  test(`clear history answered with ${kind} toasts ${detail === REFUSAL ? "the server's sentence" : 'generic copy'}`, async ({ page, request }) => {
    await resetScenario(request, 'history-ready');
    let deletes = 0;
    await page.route(url => url.pathname === '/api/v1/history', route => {
      if (route.request().method() !== 'DELETE') return route.fallback();
      deletes += 1;
      return route.fulfill(reply);
    });
    await page.goto('/search/history');
    await expect(page.locator(SEL.historyPage).locator(SEL.rowStretch)).toHaveCount(3);
    await clearHistory(page);
    const toast = page.locator(SEL.toastAny);
    await expect(toast).toHaveCount(1);
    await expect(toast.locator('.title')).toHaveText(COPY.historyClearFailed);
    await expect(toast.locator('.detail')).toHaveText(detail);
    await expectNoRawText(page);
    expect(deletes).toBe(1);
  });
}

// `cancel_query`: an enveloped refusal below 500 is the server's
// sentence. A 5xx could follow a DELETE that reached the daemon, so its
// outcome stays unknown (health-page.spec.ts covers that half).
for (const [kind, reply, outcome] of [
  ['an enveloped refusal', envelope(409, 'bad_request', REFUSAL), `Cancellation failed: ${REFUSAL}`],
  ['an HTML error page', { ...htmlPage, status: 409 }, 'Cancellation failed, server returned 409.'],
  ['an unknown code', envelope(409, 'flux_capacitor_offline', RAW_UNKNOWN), 'Cancellation failed, server returned 409.'],
] as const) {
  test(`cancel query answered with ${kind} says ${outcome}`, async ({ page, request }) => {
    await resetScenario(request, 'health-cancel');
    let cancels = 0;
    await page.route(url => url.pathname === '/api/v1/queries/101', route => {
      if (route.request().method() !== 'DELETE') return route.fallback();
      cancels += 1;
      return route.fulfill(reply);
    });
    await page.goto('/settings/health');
    await expect(page.locator(SEL.healthRow)).toHaveCount(4);
    await page.locator(`${SEL.healthRow}[data-query-id="101"]`).getByRole('button', { name: 'Cancel', exact: true }).click();
    await page.locator(SEL.healthConfirm).getByRole('button', { name: 'Cancel query', exact: true }).click();
    await expect(page.locator('.health-cancel-outcome')).toHaveText(outcome);
    await expectNoRawText(page);
    expect(cancels).toBe(1);
  });
}

// AC7 names `/api/v1/schema/catalog/*`; that path is stale. The case file
// reads `/api/v1/schema/field?name=`, which is what these routes answer.
test('the field case file shows its missing state for an enveloped 404', async ({ page }) => {
  await failReads(page, '/api/v1/schema/field', envelope(404, 'not_found', 'no pin for duration'));
  await page.goto('/search/schema?field=duration');
  const drawer = page.locator('.fc-case');
  await expect(drawer).toContainText('No pin for duration.');
  await expect(page.getByRole('dialog', { name: 'duration' }).getByRole('alert')).toHaveCount(0);
});

test('the field case file shows the store sentence for an enveloped 503', async ({ page }) => {
  await failReads(page, '/api/v1/schema/field', envelope(503, 'service_unavailable', RAW_UNKNOWN));
  await page.goto('/search/schema?field=duration');
  await expect(page.getByRole('dialog', { name: 'duration' }).getByRole('alert'))
    .toHaveText("Couldn't load field: the catalog store is unavailable");
  await expect(page.getByText('No pin for', { exact: false })).toHaveCount(0);
  await expectNoRawText(page);
});

// ---- AC8: investigation writes --------------------------------------------

const BOARD = '5f0c1a2b3c4d5e6f7a8b9c0d1e2f3a4b';
const BOARD_TITLE = '502s on checkout since 09:00';
const NOW = new Date('2026-10-24T09:45:00Z');
const LIMIT = 'The server is at its request limit and did not process this request.';

type Answer = 'lost' | 'unreadable' | 'internal error' | 'gateway page' | 'request limit';
const answers: Answer[] = ['lost', 'unreadable', 'internal error', 'gateway page', 'request limit'];

/** Answer the write once, as `answer` says. A lost answer never arrives;
 * an unreadable one is a success status whose body is cut short; the
 * two 5xx may follow a commit; the request limit refuses at admission. */
async function answerWrite(route: Route, answer: Answer, success: number) {
  switch (answer) {
    case 'lost': return route.abort('failed');
    case 'unreadable': return route.fulfill({ status: success, contentType: 'application/json', body: '{"entry":' });
    case 'internal error': return route.fulfill(envelope(500, 'internal_error', 'internal server error'));
    case 'gateway page': return route.fulfill({ ...htmlPage, status: 502 });
    case 'request limit': return route.fulfill(envelope(503, 'request_limit_reached', LIMIT));
  }
}

/** Count every request to one write endpoint and answer it as `answer`. */
async function oneWrite(page: Page, matches: (url: URL, method: string) => boolean, answer: Answer, success: number) {
  const sent = { count: 0 };
  await page.route(url => url.pathname.startsWith('/api/v1/'), route => {
    const req = route.request();
    if (!matches(new URL(req.url()), req.method())) return route.fallback();
    sent.count += 1;
    return answerWrite(route, answer, success);
  });
  return sent;
}

async function openBoard(page: Page) {
  await page.clock.install({ time: NOW.getTime() - 10_000 });
  await page.clock.pauseAt(NOW);
  await page.goto(`/investigations/${BOARD}`);
  const main = page.locator('.inv-page');
  await expect(main.getByText('Every entry is loaded.')).toBeVisible();
  return main;
}

/** No second request reaches the endpoint, given time to send one. */
async function sentOnce(page: Page, sent: { count: number }) {
  expect(sent.count).toBe(1);
  await page.waitForTimeout(500);
  expect(sent.count).toBe(1);
}

for (const answer of answers) {
  test(`a keep answered ${answer} is ${answer === 'request limit' ? 'refused' : 'unknown'} and sent once`, async ({ page, request }) => {
    await resetScenario(request, 'investigations-writer');
    const sent = await oneWrite(page, (url, method) =>
      method === 'POST' && url.pathname === `/api/v1/investigations/${BOARD}/entries`, answer, 201);
    const main = await openBoard(page);
    const finding = main.getByRole('list', { name: 'Findings' }).locator('> li').nth(0);
    await finding.getByRole('button', { name: 'Open Row 0 of result fedcba98 among its rows' }).click();
    const viewer = page.getByRole('dialog', { name: 'Result fedcba98' });
    await viewer.getByRole('button', { name: 'Keep row 0' }).click();
    const form = viewer.getByRole('form', { name: 'Keep row 0' });
    await form.getByRole('textbox', { name: 'Why keep row 0?' }).fill('pool exhausted at 64/64');
    await page.keyboard.press('Enter');
    const note = viewer.locator('.inv-write-note [role="status"]');
    if (answer === 'request limit') {
      await expect(note).toHaveText(`Could not keep row 0. ${LIMIT}`);
      await expect(form.getByRole('button', { name: 'Keep row 0' })).toBeEnabled();
    } else {
      await expect(note).toHaveText(
        'The answer to your keep of row 0 was lost, so it may or may not have been kept. Refresh the board and look for it in the activity before you keep it again.',
      );
      await expect(form.getByRole('button', { name: 'Keep row 0' })).toBeDisabled();
    }
    await expectNoRawText(page);
    await sentOnce(page, sent);
  });

  test(`a close answered ${answer} is ${answer === 'request limit' ? 'refused' : 'unknown'} and sent once`, async ({ page, request }) => {
    await resetScenario(request, 'investigations-writer');
    const sent = await oneWrite(page, (url, method) =>
      method === 'PATCH' && url.pathname === `/api/v1/investigations/${BOARD}`, answer, 200);
    const main = await openBoard(page);
    const close = main.getByRole('button', { name: 'Close investigation' });
    await close.click();
    const note = main.locator('.inv-write-note [role="status"]');
    if (answer === 'request limit') {
      await expect(note).toHaveText(`Could not close the investigation. ${LIMIT}`);
      await expect(close).toBeEnabled();
    } else {
      await expect(note).toHaveText(
        "The answer to your close was lost, so it may or may not have taken effect. Refresh to see the investigation's status before you act again.",
      );
      await expect(close).toBeDisabled();
    }
    await expectNoRawText(page);
    await sentOnce(page, sent);
  });

  test(`a capture answered ${answer} is ${answer === 'request limit' ? 'refused' : 'unknown'} and sent once`, async ({ page, request }) => {
    const reset = await request.post('/__ctl/reset', { data: { scenario: 'investigations-writer', investigationSearch: true } });
    expect(reset.ok()).toBe(true);
    const sent = await oneWrite(page, (url, method) => method === 'POST' && url.pathname === '/api/v1/results', answer, 200);
    await page.goto('/search?q=status%3E%3D500&r=2026-09-15T12:00:00Z..2026-09-15T12:30:00Z');
    await expect(page.locator('#search-results table.results-table > tbody > tr:not(:has(td.detail))')).toHaveCount(7);
    await page.locator('.tabs-actions').getByRole('button', { name: 'Send to investigation' }).click();
    const dialog = page.getByRole('dialog', { name: 'Send to investigation' });
    await dialog.getByRole('radio', { name: `${BOARD_TITLE} 5f0c1a2b` }).check();
    await dialog.getByRole('button', { name: 'Send', exact: true }).click();
    const alert: Locator = dialog.getByRole('alert');
    if (answer === 'request limit') {
      await expect(alert).toHaveText(`Nothing was sent: ${LIMIT}`);
      await expect(dialog.getByRole('button', { name: 'Send', exact: true })).toBeEnabled();
    } else {
      await expect(alert).toHaveText(
        'The answer was lost on the way back, so it is not known whether the search reached the investigation. '
        + 'Open its board to check before you send it again.');
      await expect(dialog.getByRole('button', { name: 'Send', exact: true })).toHaveCount(0);
    }
    await expectNoRawText(page);
    await sentOnce(page, sent);
  });
}
