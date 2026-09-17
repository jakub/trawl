// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Build the SPA first. This runs the exact Health assertions that create the
// wide/narrow captures, using Playwright's existing owned harness lifecycle.
// E2E_PORT=8193 node crates/trawl-web-ui/e2e/scripts/issue193-visual-evidence.mjs /tmp/issue193-captures
import { spawnSync, execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { snapshotPath } from '../harness/dist-snapshot.mjs';

const require = createRequire(import.meta.url);
const e2e = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const output = path.resolve(process.argv[2] ?? path.join(e2e, '../../../visual-evidence/issue193'));
const port = Number(process.env.E2E_PORT ?? 8193);
fs.mkdirSync(output, { recursive: true });
const git = (...args) => execFileSync('git', args, { cwd: e2e, encoding: 'utf8' }).trimEnd();
const headBefore = git('rev-parse', 'HEAD');
const statusBefore = git('status', '--porcelain=v1');
const result = spawnSync(process.execPath, [require.resolve('@playwright/test/cli'), 'test',
  '--config', path.join(e2e, 'playwright.config.ts'), 'health-page.spec.ts',
  '--grep', 'diagnostic band capture', '--workers=1'], {
  cwd: e2e, stdio: 'inherit',
  env: { ...process.env, E2E_PORT: String(port), TRAWL_HEALTH_CAPTURE_DIR: output },
});
if (result.error) throw result.error;
if (result.status !== 0) process.exit(result.status ?? 1);
const captures = [1440, 720].map(width => JSON.parse(
  fs.readFileSync(path.join(output, `health-diagnostics-${width}.json`), 'utf8'),
));
const images = captures.flatMap(capture => capture.frames.map(frame => ({
  ...frame,
  sha256: createHash('sha256').update(fs.readFileSync(path.join(output, frame.name))).digest('hex'),
})));
const dist = snapshotPath(port);
const hash = createHash('sha256');
for (const file of fs.readdirSync(dist, { recursive: true }).sort()) {
  if (!fs.statSync(path.join(dist, file)).isFile()) continue;
  hash.update(file);
  hash.update(fs.readFileSync(path.join(dist, file)));
}
const manifest = {
  head: git('rev-parse', 'HEAD'),
  head_before: headBefore,
  git_status: git('status', '--porcelain=v1'),
  git_status_before: statusBefore,
  captured_at: new Date().toISOString(),
  scope: 'Stub-backed Health rendering and query-control assertions; not real ingest evidence',
  bundleSha256: hash.digest('hex'), captures, images,
};
fs.writeFileSync(path.join(output, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
const escapeHtml = value => String(value).replace(/[&<>"']/g, character => ({
  '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
})[character]);
const clean = !manifest.git_status && !manifest.git_status_before && manifest.head === manifest.head_before;
fs.writeFileSync(path.join(output, 'index.html'), `<!doctype html>
<html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Trawl Health diagnostics evidence</title>
<style>body{font:16px/1.5 system-ui,sans-serif;max-width:1500px;margin:32px auto;padding:0 20px;color:#18232b;background:#f5f7f8}h1,h2{line-height:1.2}code,pre{overflow-wrap:anywhere;white-space:pre-wrap}img{display:block;max-width:100%;height:auto;border:1px solid #bac5cc}section{margin:32px 0}a{color:#075b8a}</style>
<h1>Trawl Health diagnostics</h1>
<p>Wide and narrow browser captures with passing layout, diagnostic-fact, and query-control assertions. Overlapping viewport slices cover the complete Health scroller: original cards, Ingestion, Storage, and Queries. The viewport remains 1440 or 720 pixels wide and 1000 pixels tall.</p>
<p>${escapeHtml(manifest.scope)}.</p>
<p>Captured at <time>${escapeHtml(manifest.captured_at)}</time>. Checkout HEAD: <code>${escapeHtml(manifest.head)}</code>.</p>
<p>${clean ? 'The checkout was clean before and after capture.' : 'The checkout had changes or HEAD changed during capture. HEAD alone does not identify the rendered sources; inspect the recorded Git status.'} The bundle hash identifies the served assets.</p>
<details><summary>Capture manifest, Git status, coverage, and SHA-256 hashes</summary><pre>${escapeHtml(JSON.stringify(manifest, null, 2))}</pre></details>
${images.map(image => {
  const data = `data:image/png;base64,${fs.readFileSync(path.join(output, image.name)).toString('base64')}`;
  return `<section><h2>${image.width}px viewport, Health scroll offset ${image.scrollTop}px</h2><p>Coverage: ${image.scrollTop} to ${Math.min(image.scrollHeight, image.scrollTop + image.clientHeight)} of ${image.scrollHeight} content pixels. <a download="${image.name}" href="${data}">Download full-size PNG</a></p><img src="${data}" alt="Health at ${image.width}px viewport width, scroll offset ${image.scrollTop}px"></section>`;
}).join('\n')}
</html>\n`);
