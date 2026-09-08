// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Zero-npm-dependency static + API stub server for the Playwright e2e
// suite. node:http only, on purpose — this file ships no third-party
// surface at all, so `npm audit` never has anything to say about it.
//
// Serves the built trawl-web-ui `dist/` (SPA fallback on extensionless
// paths) and stands in for trawl-web's `/api/*` surface with canned,
// wire-shape-accurate responses (see fixtures.mjs). Single mutable
// "current scenario" — set via POST /__ctl/reset — because
// playwright.config.ts pins workers:1 so exactly one spec talks to this
// process at a time.

import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  wire,
  meResponse,
  healthResponse,
  queryResponse,
  historyResponse,
  listSavedResponse,
  populatedListSavedResponse,
  populatedServiceSchemaResponse,
  serviceSchemaResponse,
  catalogFieldResponse,
  repinStatusRunningResponse,
  repinStatusSucceededResponse,
  repinStatusNoJobResponse,
  QUERY_SHAPES,
  corpusQueryRowsResponse,
  corpusCardinalityResponse,
  corpusTopValuesResponse,
  corpusTimechartResponse,
  corpusServiceSchemaResponse,
  corpusHistoryResponse,
  corpusNetRunsResponse,
  corpusRunResultResponse,
  corpusAllRunsResponse,
  corpusRunsStatsResponse,
} from './fixtures.mjs';

const HOST = '127.0.0.1';
// E2E_PORT lets parallel worktrees run without colliding; the Playwright
// config reads the same variable, so server and baseURL can't disagree.
const PORT = Number(process.env.E2E_PORT ?? 8123);

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const DIST = path.resolve(
  process.env.TRAWL_E2E_DIST || path.join(__dirname, '..', '..', 'dist'),
);

if (!fs.existsSync(path.join(DIST, 'index.html'))) {
  console.error(
    `e2e harness: no built SPA found at ${DIST}/index.html.\n` +
      'Run `cargo xtask e2e` without --skip-build (or `trunk build` in ' +
      'crates/trawl-web-ui/) before running the suite directly.',
  );
  process.exit(1);
}

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.wasm': 'application/wasm',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.woff2': 'font/woff2',
  '.woff': 'font/woff',
  '.svg': 'image/svg+xml',
  '.ico': 'image/x-icon',
  '.png': 'image/png',
  '.txt': 'text/plain; charset=utf-8',
};

// ---- mutable test-scoped state -------------------------------------------

/** @type {'default'|'unauth'|'query-500'|'stream-burst'|'populated'|'corpus'} */
let scenario = 'default';

// Dashboard counters survive resets. Resetting a scenario cannot hide a late
// socket close or turn a leaked connection into a fresh baseline.
const dashboard = {
  open: 0, opens: 0, closes: 0, max: 0, responses: new Set(),
  hold: false, bootstrap: 'ok', pending: new Set(), cancel: 'accepted',
};
let healthHits = {};
let cancelRequests = [];
function healthScenario() { return scenario.startsWith('health-'); }
function healthIdentity() {
  const permissions = scenario === 'health-admin' ? ['query', 'server_manage']
    : scenario === 'health-admin-no-query' ? ['server_manage']
    : scenario === 'health-cancel' ? ['query', 'query_cancel']
    : scenario === 'health-no-query' ? ['schema_read'] : ['query'];
  return { ...meResponse(), name: 'same-name', permissions };
}
function dashboardBody(bootstrap = false) {
  const body = wire('health-dashboard');
  if (bootstrap) { body.hostname = 'old-bootstrap-host'; body.hot_buffer_events = 111; }
  return body;
}
function pushDashboard() {
  for (const response of dashboard.responses) {
    response.write(`event: stats\ndata: ${JSON.stringify(dashboardBody())}\n\n`);
  }
}
function serveDashboard(res) {
  res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
  res.write('retry: 200\n\n');
  dashboard.open += 1; dashboard.opens += 1;
  dashboard.max = Math.max(dashboard.max, dashboard.open);
  dashboard.responses.add(res);
  res.on('close', () => {
    if (dashboard.responses.delete(res)) { dashboard.open -= 1; dashboard.closes += 1; }
  });
  if (!dashboard.hold) {
    res.write(`event: stats\ndata: ${JSON.stringify(dashboardBody())}\n\n`);
  }
}


