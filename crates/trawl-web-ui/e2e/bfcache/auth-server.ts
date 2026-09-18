// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import http, { type ServerResponse } from 'node:http';
import type { AddressInfo } from 'node:net';
import { test as base, expect } from '@playwright/test';

export type Identity = { name: string; roles: string[]; permissions: string[]; exp: number };
export type AuthServer = {
  origin: string;
  identity: Identity | null;
  holdMe: boolean;
  pendingMe: number;
  requests: Array<{ path: string; method: string; identity: string | null }>;
  releaseMe: () => void;
};

export const identityFor = (name: string): Identity => ({
  name, roles: [], permissions: ['query', 'saved_query'],
  exp: Math.floor(Date.now() / 1000) + 3600,
});

export const test = base.extend<{ authServer: AuthServer }>({
  authServer: async ({ request }, use) => {
    const upstreamPort = Number(process.env.E2E_PORT ?? 8123);
    const reset = await request.post(`http://127.0.0.1:${upstreamPort}/__ctl/reset`, {
      data: { scenario: 'pagination', pagination: { runsTotal: 43 } },
    });
    expect(reset.ok()).toBe(true);
    const pending: ServerResponse[] = [];
    const state: AuthServer = {
      origin: '', identity: identityFor('previous-identity'), holdMe: false, pendingMe: 0,
      requests: [],
      releaseMe() {
        state.holdMe = false;
        for (const response of pending.splice(0)) sendIdentity(response);
        state.pendingMe = 0;
      },
    };
    const json = (response: ServerResponse, status: number, value: unknown) => {
      response.writeHead(status, { 'content-type': 'application/json', 'cache-control': 'no-store' });
      response.end(JSON.stringify(value));
    };
    const sendIdentity = (response: ServerResponse) => {
      if (!response.destroyed) json(response, state.identity ? 200 : 401, state.identity ?? {});
    };
    // Real loopback HTTP controls authentication timing without Playwright
    // routing, Fetch-domain interception, or changes to production assets.
    const server = http.createServer((req, res) => {
      const path = new URL(req.url!, 'http://127.0.0.1').pathname;
      state.requests.push({ path, method: req.method!, identity: state.identity?.name ?? null });
      if (path === '/api/auth/logout' && req.method === 'POST') {
        state.identity = null;
        req.resume();
        res.writeHead(204);
        res.end();
        return;
      }
      if (path === '/api/auth/me') {
        if (state.holdMe) {
          pending.push(res);
          state.pendingMe++;
        } else sendIdentity(res);
        return;
      }
      if (path.startsWith('/api/') && !state.identity) {
        req.resume();
        json(res, 401, {});
        return;
      }
      const upstream = http.request({
        hostname: '127.0.0.1', port: upstreamPort, path: req.url, method: req.method,
        headers: req.headers,
      }, response => {
        const headers = { ...response.headers };
        if (headers['content-type']?.includes('text/html')) {
          // Match the BFCache-enabled reproduction's document policy. The
          // shared harness still owns all asset bytes and its request cap.
          headers['cache-control'] = 'no-cache';
          headers['content-security-policy'] = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; font-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'";
          headers['x-content-type-options'] = 'nosniff';
          headers['referrer-policy'] = 'no-referrer';
          headers['x-frame-options'] = 'DENY';
        }
        res.writeHead(response.statusCode!, headers);
        response.pipe(res);
      });
      upstream.on('error', () => {
        if (!res.headersSent) res.writeHead(502);
        res.end();
      });
      req.pipe(upstream);
    });
    await new Promise<void>((resolve, reject) => {
      server.once('error', reject);
      server.listen(0, '127.0.0.1', resolve);
    });
    state.origin = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
    try {
      await use(state);
    } finally {
      for (const response of pending.splice(0)) response.destroy();
      server.closeAllConnections();
      await new Promise<void>(resolve => server.close(() => resolve()));
      const final = await (await request.get(`http://127.0.0.1:${upstreamPort}/__ctl/state`)).json();
      expect(final.unstubbed).toEqual([]);
      expect(final.unhandledQueries ?? []).toEqual([]);
    }
  },
});

export { expect };
