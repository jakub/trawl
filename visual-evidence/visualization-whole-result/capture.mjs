#!/usr/bin/env node
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Capture the whole-result chart defect (issue #228) against a live
// Trawl instance, once per revision.
//
// The property under test is the REQUEST and what the browser drew from
// it, not that a picture appeared. A canvas is opaque, so the script
// records three independent things per page:
//
//   1. every `/api/v1/query` request body the SPA posted (limit/offset)
//      and the `pagination` object that came back,
//   2. the plotted point count, read from the chart host's `data-points`
//      attribute where the revision publishes it, and
//   3. the categorical bar widths, read from each bar's inline style.
//
// Only (2) is revision-dependent: the base revision has no `data-points`
// attribute, so this script falls back to the response's `returned` and
// says so in the transcript. (1) and (3) read the same DOM and wire in
// both revisions, so the before/after comparison never rests on a
// measurement one side could not make.
//
// The corpus is seeded here, into a four-hour window that no other
// workload occupies, so the observed numbers are the script's own and
// not the host instance's history.
//
// Environment (all required):
//   TRAWL_EVIDENCE_LABEL   `before` or `after`; names the results subdirectory
//   TRAWL_URL              browser origin of the instance, e.g. http://127.0.0.1:PORT
//   TRAWL_API_URL          the daemon's own API base, for seeding and probing
//   TRAWL_BROWSER_KEY_FILE path to a file holding a reader API key
//   TRAWL_INGEST_KEY       an API key with `trawl:ingest`
// Optional:
//   TRAWL_EVIDENCE_OUT     results root (default: ./results beside this file)
//
// Nothing read from those variables reaches the transcript or a
// screenshot: no origin, no port, no hostname, no key.

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs/promises';
import http from 'node:http';
import https from 'node:https';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, '../..');

const label = required('TRAWL_EVIDENCE_LABEL');
assert.ok(['before', 'after'].includes(label), 'TRAWL_EVIDENCE_LABEL must be `before` or `after`');
const origin = new URL(required('TRAWL_URL')).origin;
const apiBase = required('TRAWL_API_URL').replace(/\/+$/, '');
const readerKey = (await fs.readFile(required('TRAWL_BROWSER_KEY_FILE'), 'utf8')).trim();
const ingestKey = required('TRAWL_INGEST_KEY').trim();
const outDir = path.join(process.env.TRAWL_EVIDENCE_OUT || path.join(here, 'results'), label);

function required(name) {
  const value = process.env[name];
  assert.ok(value, `${name} must be set; see README.md`);
  return value;
}

// ---------------------------------------------------------------- corpus

// A four-hour window chosen to hold nothing else. The app-experiment
// runner's own corpus is fixed at 2026-01-01T12:00:00Z, outside this
// window at both bounds, so `level=error` here means this script's rows.
const WINDOW_FROM = '2026-01-01T02:00:00Z';
const WINDOW_TO = '2026-01-01T06:00:00Z';
/** `span=2m` buckets that carry at least one event. Chosen above the
 * 50-row page so "whole" and "the first page" are different answers. */
const BUCKETS = 59;
/** Distinct `message` values, likewise above the page size. */
const GROUPS = 60;
/** The largest group count: the scale every bar is drawn against once
 * the whole result is in hand. */
const PEAK_COUNT = 15;
/** The group that carries it. Every OTHER group carries exactly one
 * event, which is what makes this corpus independent of result order:
 * `stats` emits no ORDER BY, so which 50 groups land on page 1 is the
 * engine's business. Whichever page does not hold the peak holds
 * nothing but count-1 groups, and a bar scaled against its own page
 * draws every one of them full width. */
const NAMED_PEAK = 'group-00';

const groupName = i => `group-${String(i).padStart(2, '0')}`;
const groupCount = i => (i === 0 ? PEAK_COUNT : 1);