/** `corpus` is `populated` plus data. Every place that used to ask
 * "is this `populated`?" asks this instead, so the two scenarios cannot
 * drift apart on a body they share. The services route is the one
 * exception and says so where it splits. */
function hasCorpus() {
  return scenario === 'populated' || scenario === 'corpus';
}

const sse = {
  open: 0,
  opens: 0,
  closes: 0,
  /** @type {Set<import('node:http').ServerResponse>} */
  responses: new Set(),
};

// Scripted repin status, for the field case drawer's poll. Unlike the SSE
// counters below, EVERY field here is rolled by a reset — see the note in
// `resetState`. The two rules are deliberately different and must stay
// that way: unifying them would either strand a held response across
// tests or reset a counter under a stream that is still opening.
const repin = {
  /** The field the scripted job belongs to. `null` = script disarmed, and
   * the status route answers `{ job: null }` forever, so no other spec's
   * behaviour changes. */
  field: null,
  /** Status reads served since the reset that armed the script. */
  hits: 0,
  /** The parked `ServerResponse` for hit `HELD_HIT` — headers not even
   * written, so the client is still waiting on it.
   * @type {import('node:http').ServerResponse|null} */
  held: null,
  /** The held socket closed before anything was written to it. A release
   * after that is a lie, and `/__ctl/repin/release` answers 409. */
  heldAborted: false,
  /** Whether the held response was still pending at the moment
   * `/__ctl/repin/release` wrote to it. This is the alive-latch evidence:
   * a read still open across a teardown is a read the client never
   * abandoned. */
  pendingAtRelease: null,
};

/** Which status read is parked. Read 1 is the drawer's mount probe, read
 * 2 the immediate `poll_once` the probe's adoption starts — so holding 2
 * leaves the drawer with a read in flight for the whole test. */
const HELD_HIT = 2;

/** @type {string[]} */
let unstubbed = [];
/** @type {object[]} */
let queries = [];
/** @type {object[]} */
let exports_ = [];
/** DSL of every `corpus` query the shape dispatch did not recognise. Kept
 * beside `unstubbed` and for the same reason: a query nobody keyed a
 * fixture to must be visible as itself, never answered with rows a spec
 * would read as its own. */
let unhandledQueries = [];

function resetState() {
  healthHits = {};
  cancelRequests = [];
  dashboard.hold = false;
  dashboard.bootstrap = 'ok';
  dashboard.cancel = 'accepted';
  for (const response of dashboard.pending) response.destroy();
  dashboard.pending.clear();
  unstubbed = [];
  queries = [];
  exports_ = [];
  unhandledQueries = [];
  // sse.open/opens/closes deliberately survive a reset — a spec resets
  // the scenario, then drives its own stream lifecycle and reads the
  // counters itself. Rolling them here would race a stream this same
  // request is about to open in the previous test's teardown.
  //
  // The repin script is the opposite: it is armed BY a reset (which
  // carries the field to script) and its whole point is a per-test
  // sequence, so nothing may survive into the next test. A held response
  // outliving its test would park a socket forever.
  if (repin.held) {
    repin.held.destroy();
  }
  repin.field = null;
  repin.hits = 0;
  repin.held = null;
  repin.heldAborted = false;
  repin.pendingAtRelease = null;
}

// ---- helpers --------------------------------------------------------------

function sendJson(res, status, body) {
  const text = JSON.stringify(body);
  res.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'content-length': Buffer.byteLength(text),
  });
  res.end(text);
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    req.on('error', reject);
  });
}

function errorEnvelope(message) {
  return { error: { code: 'execution_error', message } };
}

// ---- static file serving with SPA fallback --------------------------------

function serveStatic(req, res, urlPath) {
  const clean = urlPath.split('?')[0];
  const hasExt = path.extname(clean) !== '';
  const rel = hasExt ? clean : '/index.html';
  const filePath = path.join(DIST, rel);

  // Guard against escaping DIST via a crafted path.
  if (!filePath.startsWith(DIST)) {
    res.writeHead(403);
    res.end('forbidden');
    return;
  }

  fs.readFile(filePath, (err, data) => {
    if (err) {
      res.writeHead(404);
      res.end('not found');
      return;
    }
    const ext = path.extname(filePath);
    res.writeHead(200, { 'content-type': MIME[ext] || 'application/octet-stream' });
    res.end(data);
  });
}

