// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The search URL contract (ADR-0027) in a real browser.
//
// Every URL here is a HAND-WRITTEN literal, never one the app's own
// encoder produced: the whole point is that a link someone pasted from a
// chat message behaves as the contract says, and a spec that asked the
// encoder for its input would agree with any bug the encoder has. The
// native table tests in `src/search_url.rs` cover the codec's rules; this
// file covers what the browser does to them on the way in and out.

import {
  test,
  expect,
  capturedExportCount,
  capturedQueries,
  capturedQueryCount,
  lastCapturedQuery,
  resetScenario,
} from '../fixtures';
import { SEL, COPY } from '../selectors';

type Ctl = import('@playwright/test').APIRequestContext;

const QUIET_MS = 500;

async function sseOpens(request: Ctl): Promise<number> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.sse.opens;
}

/** Run a query from the editor buffer, replacing whatever is there, and
 * wait for the answer to land.
 *
 * Waiting for the RESPONSE, not just the request, is deliberate and
 * documents a residual outside this slice: while a query is in flight
 * the results resource ignores source changes entirely, so a navigation
 * issued before the answer arrives posts nothing (probed on 275a6094 —
 * with a 1.5s stubbed delay, submitting a second query mid-flight never
 * runs it). That is a defect in how results are fetched, not in the URL
 * contract, and a reader pressing Back has seen results by then. */
async function runInEditor(page: import('@playwright/test').Page, dsl: string) {
  const answered = page.waitForResponse((r) => r.url().includes('/api/v1/query'));
  await page.locator(SEL.cmContent).click();
  await page.keyboard.press('Control+a');
  await page.keyboard.insertText(dsl);
  await page.keyboard.press('Control+Enter');
  await answered;
}

/** A link whose structured state cannot be read: banner up, nothing run. */
async function expectRefused(
  page: import('@playwright/test').Page,
  request: Ctl,
  url: string,
  copyPrefix: string,
) {
  // Fresh capture window: this helper is called several times per test
  // and "posted nothing" has to mean nothing since THIS navigation.
  await resetScenario(request, 'default');
  await page.goto(url);
  const notice = page.locator(SEL.urlNotice);
  await expect(notice, `no banner for ${url}`).toBeVisible();
  await expect(notice).toContainText(copyPrefix);
  // The refusal is an absence, so it needs a bounded quiet wait rather
  // than an await on something appearing.
  await page.waitForTimeout(QUIET_MS);
  expect(await capturedQueryCount(request), `${url} posted a query`).toBe(0);
  // …and the URL is untouched until the reader clicks.
  expect(page.url()).toContain(url.slice(url.indexOf('?')));
}

test('absolute range round-trips through the picker', async ({ page, request }) => {
  await page.goto('/search');
  await runInEditor(page, 'service=nginx');
  await lastCapturedQuery(request, 1);

  await page.locator(SEL.dateRangeTrigger).click();
  await page.locator(SEL.absoluteTab).click();
  // An offset on the way in is fine; only the canonical UTC form is
  // written out.
  await page.locator(SEL.dateRangeFrom).fill('2026-01-01T01:00:00+01:00');
  await page.locator(SEL.dateRangeTo).fill('2026-01-01T00:15:00Z');
  await page.locator(SEL.dateRangeApply).click();

  const search = await page.evaluate(() => location.search);
  expect(search).toContain('r=2026-01-01T00:00:00Z..2026-01-01T00:15:00Z');
  expect(search).not.toContain('abs:');
  // Readable in the address bar: the only escaping in the whole URL is
  // the query text's.
  expect(search).toBe('?q=service%3Dnginx&page=0&r=2026-01-01T00:00:00Z..2026-01-01T00:15:00Z');

  const label = page.locator(SEL.dateRangeTrigger);
  await expect(label).toContainText('2026-01-01T00:00:00Z');
  await expect(label).toContainText('2026-01-01T00:15:00Z');

  const body = await lastCapturedQuery(request, 2);
  expect(body.query).toBe(
    '_time>="2026-01-01T00:00:00Z" _time<="2026-01-01T00:15:00Z" service=nginx',
  );
});

