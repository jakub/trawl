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
  meResponse,
  healthResponse,
  queryResponse,
  historyResponse,
  listSavedResponse,
  serviceSchemaResponse,
} from './fixtures.mjs';

const HOST = '127.0.0.1';
const PORT = 8123;

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

/** @type {'default'|'unauth'|'query-500'} */
let scenario = 'default';

const sse = {
  open: 0,
  opens: 0,
  closes: 0,
  /** @type {Set<import('node:http').ServerResponse>} */
  responses: new Set(),
};

/** @type {string[]} */
let unstubbed = [];
/** @type {object[]} */
let queries = [];

function resetState() {
  unstubbed = [];
  queries = [];
  // sse.open/opens/closes deliberately survive a reset — a spec resets
  // the scenario, then drives its own stream lifecycle and reads the
  // counters itself. Rolling them here would race a stream this same
  // request is about to open in the previous test's teardown.
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
  const timer = setInterval(() => {
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
  setTimeout(() => {
    if (sse.responses.has(res)) {
      res.end();
    }
  }, 1000);

  res.on('close', end);
}

// ---- request handling -------------------------------------------------------

const server = http.createServer(async (req, res) => {
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
      sendJson(res, 200, { ok: true, scenario });
      return;
    }
    if (p === '/__ctl/state' && req.method === 'GET') {
      sendJson(res, 200, {
        sse: { open: sse.open, opens: sse.opens, closes: sse.closes },
        unstubbed,
        queries,
      });
      return;
    }

    // -- auth --------------------------------------------------------------
    if (p === '/api/auth/me' && req.method === 'GET') {
      if (scenario === 'unauth') {
        sendJson(res, 401, errorEnvelope('unauthorized'));
        return;
      }
      sendJson(res, 200, meResponse());
      return;
    }
    if (p === '/api/auth/logout' && req.method === 'POST') {
      res.writeHead(204);
      res.end();
      return;
    }

    // -- health --------------------------------------------------------------
    if (p === '/api/v1/health' && req.method === 'GET') {
      sendJson(res, 200, healthResponse());
      return;
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
      sendJson(res, 200, queryResponse());
      return;
    }

    // -- SSE stream -----------------------------------------------------
    if (p === '/api/v1/stream' && req.method === 'GET') {
      serveStream(res);
      return;
    }

    // -- other read-only canned surfaces --------------------------------
    if (p === '/api/v1/history' && req.method === 'GET') {
      sendJson(res, 200, historyResponse());
      return;
    }
    if (p === '/api/v1/schema/services' && req.method === 'GET') {
      sendJson(res, 200, serviceSchemaResponse());
      return;
    }
    if (p === '/api/v1/saved' && req.method === 'GET') {
      sendJson(res, 200, listSavedResponse());
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
  for (const res of sse.responses) {
    try {
      res.end();
    } catch {
      // already closing
    }
  }
  server.close(() => process.exit(0));
  // Force-exit if close() hangs (a lingering keep-alive socket).
  setTimeout(() => process.exit(0), 500).unref();
}
process.on('SIGTERM', shutdown);
process.on('SIGINT', shutdown);