// ---- SSE ------------------------------------------------------------------

function serveStream(res) {
  res.writeHead(200, {
    'content-type': 'text/event-stream; charset=utf-8',
    'cache-control': 'no-cache',
    connection: 'keep-alive',
  });
  res.write('retry: 200\n\n');

  sse.open += 1;
  sse.opens += 1;
  sse.responses.add(res);

  let n = 0;
  const burst = scenario === 'stream-burst';
  const timer = burst ? undefined : setInterval(() => {
    n += 1;
    res.write(`event: data\ndata: ${JSON.stringify({ _time: new Date().toISOString(), message: `tick ${n}` })}\n\n`);
  }, 150);

  const end = () => {
    clearInterval(timer);
    if (sse.responses.delete(res)) {
      sse.open -= 1;
      sse.closes += 1;
    }
  };

  // Deliberately END the response after ~1s server-side: this is what
  // makes a *leaked* client-side EventSource observable — a browser
  // EventSource auto-reconnects on a server-closed stream, so if the
  // SPA failed to `close()` it on unmount we'll see `opens` keep
  // climbing every ~200ms (the `retry:` interval) after teardown.
  if (!burst) setTimeout(() => {
    if (sse.responses.has(res)) {
      res.end();
    }
  }, 1000);

  res.on('close', end);
  if (burst) {
    for (let seq = 0; seq < 6000; seq++) {
      const event = { seq, message: `burst-${seq}` };
      if (seq < 1000) event.expired_only = true;
      if (seq === 5999) event.late_column = 'arrived';
      res.write(`event: data\ndata: ${JSON.stringify(event)}\n\n`);
    }
  }
}

// ---- request handling -------------------------------------------------------