test('literal versioned filters and range produce the exact DSL', async ({ page, request }) => {
  await page.goto(
    '/search?q=service%3Dnginx&page=0' +
      '&f=v1.W3sib3AiOiIrIiwiZmllbGQiOiJob3N0IiwidmFsdWUiOiJ3ZWItMDEifV0' +
      '&r=2026-01-01T00:00:00Z..now',
  );

  const body = await lastCapturedQuery(request, 1);
  expect(body.query).toBe('host="web-01" _time>="2026-01-01T00:00:00Z" service=nginx');
  await expect(page.locator(SEL.urlNotice)).toHaveCount(0);
});

test('a literal percent in q survives one decode', async ({ page, request }) => {
  // `%2541` is one encoding of `%41`, so a single decode gives the DSL
  // `message=/100%41/` while a second gives `message=/100A/`, a
  // different regex run without a word to the reader. leptos-router's
  // ParamsMap did exactly that: decodeURIComponent over what
  // UrlSearchParams had already decoded. Hence the raw query string.
  await page.goto('/search?q=message%3D%2F100%2541%2F&page=0');

  const body = await lastCapturedQuery(request, 1);
  expect(body.query).toBe('last=15m message=/100%41/');
  await expect(page.locator(SEL.cmContent)).toHaveText('message=/100%41/');
  await expect(page.locator(SEL.urlNotice)).toHaveCount(0);
  // Nothing rewrites the link on the reader's behalf.
  expect(await page.evaluate(() => location.search)).toBe('?q=message%3D%2F100%2541%2F&page=0');
});

test('legacy and malformed filter payloads refuse to run', async ({ page, request }) => {
  // The pre-#85 plain-text dialect, as the browser hands it over: `%2B`
  // decodes to `+` before the app sees it.
  await expectRefused(
    page,
    request,
    '/search?q=service%3Dnginx&page=0&f=%2Bhost%3Dweb-01',
    COPY.urlNoticeFiltersPrefix,
  );
  await expect(page.locator(SEL.urlNoticeRaw)).toHaveText('+host=web-01');
  await expect(page.locator(SEL.filtersBadChip)).toBeVisible();
  await expect(page.locator(SEL.filtersBadChip)).toHaveText(COPY.filtersUnreadableChip);
  await expect(page.locator(SEL.urlNoticeRepair)).toHaveText(COPY.urlNoticeRepairFilters);

  await expectRefused(
    page,
    request,
    '/search?q=service%3Dnginx&page=0&f=-source%3Dauth.log',
    COPY.urlNoticeFiltersPrefix,
  );
  await expect(page.locator(SEL.urlNoticeRaw)).toHaveText('-source=auth.log');
  await expect(page.locator(SEL.filtersBadChip)).toBeVisible();

  // Versioned but undecodable: no longer "zero filters".
  await expectRefused(
    page,
    request,
    '/search?q=service%3Dnginx&page=0&f=v1.!',
    COPY.urlNoticeFiltersPrefix,
  );
  await expect(page.locator(SEL.urlNoticeRaw)).toHaveText('v1.!');
  await expect(page.locator(SEL.filtersBadChip)).toBeVisible();

  // A versioned payload whose RECORD is not one this app writes: an
  // operator it never emits, and an empty field name. Skipping the bad
  // record and running the rest would be the same lie in a narrower
  // query. (Literals: base64url of
  // [{"op":"x","field":"host","value":"prod"}] and of
  // [{"op":"+","field":"","value":"prod"}].)
  for (const payload of [
    'v1.W3sib3AiOiJ4IiwiZmllbGQiOiJob3N0IiwidmFsdWUiOiJwcm9kIn1d',
    'v1.W3sib3AiOiIrIiwiZmllbGQiOiIiLCJ2YWx1ZSI6InByb2QifV0',
  ]) {
    await expectRefused(
      page,
      request,
      `/search?q=service%3Dnginx&page=0&f=${payload}`,
      COPY.urlNoticeFiltersPrefix,
    );
    await expect(page.locator(SEL.filtersBadChip)).toBeVisible();
  }

  // …and the live tail does not start either.
  const opensBefore = await sseOpens(request);
  await page.goto('/search?q=service%3Dnginx&mode=live&f=v1.!');
  await expect(page.locator(SEL.urlNotice)).toBeVisible();
  await page.waitForTimeout(QUIET_MS);
  expect(await sseOpens(request)).toBe(opensBefore);
  expect(await capturedQueryCount(request)).toBe(0);
});

