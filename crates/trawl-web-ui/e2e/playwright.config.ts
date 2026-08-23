// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { defineConfig, devices } from '@playwright/test';

// Same variable harness/server.mjs reads — one owner for the port.
const PORT = Number(process.env.E2E_PORT ?? 8123);

// One stub server, one scenario in flight at a time (harness/server.mjs
// keeps a single mutable "current scenario" — see its `/__ctl/reset`
// contract) — so specs never run concurrently against it.
export default defineConfig({
  testDir: './tests',
  workers: 1,
  fullyParallel: false,
  retries: 0,
  timeout: 20_000,
  expect: { timeout: 5_000 },
  globalTimeout: 240_000,
  reporter: process.env.CI ? [['github'], ['list']] : [['list']],
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    viewport: { width: 1440, height: 900 },
    locale: 'en-US',
    timezoneId: 'UTC',
    colorScheme: 'light',
    reducedMotion: 'reduce',
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],
  webServer: {
    command: 'node harness/server.mjs',
    url: `http://127.0.0.1:${PORT}/__ctl/health`,
    // Never reuse: a server left by another worktree would serve THAT
    // checkout's dist and this run would silently test the wrong SPA.
    // A port collision must fail loudly instead (set E2E_PORT to run
    // suites in parallel).
    reuseExistingServer: false,
    stdout: 'pipe',
  },
});
