// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { defineConfig, devices } from '@playwright/test';

const port = Number(process.env.E2E_PORT ?? 8123);

// The normal fixture intercepts requests and Chromium normally disables
// BFCache. This separate suite must exercise actual cached document restores.
export default defineConfig({
  testDir: './bfcache',
  outputDir: '../../../e2e-artifacts/bfcache',
  workers: 1,
  fullyParallel: false,
  retries: 0,
  timeout: 30000,
  expect: { timeout: 8000 },
  reporter: [
    ...(process.env.CI ? [['github'] as const] : []),
    ['list'], ['html', { outputFolder: '../../../e2e-artifacts/bfcache-report', open: 'never' }],
  ],
  use: {
    ...devices['Desktop Chrome'],
    channel: 'chromium',
    launchOptions: { ignoreDefaultArgs: ['--disable-back-forward-cache'] },
    // Trace recording can perturb BFCache. The tests attach their explicit
    // document lifecycle and server request evidence on both pass and fail.
    trace: 'off',
    screenshot: 'only-on-failure',
  },
  webServer: {
    command: 'node harness/server.mjs',
    url: `http://127.0.0.1:${port}/__ctl/health`,
    reuseExistingServer: false,
    stdout: 'pipe',
  },
});