test('malformed r refuses to run and repairs on click', async ({ page, request }) => {
  for (const raw of [
    'garbage',
    // Backwards.
    '2026-01-02T00:00:00Z..2026-01-01T00:00:00Z',
    // The retired form, which the browser's own decode used to split
    // inside the timestamp.
    'abs:2026-01-01T00:00:00Z:2026-01-01T00:15:00Z',
    // A real `+02:00` offset, percent-escaped so it survives the browser
    // as an offset…
    '2026-01-01T00%3A00%3A00%2B02%3A00..now',
    // …and unescaped, where `+` arrives as a space. Both unreadable.
    '2026-01-01T00:00:00+02:00..now',
  ]) {
    await expectRefused(
      page,
      request,
      `/search?q=service%3Dnginx&page=0&r=${raw}`,
      COPY.urlNoticeRangePrefix,
    );
    await expect(page.locator(SEL.urlNoticeRepair)).toHaveText(COPY.urlNoticeRepairRange);
  }

  // Repair the last one: replace navigation, `r` gone, one query runs.
  const historyBefore = await page.evaluate(() => history.length);
  await page.locator(SEL.urlNoticeRepair).click();
  await expect(page.locator(SEL.urlNotice)).toHaveCount(0);
  expect(await page.evaluate(() => location.search)).toBe('?q=service%3Dnginx&page=0');
  expect(await page.evaluate(() => history.length)).toBe(historyBefore);
  const body = await lastCapturedQuery(request, 1);
  expect(body.query).toBe('last=15m service=nginx');
});

test('a repair fixes only the parameter the banner names', async ({ page, request }) => {
  // Wrong in two places at once. Precedence puts filters first.
  await expectRefused(
    page,
    request,
    '/search?q=service%3Dnginx&f=v1.!&r=garbage',
    COPY.urlNoticeFiltersPrefix,
  );
  // The banner is the whole results area: no rows under a notice that
  // says the link was not run, even for an answer already in flight.
  await expect(page.locator(SEL.resultsPane)).toHaveCount(0);

  // "Drop filters" drops `f` and NOTHING else. `r=garbage` is still in
  // the address bar, so the next verdict raises its own banner and the
  // link still has not run.
  await page.locator(SEL.urlNoticeRepair).click();
  await expect(page.locator(SEL.urlNotice)).toContainText(COPY.urlNoticeRangePrefix);
  await expect(page.locator(SEL.urlNoticeRaw)).toHaveText('garbage');
  expect(await page.evaluate(() => location.search)).toBe('?q=service%3Dnginx&r=garbage');
  await expect(page.locator(SEL.resultsPane)).toHaveCount(0);
  await page.waitForTimeout(QUIET_MS);
  expect(await capturedQueryCount(request), 'the range banner still ran a query').toBe(0);

  // The second click repairs the range, and only then does it run.
  await page.locator(SEL.urlNoticeRepair).click();
  await expect(page.locator(SEL.urlNotice)).toHaveCount(0);
  expect(await page.evaluate(() => location.search)).toBe('?q=service%3Dnginx');
  const body = await lastCapturedQuery(request, 1);
  expect(body.query).toBe('last=15m service=nginx');
  // …and the results pane is back, so its absence above was the gate
  // and not a selector that never matches anything.
  await expect(page.locator(SEL.resultsPane)).toBeVisible();
});