// Node's default header cap is 16 KiB INCLUDING the request line, so a
// deliberately oversized search link (the SPA's own bound is 32 KiB)
// would be answered with a 431 by the harness before the app ever saw
// it. The spec that proves the app refuses such a link needs the link to
// arrive, so this stub carries more than any real deployment would.
const server = http.createServer({ maxHeaderSize: 256 * 1024 }, async (req, res) => {
  let url;
  let p;

  try {
    // Inside the try: a malformed Host header (only a non-browser local
    // client can send one) must 500 this request, not crash the process
    // via an unhandled async rejection.
    url = new URL(req.url, `http://${req.headers.host}`);
    p = url.pathname;
    // -- control plane ---------------------------------------------------
    if (p === '/__ctl/health' && req.method === 'GET') {
      res.writeHead(200, { 'content-type': 'text/plain' });
      res.end('ok');
      return;
    }
    if (p === '/__ctl/reset' && req.method === 'POST') {
      const body = await readBody(req);
      let parsed = {};
      try {
        parsed = body ? JSON.parse(body) : {};
      } catch {
        // malformed body -> keep default scenario
      }
      scenario = parsed.scenario || 'default';
      resetState();
      // The ONE door that arms the repin script. Everything else reads
      // `repin`; only a reset writes `field`, so a spec cannot end up
      // scripting a second job on top of a live one.
      repin.field = parsed.repinField || null;
      dashboard.hold = parsed.dashboardHold ?? false;
      dashboard.bootstrap = parsed.dashboardBootstrap ?? 'ok';
      dashboard.cancel = parsed.cancelOutcome ?? 'accepted';
      sendJson(res, 200, { ok: true, scenario, repinField: repin.field });
      return;
    }
    if (p === '/__ctl/state' && req.method === 'GET') {
      sendJson(res, 200, {
        sse: { open: sse.open, opens: sse.opens, closes: sse.closes },
        repin: {
          field: repin.field,
          hits: repin.hits,
          held: repin.held !== null,
          aborted: repin.heldAborted,
          pendingAtRelease: repin.pendingAtRelease,
        },
        dashboard: {
          open: dashboard.open, opens: dashboard.opens, closes: dashboard.closes,
          max: dashboard.max, pending: dashboard.pending.size,
        },
        healthHits,
        cancelRequests,
        unstubbed,
        queries,
        unhandledQueries,
        exports: exports_,
      });
      return;
    }
    // Finish the parked status read. A spec calls this AFTER the drawer
    // is gone: a 200 with `pending: true` means the response was still
    // open at that moment, which is only true if the client never
    // abandoned it. A premature call (nothing held) or one after the
    // socket died must fail loudly — a 409 read as proof would be the
    // whole assertion inverted.
    if (p === '/__ctl/repin/release' && req.method === 'POST') {
      if (!repin.held || repin.heldAborted) {
        sendJson(res, 409, {
          ok: false,
          pending: false,
          aborted: repin.heldAborted,
          hits: repin.hits,
        });
        return;
      }
      const held = repin.held;
      // Captured before the write, with nothing awaited in between: this
      // is the fact the spec is buying, and reading it after the write
      // would read the write's own effect.
      repin.pendingAtRelease = true;
      repin.held = null;
      const body = repinStatusSucceededResponse();
      body.job.field = repin.field;
      sendJson(held, 200, body);
      sendJson(res, 200, { ok: true, pending: true, aborted: false, hits: repin.hits });
      return;
    }

    if (p === '/__ctl/dashboard/release' && req.method === 'POST') {
      dashboard.hold = false;
      pushDashboard();
      sendJson(res, 200, { ok: true, open: dashboard.open });
      return;
    }
    if (p === '/__ctl/dashboard/drop' && req.method === 'POST') {
      dashboard.hold = true;
      for (const response of dashboard.responses) response.end();
      sendJson(res, 200, { ok: true });
      return;
    }
    if (p === '/__ctl/dashboard/bootstrap-release' && req.method === 'POST') {
      const count = dashboard.pending.size;
      for (const response of dashboard.pending) sendJson(response, 200, dashboardBody(true));
      dashboard.pending.clear();
      sendJson(res, 200, { ok: count > 0, count });
      return;
    }

    // -- auth --------------------------------------------------------------
    if (p === '/api/auth/me' && req.method === 'GET') {
      if (scenario === 'unauth') {
        sendJson(res, 401, errorEnvelope('unauthorized'));
        return;
      }
      sendJson(res, 200, healthScenario() ? healthIdentity() : meResponse());
      return;
    }
    if (p === '/api/auth/logout' && req.method === 'POST') {
      res.writeHead(204);
      res.end();
      return;
    }

    // -- health --------------------------------------------------------------
    if (p === '/api/v1/health' && req.method === 'GET') {
      healthHits.health = (healthHits.health ?? 0) + 1;
      sendJson(res, scenario === 'health-degraded' ? 503 : 200,
        healthScenario() ? wire(scenario === 'health-degraded' ? 'health-unavailable' : 'health-ok') : healthResponse());
      return;
    }

    // Health scenarios answer forbidden admin reads as well as counting them.
    // A missing client gate must fail the request-silence assertion itself.
    if (healthScenario()) {
      const key = p === '/api/v1/stats' ? 'stats'
        : p === '/api/v1/dashboard' ? 'dashboard'
        : p === '/api/v1/dashboard/stream' ? 'stream'
        : p === '/api/v1/queries' ? 'queries' : null;
      if (key && req.method === 'GET') {
        healthHits[key] = (healthHits[key] ?? 0) + 1;
        const permissions = healthIdentity().permissions;
        if (!(key === 'queries' ? permissions.includes('query') : permissions.includes('server_manage'))) {
          sendJson(res, 403, errorEnvelope('insufficient permissions'));
        } else if (key === 'stats') sendJson(res, 200, wire('health-stats'));
        else if (key === 'queries') sendJson(res, 200, wire('health-queries'));
        else if (key === 'stream') serveDashboard(res);
        else if (dashboard.bootstrap === 'waiting') sendJson(res, 503, errorEnvelope('dashboard data not yet available'));
        else if (dashboard.bootstrap === 'held') {
          dashboard.pending.add(res);
          res.on('close', () => dashboard.pending.delete(res));
        } else sendJson(res, 200, dashboardBody(true));
        return;
      }
      const cancel = p.match(/^\/api\/v1\/queries\/(\d+)$/);
      if (cancel && req.method === 'DELETE') {
        const id = Number(cancel[1]);
        cancelRequests.push(id);
        if (dashboard.cancel === 'unknown') {
          // A response the client cannot decode leaves the mutation outcome
          // unknown. Destroying a headerless socket would let Chromium retry
          // this idempotent DELETE before reporting the network failure.
          res.writeHead(200, { 'content-type': 'application/json' });
          res.end('{"cancelled":');
        }
        else {
          const body = wire(dashboard.cancel === 'finished' ? 'health-cancel-finished' : 'health-cancel-accepted');
          body.query_id = id;
          sendJson(res, 200, body);
        }
        return;
      }
    }

    // -- query ---------------------------------------------------------------
    if (p === '/api/v1/query' && req.method === 'POST') {
      const bodyText = await readBody(req);
      let parsedBody = {};
      try {
        parsedBody = bodyText ? JSON.parse(bodyText) : {};
      } catch {
        // fall through with an empty body record
      }
      queries.push(parsedBody);
      if (scenario === 'query-500') {
        sendJson(res, 500, errorEnvelope('Couldn’t load results: stub query failure'));
        return;
      }
      if (scenario === 'corpus') {
        // Dispatch by DSL SHAPE, because the service drawer's reads
        // carry a field or service name the stub cannot predict. The
        // order matters only in that each shape is checked before the
        // pipeline catch-all below it.
        const dsl = typeof parsedBody.query === 'string' ? parsedBody.query : '';
        if (dsl.includes(QUERY_SHAPES.cardinality)) {
          sendJson(res, 200, corpusCardinalityResponse());
          return;
        }
        if (dsl.includes(QUERY_SHAPES.topValues)) {
          sendJson(res, 200, corpusTopValuesResponse());
          return;
        }
        if (dsl.includes(QUERY_SHAPES.timechart)) {
          sendJson(res, 200, corpusTimechartResponse());
          return;
        }
        // A pipeline this scenario has no fixture for, such as the
        // drawer's `stats count() as hits` collision form, FAILS.
        // Answering it with the rows fixture would hand a spec a body
        // that says nothing about the query it asked, which is the
        // failure mode this dispatch exists to prevent. A bare `|` test
        // is enough here: no fixture query quotes one.
        if (dsl.includes('|')) {
          unhandledQueries.push(dsl);
          sendJson(res, 500, errorEnvelope(`no corpus fixture for this pipeline shape: ${dsl}`));
          return;
        }
        sendJson(res, 200, corpusQueryRowsResponse());
        return;
      }
      sendJson(res, 200, queryResponse());
      return;
    }

    // -- export -------------------------------------------------------
    // Captured for the same reason queries are: a spec that asserts a
    // refused link exported NOTHING needs a counter, and an export that
    // slipped through must be visible as itself rather than as an
    // `unstubbed` line item.
    if (p === '/api/v1/export' && req.method === 'POST') {
      const bodyText = await readBody(req);
      let parsedBody = {};
      try {
        parsedBody = bodyText ? JSON.parse(bodyText) : {};
      } catch {
        // fall through with an empty body record
      }
      exports_.push({ format: url.searchParams.get('format'), ...parsedBody });
      res.writeHead(200, {
        'content-type': 'text/csv; charset=utf-8',
        'content-disposition': 'attachment; filename="export.csv"',
      });
      res.end('service,count\nnginx,1\n');
      return;
    }

    // -- SSE stream -----------------------------------------------------
    if (p === '/api/v1/stream' && req.method === 'GET') {
      serveStream(res);
      return;
    }

    // -- other read-only canned surfaces --------------------------------
    if (p === '/api/v1/history' && req.method === 'GET') {
      sendJson(res, 200, scenario === 'corpus' ? corpusHistoryResponse() : historyResponse());
      return;
    }
    // The `populated` scenario answers these two with a corpus that has
    // something in it: one service and one net. It is an ADDITIONAL
    // scenario, never a change to `default` — the schema and nets specs
    // written before it assert on the empty state, and the wire fixture
    // contract pins both halves.
    if (p === '/api/v1/schema/services' && req.method === 'GET') {
      sendJson(
        res,
        200,
        // Three bodies, not two: `corpus` gets its own so it can carry a
        // degraded column, and `populated` keeps the body its specs were
        // written against.
        scenario === 'corpus'
          ? corpusServiceSchemaResponse()
          : scenario === 'populated'
            ? populatedServiceSchemaResponse()
            : serviceSchemaResponse(),
      );
      return;
    }
    if (p === '/api/v1/saved' && req.method === 'GET') {
      sendJson(
        res,
        200,
        hasCorpus() ? populatedListSavedResponse() : listSavedResponse(),
      );
      return;
    }

    // -- report runs (`corpus` only) ------------------------------------
    // Gated on the scenario rather than answered everywhere: under every
    // other scenario these paths fall through to the `unstubbed`
    // catch-all, and the auto fixture's empty-`unstubbed` assertion is
    // what tells a spec author they reached a surface they did not
    // fixture. The run result is answered for ANY run id — expanding a
    // row is the behaviour under test, not id routing.
    const netRuns = p.match(/^\/api\/v1\/saved\/\d+\/runs$/);
    const netRun = p.match(/^\/api\/v1\/saved\/\d+\/runs\/\d+$/);
    if (scenario === 'corpus' && req.method === 'GET') {
      if (netRuns) {
        sendJson(res, 200, corpusNetRunsResponse());
        return;
      }
      if (netRun) {
        sendJson(res, 200, corpusRunResultResponse());
        return;
      }
      if (p === '/api/v1/runs') {
        sendJson(res, 200, corpusAllRunsResponse());
        return;
      }
      if (p === '/api/v1/runs/stats') {
        sendJson(res, 200, corpusRunsStatsResponse());
        return;
      }
    }

    // -- field catalog --------------------------------------------------
    // The case file's own fetch. The name is whatever the URL asked for,
    // so a spec that opens `?field=status` gets a case file for `status`
    // rather than for whatever the fixture happens to be named.
    if (p === '/api/v1/schema/field' && req.method === 'GET') {
      const body = catalogFieldResponse();
      body.name = url.searchParams.get('name') ?? body.name;
      sendJson(res, 200, body);
      return;
    }

    // -- repin status ---------------------------------------------------
    // Scripted, and only once a reset armed it (see `repin` above). The
    // sequence is: hit 1 running, hit HELD_HIT parked open and never
    // answered, everything after that succeeded. Holding a read open is
    // what makes the drawer's alive latch observable — the parked
    // response is still there to release once the drawer is gone.
    if (p === '/api/v1/schema/repin/status' && req.method === 'GET') {
      if (repin.field === null) {
        sendJson(res, 200, repinStatusNoJobResponse());
        return;
      }
      repin.hits += 1;
      if (repin.hits === HELD_HIT) {
        repin.held = res;
        res.on('close', () => {
          // Identity-checked: a `close` for a response a later reset
          // already destroyed must not clobber the fresh state.
          if (repin.held === res) {
            repin.held = null;
            // Nothing was written, so the socket went away under a read
            // the client was still owed. `/__ctl/repin/release` reports
            // this instead of pretending it delivered.
            repin.heldAborted = true;
          }
        });
        return;
      }
      const body =
        repin.hits < HELD_HIT ? repinStatusRunningResponse() : repinStatusSucceededResponse();
      body.job.field = repin.field;
      sendJson(res, 200, body);
      return;
    }

    // -- anything else under /api: 200 {} AND recorded as unstubbed -----
    if (p.startsWith('/api/')) {
      unstubbed.push(`${req.method} ${p}`);
      sendJson(res, 200, {});
      return;
    }

    // -- static SPA -------------------------------------------------------
    serveStatic(req, res, p);
  } catch (e) {
    // If headers already went out (e.g. a throw mid-SSE stream), writeHead
    // would itself throw ERR_HTTP_HEADERS_SENT and kill the process.
    if (!res.headersSent) {
      res.writeHead(500, { 'content-type': 'text/plain' });
    }
    res.end(String(e && e.stack ? e.stack : e));
  }
});

server.listen(PORT, HOST, () => {
  console.log(`e2e stub server listening on http://${HOST}:${PORT} (dist: ${DIST})`);
});

// End every open SSE response on SIGTERM so playwright's webServer
// teardown never hangs on a keep-alive connection.
function shutdown() {
  for (const res of dashboard.responses) res.end();
  for (const res of dashboard.pending) res.destroy();
  for (const res of sse.responses) {
    try {
      res.end();
    } catch {
      // already closing
    }
  }
  // Same reason: a status read parked with no headers written keeps its
  // socket alive, and `server.close()` waits for it.
  if (repin.held) {
    try {
      repin.held.destroy();
    } catch {
      // already closing
    }
    repin.held = null;
  }
  server.close(() => process.exit(0));
  // Force-exit if close() hangs (a lingering keep-alive socket).
  setTimeout(() => process.exit(0), 500).unref();
}
process.on('SIGTERM', shutdown);
process.on('SIGINT', shutdown);
