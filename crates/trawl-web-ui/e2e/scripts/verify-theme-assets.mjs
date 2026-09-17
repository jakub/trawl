// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Run after both Trunk builds. The two extra builds contain only JavaScript
// assets and prove content changes alter the URL through the real pipeline.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync, mkdirSync, mkdtempSync, rmSync, copyFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const [trawlDir, workbenchDir, outputDir] = process.argv.slice(2).map(value => path.resolve(value));
if (!trawlDir || !workbenchDir || !outputDir) throw new Error('usage: node verify-theme-assets.mjs TRAWL_DIST WORKBENCH_DIST OUTPUT_DIR');
mkdirSync(outputDir, { recursive: true });
const decode = value => value.replace(/&#x([\da-f]+);|&#(\d+);|&amp;/gi, (match, hex, decimal) =>
  match === '&amp;' ? '&' : String.fromCodePoint(Number.parseInt(hex ?? decimal, hex ? 16 : 10)));

function inspect(dist, key, label, app = true) {
  const source = readFileSync(path.join(dist, 'index.html'), 'utf8');
  const html = decode(source);
  const scripts = [...html.matchAll(/<script\b[^>]*>/g)].filter(([tag]) => tag.includes('theme-bootstrap-'));
  assert.equal(scripts.length, 1, `${label}: exactly one bootstrap`);
  const tag = scripts[0][0];
  assert(!/\s(?:async|defer)(?:=|\s|>)/.test(tag), `${label}: blocking script`);
  assert([undefined, '', 'text/javascript'].includes(tag.match(/\btype="([^"]*)"/)?.[1]), `${label}: classic script`);
  assert(tag.includes(`data-storage-key="${key}"`), `${label}: namespace`);
  const src = tag.match(/\bsrc="([^"]+)"/)?.[1];
  assert(src && /^\/?theme-bootstrap-[a-f0-9]{8,}\.js$/.test(src), `${label}: immutable asset filename`);
  const laterAssets = [...html.matchAll(/<(?:script|link)\b[^>]*(?:type="module"|rel="stylesheet"|rel="modulepreload")[^>]*>/g)];
  if (app) {
    assert(laterAssets.some(([tag]) => tag.includes('rel="stylesheet"')), `${label}: stylesheet present`);
    assert(laterAssets.some(([tag]) => tag.includes('type="module"')), `${label}: Wasm loader present`);
  }
  for (const node of laterAssets) {
    assert(scripts[0].index < node.index, `${label}: bootstrap precedes styles and Wasm`);
  }
  const bytes = readFileSync(path.join(dist, src.replace(/^\//, '')));
  writeFileSync(path.join(outputDir, `${label}-index.html`), source);
  writeFileSync(path.join(outputDir, `${label}-bootstrap.js`), bytes);
  return { src, key, sha256: createHash('sha256').update(bytes).digest('hex') };
}

const trawl = inspect(trawlDir, 'trawl.ui', 'trawl');
const workbench = inspect(workbenchDir, 'fleet-ui-demo:prefs', 'workbench');
const scratch = mkdtempSync(path.join(tmpdir(), 'trawl-theme-hash-'));
const environment = { ...process.env };
delete environment.NO_COLOR;
try {
  const bootstrap = fileURLToPath(new URL('../../../fleet-ui/js/theme-bootstrap.js', import.meta.url));
  copyFileSync(bootstrap, path.join(scratch, 'theme-bootstrap.js'));
  writeFileSync(path.join(scratch, 'Trunk.toml'), '[build]\ntarget = "index.html"\n');
  writeFileSync(path.join(scratch, 'index.html'), '<!doctype html><html data-theme="light"><head><script data-trunk src="theme-bootstrap.js" data-storage-key="trawl.ui"></script></head><body></body></html>');
  const build = label => {
    const dist = path.join(scratch, label);
    const log = execFileSync('trunk', ['build', '--release', '--dist', dist, 'index.html'], { cwd: scratch, env: environment, encoding: 'utf8' });
    writeFileSync(path.join(outputDir, `${label}-build.log`), log);
    return inspect(dist, 'trawl.ui', label, false);
  };
  const original = build('hash-original');
  // Modify only a temporary pipeline input; the candidate source stays intact.
  writeFileSync(path.join(scratch, 'theme-bootstrap.js'), readFileSync(bootstrap, 'utf8') + '\ndocument.documentElement.dataset.hashEvidence = "changed";\n');
  const changed = build('hash-changed');
  assert.notEqual(original.src, changed.src, 'changed bootstrap content changes its Trunk URL');
  assert.notEqual(original.sha256, changed.sha256, 'changed emitted bytes');
  const sourceSha256 = createHash('sha256').update(readFileSync(bootstrap)).digest('hex');
  const revision = execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  writeFileSync(path.join(outputDir, 'theme-assets.json'), JSON.stringify({ revision, sourceSha256, trawl, workbench, original, changed }, null, 2));
} finally { rmSync(scratch, { recursive: true, force: true }); }