test('a malformed link disables every control that could run it', async ({ page, request }) => {
  // Wrong in two places, so the banner names `f` while `r` is unreadable
  // too: whatever a control read back would be the fallback, not the
  // link. Nothing here may navigate except the repair (ADR-0027).
  const url = '/search?q=service%3Dnginx&f=v1.!&r=garbage';
  const search = url.slice(url.indexOf('?'));
  await resetScenario(request, 'default');
  const opensBefore = await sseOpens(request);
  await page.goto(url);
  await expect(page.locator(SEL.urlNotice)).toBeVisible();

  // Haul, and the editor's own Ctrl+Enter, which reaches the same
  // callback without touching the button.
  await expect(page.locator(SEL.runButton)).toBeDisabled();
  await page.locator(SEL.cmContent).click();
  await page.keyboard.press('Control+Enter');
  await page.locator(SEL.runButton).click({ force: true });

  // Export: the one that used to post `{"query":""}`, which the server
  // reads as every row. Disabled, and the modal does not even open.
  await expect(page.locator(SEL.exportAction)).toBeDisabled();
  await expect(page.locator(SEL.saveAction)).toBeDisabled();
  await page.locator(SEL.exportAction).click({ force: true });
  await expect(page.locator(SEL.modalPanel)).toHaveCount(0);

  // The range picker still opens — it is how the current range is read —
  // but a preset applies nothing.
  await page.locator(SEL.dateRangeTrigger).click();
  const preset = page.locator(SEL.quickRangeOption).first();
  await expect(preset).toBeDisabled();
  await preset.click({ force: true });

  // Live Tail, on the same popover's Real-time tab.
  await page.locator(SEL.realtimeTab).click();
  await expect(page.locator(SEL.liveTailButton)).toBeDisabled();
  await page.locator(SEL.liveTailButton).click({ force: true });

  // Every refusal is an absence, so one bounded quiet wait covers them.
  await page.waitForTimeout(QUIET_MS);
  expect(await page.evaluate(() => location.search), 'a control rewrote the link').toBe(search);
  expect(await capturedQueryCount(request), 'a control ran the link').toBe(0);
  expect(await capturedExportCount(request), 'a control exported the link').toBe(0);
  expect(await sseOpens(request), 'a control opened a stream').toBe(opensBefore);
});

test('an open modal closes when the link stops being readable', async ({ page, request }) => {
  await page.goto('/search?q=service%3Dnginx&page=0');
  await lastCapturedQuery(request, 1);
  await page.locator(SEL.exportAction).click();
  await expect(page.locator(SEL.modalPanel)).toBeVisible();

  // A history entry the app itself can no longer produce — the gate
  // above is exactly what stops it — planted with pushState (which the
  // router does not observe) and then REACHED with a real Forward, so
  // what the router sees is the popstate a Back into a shared broken
  // link delivers.
  await page.evaluate(() => history.pushState({}, '', '/search?q=service%3Dnginx&f=v1.!'));
  await page.goBack();
  await page.goForward();

  await expect(page.locator(SEL.urlNotice)).toBeVisible();
  await expect(page.locator(SEL.modalPanel), 'a modal outlived the link it was opened on').toHaveCount(0);
});

test('the export modal refuses an empty query instead of posting it', async ({ page, request }) => {
  // The second layer, reached where the first one does not apply: a page
  // with no `q` at all hands the modal the same empty string the
  // malformed gate does, and an empty query is `SELECT *` with no WHERE
  // to the server.
  await page.goto('/search');
  await page.locator(SEL.exportAction).click();
  await expect(page.locator(SEL.modalPanel)).toBeVisible();
  // The modal's own submit, so the refusal is proven at the callback and
  // not at the footer button's disabled state.
  await page.keyboard.press('Control+Enter');
  await expect(page.locator(SEL.modalRefusal)).toHaveText(COPY.emptyQueryRefusal);
  await page.waitForTimeout(QUIET_MS);
  expect(await capturedExportCount(request)).toBe(0);
});

