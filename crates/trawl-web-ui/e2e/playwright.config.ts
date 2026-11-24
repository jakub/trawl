// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { defineConfig, devices } from '@playwright/test';

// No `webServer`: every worker starts its own harness/server.mjs on
// E2E_PORT + its parallelIndex (the worker-scoped `stubOrigin` fixture in
// fixtures.ts). A stub server keeps one mutable "current scenario", so it
// must never serve two tests at once.
export default defineConfig({
  testDir: './tests',
  // Scratch output lands at the repository root, not beside the suite.
  // `trunk serve` watches crates/trawl-web-ui, so a write anywhere below it
  // wakes a running dev server for a full rebuild, and that rebuild clears
  // `dist/.stage` under any concurrent `trunk build`.
  outputDir: '../../../e2e-artifacts/test-results',
  workers: process.env.CI ? 1 : 4,
  // Tests are independent (the contract fixture resets the scenario before
  // each one), so a file's tests may run on different workers and shards.
  fullyParallel: true,
  retries: 0,
  timeout: 20_000,
  expect: { timeout: 5_000 },
  // Default for local and focused mutation runs. The complete CI suite owns
  // its larger aggregate budget in ci.yml.
  globalTimeout: 720_000,
  reporter: process.env.CI ? [['github'], ['list']] : [['list']],
  // In CI, Playwright otherwise runs `git fetch` on every invocation and
  // prints any failure to stdout, which corrupts mutation-check.sh's JSON
  // reports.
  captureGitInfo: { commit: false, diff: false },
  use: {
    trace: 'retain-on-failure',
    // The contract fixture in fixtures.ts takes the failure screenshot
    // while routes still answer; see the comment there.
    screenshot: 'off',
    viewport: { width: 1440, height: 900 },
    locale: 'en-US',
    timezoneId: 'UTC',
    colorScheme: 'light',
  },
  projects: [
    {
      name: 'chromium',
      // Full Chromium avoids the headless-shell renderer crashes seen
      // during native new-tab tests, including a plain HTML reproduction.
      use: { ...devices['Desktop Chrome'], channel: 'chromium' },
    },
  ],
});
