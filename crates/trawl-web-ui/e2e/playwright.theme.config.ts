// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { defineConfig, devices } from '@playwright/test';
import path from 'node:path';

// The root runner builds this binary and both distributions before invoking
// Playwright. There is no build or Node SPA substitute inside this config.
const binary = process.env.THEME_ASSET_SERVER;
const trawl = process.env.THEME_TRAWL_DIST;
const workbench = process.env.THEME_WORKBENCH_DIST;
if (!binary || !trawl || !workbench) throw new Error('Set THEME_ASSET_SERVER, THEME_TRAWL_DIST and THEME_WORKBENCH_DIST to built artifacts');
const port = Number(process.env.THEME_E2E_PORT ?? 8136);
const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;
const origins = [port, port + 1, port + 2].map(port => `http://127.0.0.1:${port}`);
const servers = [
  `--mode embedded --bind 127.0.0.1:${port}`,
  `--mode disk --bind 127.0.0.1:${port + 1} --dist ${quote(path.resolve(trawl))}`,
  `--mode disk --bind 127.0.0.1:${port + 2} --dist ${quote(path.resolve(workbench))}`,
];

export default defineConfig({
  testDir: './theme-tests',
  outputDir: '../../../e2e-artifacts/theme-production',
  workers: 1, fullyParallel: false, retries: 0, timeout: 30_000,
  expect: { timeout: 10_000 },
  reporter: [['list'], ['html', { outputFolder: '../../../e2e-artifacts/theme-report', open: 'never' }]],
  use: { ...devices['Desktop Chrome'], channel: 'chromium', colorScheme: 'light', trace: 'on', screenshot: 'on' },
  projects: [
    { name: 'embedded', testMatch: 'production.spec.ts', use: { baseURL: origins[0] } },
    { name: 'disk', testMatch: 'production.spec.ts', use: { baseURL: origins[1] } },
    { name: 'workbench', testMatch: ['production.spec.ts', 'workbench.spec.ts'], use: { baseURL: origins[2] } },
  ],
  webServer: servers.map((args, i) => ({
    command: `${quote(path.resolve(binary))} ${args}`,
    url: `${origins[i]}/__theme_health`, reuseExistingServer: false, stdout: 'pipe' as const,
  })),
});