test('page offset overflows refuse to run while an unreadable page is page 1', async ({
  page,
  request,
}) => {
  // Past `u64`: not a number at all, so it is a missing value and the
  // link runs as page 1. (The plan's `18446744073709551615` parses as a
  // u64 and lands in the overflow case below; `usize` would have made
  // the answer depend on the browser's 32-bit pointer width.)
  await page.goto('/search?q=service%3Dnginx&page=99999999999999999999999999');
  const first = await lastCapturedQuery(request, 1);
  expect(first.offset).toBe(0);
  expect(first.query).toBe('last=15m service=nginx');
  await expect(page.locator(SEL.urlNotice)).toHaveCount(0);

  // Parses, but `page * 50` does not fit: a false claim.
  // 85899346 * 50 is one past `u32::MAX`, the ceiling the request's own
  // `offset` imposes; the other two overflow `u64` multiplication.
  for (const raw of ['18446744073709551615', '368934881474191033', '85899346']) {
    await expectRefused(page, request, `/search?q=service%3Dnginx&page=${raw}`, COPY.urlNoticePagePrefix);
    await expect(page.locator(SEL.urlNoticeRepair)).toHaveText(COPY.urlNoticeRepairPage);
    await expect(page.locator(SEL.urlNoticeRaw)).toHaveText(raw);
  }

  const historyBefore = await page.evaluate(() => history.length);
  await page.locator(SEL.urlNoticeRepair).click();
  await expect(page.locator(SEL.urlNotice)).toHaveCount(0);
  expect(await page.evaluate(() => location.search)).toBe('?q=service%3Dnginx&page=0');
  expect(await page.evaluate(() => history.length)).toBe(historyBefore);
  const body = await lastCapturedQuery(request, 1);
  expect(body.offset).toBe(0);
});

test('q encoding matches encodeURIComponent', async ({ page }) => {
  await page.goto('/search');
  await runInEditor(page, COPY.reservedSet);

  await expect(page).toHaveURL(/[?&]q=/);
  // The drift guard proper: the app's own encoder (whose output the
  // native table test pins as RESERVED_SET_ENCODED) and the browser's
  // encodeURIComponent produce the same string, character for
  // character.
  const browserEncoded = await page.evaluate((raw) => encodeURIComponent(raw), COPY.reservedSet);
  expect(browserEncoded).toBe(COPY.reservedSetEncoded);

  // What the address bar then shows is Chromium's own re-serialization
  // of that URL: it percent-encodes an apostrophe in the query even
  // though encodeURIComponent leaves it (the URL standard's
  // special-query percent-encode set). Nothing else moves…
  const search = await page.evaluate(() => location.search);
  expect(search).toBe(`?q=${COPY.reservedSetEncoded.replaceAll("'", '%27')}&page=0`);
  // …and it decodes back to exactly what was typed.
  const roundTripped = await page.evaluate(() => new URLSearchParams(location.search).get('q'));
  expect(roundTripped).toBe(COPY.reservedSet);
});

test('back and forward restore each entry exactly once', async ({ page, request }) => {
  await page.goto('/search');
  // Tag `window` so a full reload (which replaces it) is detectable.
  await page.evaluate(() => {
    (window as any).__e2e_marker = true;
  });

  await runInEditor(page, 'service=nginx');
  await lastCapturedQuery(request, 1);
  const urlA = page.url();
  await runInEditor(page, 'service=auth');
  await lastCapturedQuery(request, 2);
  const urlB = page.url();
  expect(urlA).not.toBe(urlB);

  const backAnswered = page.waitForResponse((r) => r.url().includes('/api/v1/query'));
  await page.goBack();
  await expect(page).toHaveURL(urlA);
  expect(page.url()).toBe(urlA);
  await expect(page.locator(SEL.cmContent)).toHaveText('service=nginx');
  await backAnswered;
  await lastCapturedQuery(request, 3);

  const forwardAnswered = page.waitForResponse((r) => r.url().includes('/api/v1/query'));
  await page.goForward();
  expect(page.url()).toBe(urlB);
  await expect(page.locator(SEL.cmContent)).toHaveText('service=auth');
  await forwardAnswered;
  await lastCapturedQuery(request, 4);

  expect(await capturedQueries(request)).toEqual([
    'last=15m service=nginx',
    'last=15m service=auth',
    'last=15m service=nginx',
    'last=15m service=auth',
  ]);

  const markerSurvived = await page.evaluate(() => (window as any).__e2e_marker === true);
  expect(markerSurvived, 'Back/Forward left the SPA — the router did a full load').toBe(true);
});