/** Every event this script ingests. Pure: the same bytes every run. */
function corpus() {
  const base = Date.parse(WINDOW_FROM);
  const events = [];
  for (let i = 0; i < BUCKETS; i += 1) {
    // Mid-bucket, so a boundary rounding difference cannot move a row.
    const at = new Date(base + i * 120_000 + 30_000).toISOString().replace('.000Z', 'Z');
    for (let n = 0; n < 1 + (i % 7); n += 1) {
      events.push({ env: 'experiment', service: 'web', host: 'host-0', level: 'error',
        timestamp: at, message: `bucket-${String(i).padStart(2, '0')}-event-${n}` });
    }
  }
  // Every group at one instant inside the window: the stats query groups
  // by message and the times are irrelevant to it.
  const at = '2026-01-01T04:00:00Z';
  for (let i = 0; i < GROUPS; i += 1) {
    for (let n = 0; n < groupCount(i); n += 1) {
      events.push({ env: 'experiment', service: 'web', host: 'host-0', level: 'info',
        timestamp: at, message: groupName(i) });
    }
  }
  return events;
}

// ------------------------------------------------------------- transport

function post(url, { token, body, contentType }) {
  const encoded = typeof body === 'string' ? body : JSON.stringify(body);
  const transport = url.startsWith('https:') ? https : http;
  return new Promise((resolve, reject) => {
    const req = transport.request(url, { method: 'POST', rejectUnauthorized: false,
      headers: { authorization: `Bearer ${token}`, 'content-type': contentType,
        'content-length': Buffer.byteLength(encoded) } }, res => {
      let text = '';
      res.on('data', c => { text += c; });
      res.on('end', () => (res.statusCode >= 200 && res.statusCode < 300
        ? resolve(JSON.parse(text))
        // The path, never the origin: a failure message is not a place
        // to publish where this instance lives.
        : reject(new Error(`HTTP ${res.statusCode} ${new URL(url).pathname}`))));
    });
    req.on('error', reject);
    req.end(encoded);
  });
}

const ingest = batch => post(`${apiBase}/api/v1/ingest`, { token: ingestKey,
  contentType: 'application/x-ndjson', body: `${batch.map(e => JSON.stringify(e)).join('\n')}\n` });
const probe = query => post(`${apiBase}/api/v1/query`, { token: readerKey,
  contentType: 'application/json', body: { query, limit: 25_000 } });

// ------------------------------------------------------------- the queries

/** The defect's own query and window, as the issue states them. */
const TIMECHART = 'level=error | timechart span=2m count()';
/** The issue's second query, verbatim. No ordering stage: the bar chart
 * is offered only when the LAST pipeline stage is the `stats`, so a
 * `| sort` would take the bars off the page being photographed. The
 * corpus carries the determinism instead. */
const STATS = 'level=info | stats count() by message';
const range = `${WINDOW_FROM}..${WINDOW_TO}`;
const searchUrl = (query, page) =>
  `${origin}/search?q=${encodeURIComponent(query)}&r=${encodeURIComponent(range)}`
  + (page ? `&page=${page}` : '');

// ------------------------------------------------------------- transcript

const transcript = {
  schema: 1,
  label,
  status: 'running',
  capturedAt: new Date().toISOString(),
  // This file's own bytes: a transcript names the script that produced
  // it, so a reader can tell a rerun from a rewrite.
  captureSha256: createHash('sha256').update(await fs.readFile(fileURLToPath(import.meta.url))).digest('hex'),
  corpus: { window: { from: WINDOW_FROM, to: WINDOW_TO }, buckets: BUCKETS, groups: GROUPS,
    peakCount: PEAK_COUNT, namedPeak: NAMED_PEAK, singletonGroups: GROUPS - 1, events: 0 },
  queries: { timechart: TIMECHART, stats: STATS, range },
  probe: {},
  observations: [],
  screenshots: [],
  notes: [],
};

let browser;
let phase = 'seed';
try {
  await fs.mkdir(outDir, { recursive: true });
  const events = corpus();
  transcript.corpus.events = events.length;
  for (let i = 0; i < events.length; i += 100) {
    const accepted = await ingest(events.slice(i, i + 100));
    assert.equal(accepted.accepted, Math.min(100, events.length - i), 'batch not fully accepted');
    assert.equal(accepted.rejected || 0, 0, 'events rejected');
  }

  // Ingested is not yet searchable. Poll the daemon's own API until the
  // seeded rows are all visible, so a short read cannot be mistaken for
  // the defect this script is here to photograph.
  phase = 'probe';
  const bounded = `earliest="${WINDOW_FROM}" latest="${WINDOW_TO}"`;
  const deadline = Date.now() + 60_000;
  let buckets;
  let groups;
  for (;;) {
    buckets = await probe(`level=error ${bounded} | timechart span=2m count()`);
    groups = await probe(`level=info ${bounded} | stats count() by message`);
    if (buckets.rows.length === BUCKETS && groups.rows.length === GROUPS) break;
    assert.ok(Date.now() < deadline, 'seeded corpus did not become searchable');
    await delay(500);
  }
  const metric = groups.columns.findIndex(c => c.name !== 'message');
  transcript.probe = {
    note: 'Read from the daemon API with an explicit whole-result limit, independent of the browser.',
    timechartRows: buckets.rows.length,
    timechartTotal: buckets.pagination.total,
    statsRows: groups.rows.length,
    statsMaxCount: Math.max(...groups.rows.map(r => Number(r[metric]))),
    statsSingletons: groups.rows.filter(r => Number(r[metric]) === 1).length,
  };
  assert.equal(transcript.probe.statsMaxCount, PEAK_COUNT);
  assert.equal(transcript.probe.statsSingletons, GROUPS - 1, 'every group but the peak must carry one event');

  // ------------------------------------------------------------ browser
  phase = 'browser';
  // The Playwright the repository already depends on. No new dependency,
  // and the Chromium is the one `bin/app-experiment` installed.
  const require = createRequire(path.join(repo, 'crates/trawl-web-ui/e2e/package.json'));
  const { chromium } = require('@playwright/test');
  browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 },
    timezoneId: 'UTC', locale: 'en-US', reducedMotion: 'reduce', serviceWorkers: 'block' });
  // Nothing outside this instance, including redirects.
  await context.route('**/*', route =>
    (new URL(route.request().url()).origin === origin ? route.continue() : route.abort()));
  const page = await context.newPage();
  page.setDefaultTimeout(30_000);

  /** Every query the SPA posted, in order, with what came back. */
  let posted = [];
  page.on('response', response => {
    const request = response.request();
    if (request.method() !== 'POST') return;
    if (new URL(response.url()).pathname !== '/api/v1/query') return;
    const body = request.postDataJSON() || {};
    const record = { dsl: body.query, limit: body.limit, offset: body.offset ?? 0,
      status: response.status(), pagination: null };
    posted.push(record);
    // Attached, not awaited: the page turn must not wait on this read.
    void response.json().then(json => { record.pagination = json.pagination ?? null; })
      .catch(() => { record.pagination = 'unreadable'; });
  });

  await page.goto(`${origin}/login`);
  await page.getByLabel('API key', { exact: true }).fill(readerKey);
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await page.waitForURL('**/search');

  const shoot = async name => {
    // The footer prints the upstream endpoint it is connected to. A
    // disposable instance's address is not evidence and does not belong
    // in a committed image, so the text is replaced before the shutter,
    // not painted over afterwards.
    await page.locator('.statusbar .status-label').evaluateAll(nodes => nodes.forEach(node => {
      node.textContent = node.textContent.replace(/\(.*\)/, '(redacted)');
    }));
    await page.screenshot({ path: path.join(outDir, `${name}.png`) });
    transcript.screenshots.push(`${name}.png`);
  };

  /** The chart host's own account of what it plotted, or the documented
   * fallback when the revision does not publish one. */
  const plotted = async fetched => {
    const attribute = await page.locator('.chart').getAttribute('data-points');
    if (attribute !== null) {
      return { points: Number(attribute), source: 'chart host data-points attribute' };
    }
    return { points: fetched, source: 'response pagination.returned; this revision '
      + 'publishes no data-points attribute and a canvas cannot be read back' };
  };

  /** Every categorical bar on screen: its group label and the exact
   * inline width the component wrote. */
  const bars = () => page.locator('.cat-chart .cat-bars li').evaluateAll(items => items.map(li => ({
    group: li.querySelector('.cat-lb')?.textContent ?? null,
    value: li.querySelector('.cat-val')?.textContent ?? null,
    width: li.querySelector('.cat-track i')?.getAttribute('style') ?? null,
  })));

  const summary = async () => (await page.locator('.results-summary').first().innerText()).trim();
  const capLine = async () => (await page.locator('.results-cap').count()
    ? (await page.locator('.results-cap').first().innerText()).trim() : null);

  /** One query, both pages, through the real controls. */
  async function walk({ name, query, chart, categorical }) {
    posted = [];
    await page.goto(searchUrl(query));
    await page.locator('.results-table.exact tbody tr').first().waitFor();

    const pages = [];
    for (const human of [1, 2]) {
      if (human === 2) {
        const before = await page.locator('.results-table.exact tbody tr').first().innerText();
        await page.getByRole('button', { name: /^Next/ }).click();
        await page.waitForURL(/[?&]page=1/);
        await page.waitForFunction(
          previous => document.querySelector('.results-table.exact tbody tr')?.innerText !== previous,
          before);
      }
      // Wait for whichever response the page turn did or did not post to
      // finish being read, so the transcript is not racing the network.
      await delay(750);

      const record = { page: human, urlPage: human - 1, summary: await summary(),
        capLine: await capLine(), postedSoFar: posted.length };
      if (categorical) {
        record.bars = await bars();
        record.peakBar = record.bars.find(b => b.group === NAMED_PEAK) ?? null;
        const singletons = record.bars.filter(b => b.value === '1');
        // Order-independent by construction: every bar but the peak
        // stands for one event, so one distinct width per page is the
        // whole story, and the named example is an instance of it.
        record.singletons = { count: singletons.length,
          distinctWidths: [...new Set(singletons.map(b => b.width))].sort(),
          example: singletons[0] ?? null };
        await shoot(`${name}-events-page${human}`);
      }
      if (chart) {
        await page.getByRole('tab', { name: 'Visualization', exact: true }).click();
        // The chart mounts on a measured width; wait for the canvas or
        // for the refusal that replaces it.
        await page.locator('.visualization canvas, .visualization .results-empty').first().waitFor();
        const fetched = posted.at(-1)?.pagination?.returned ?? null;
        record.chart = await plotted(fetched);
        record.chartRefusal = await page.locator('.visualization .results-empty').count()
          ? (await page.locator('.visualization .results-empty').first().innerText()).trim() : null;
        await shoot(`${name}-visualization-page${human}`);
        await page.getByRole('tab', { name: /^Events/ }).click();
        await page.locator('.results-table.exact').waitFor();
      }
      pages.push(record);
    }
    transcript.observations.push({ name, query, requests: posted.map(r => ({
      limit: r.limit, offset: r.offset, status: r.status, pagination: r.pagination })), pages });
  }

  await walk({ name: 'timechart', query: TIMECHART, chart: true });
  await walk({ name: 'stats', query: STATS, categorical: true });

  // `stats` emits no ORDER BY, so a revision that asks the server for
  // page 2 separately gets a SECOND execution, free to order its groups
  // differently. Measure the two pages against each other rather than
  // asserting a clean partition: whether they cover the result once is
  // itself a difference between the revisions, not a precondition of
  // the measurement. The bar-width claim is per page and needs none of
  // it — every group but the peak carries one event, so a page without
  // the peak is a page of count-1 groups however it was chosen.
  const stats = transcript.observations.find(o => o.name === 'stats');
  const seen = stats.pages.flatMap(p => p.bars.map(b => b.group));
  transcript.coverage = { groupsInResult: GROUPS, rowsOnTheTwoPages: seen.length,
    distinctGroups: new Set(seen).size, onBothPages: seen.length - new Set(seen).size,
    neverShown: GROUPS - new Set(seen).size,
    peakOnPage: stats.pages.find(p => p.peakBar)?.page ?? null };
  transcript.notes.push(transcript.coverage.distinctGroups === GROUPS
    ? `The two table pages together held all ${GROUPS} groups, each exactly once.`
    : `The two table pages held ${transcript.coverage.distinctGroups} of ${GROUPS} distinct `
      + `groups: ${transcript.coverage.onBothPages} appeared on both and as many were never `
      + 'shown. Two server-side pages of an aggregation with no ordering stage are two '
      + 'executions, and nothing makes them agree on an order.');
  transcript.notes.push(`The peak group ${NAMED_PEAK} was on page `
    + `${transcript.coverage.peakOnPage ?? 'neither'}.`);

  transcript.status = 'captured';
} catch (error) {
  transcript.status = 'failed';
  // The message can quote a URL or a response body. Publish the phase.
  transcript.failure = { phase, kind: error instanceof assert.AssertionError ? 'assertion' : 'operation' };
  console.error(`failed in phase ${phase}: ${error.message}`);
  process.exitCode = 1;
} finally {
  await browser?.close().catch(() => {});
  await fs.writeFile(path.join(outDir, 'transcript.json'), `${JSON.stringify(transcript, null, 2)}\n`);
  console.log(`${transcript.status}: ${path.join(outDir, 'transcript.json')}`);
}
