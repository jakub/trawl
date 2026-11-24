// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The harness serves a private copy of the trunk dist, never the build
// directory itself. Any `trunk serve` running from the same checkout
// rebuilds it on every save:
//
//   * a rebuild that lands mid-run replaces the hashed wasm index.html
//     names, so the next navigation fails;
//   * a serve build injects trunk's autoreload client, whose
//     `{{__TRUNK_ADDRESS__}}` placeholder only trunk's own HTTP server
//     substitutes. Served statically it opens a WebSocket to a literal
//     placeholder host, and the failure lands in `console.error`.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HARNESS_DIR = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(HARNESS_DIR, '..', '..', '..', '..');

/** Under the repository root, never inside crates/trawl-web-ui. `trunk
 * serve` watches that crate and rebuilds on any write below it, and a
 * rebuild applying its distribution clears `dist/.stage` under a
 * concurrent `trunk build`. */
export function snapshotPath(port) {
  return path.join(REPO_ROOT, 'e2e-artifacts', `dist-snapshot-${port}`);
}

function referenced(html) {
  return [...html.matchAll(/(?:href|src)="\/([^"'?#]+)"/g)].map((m) => m[1]);
}

function withoutAutoreload(html) {
  return html.replace(/<script\b[^>]*>[\s\S]*?<\/script>/g, (script) =>
    script.includes('__TRUNK_ADDRESS__') ? '' : script,
  );
}

/**
 * A copy taken while trunk is applying a new distribution can be torn —
 * an index.html naming assets the copy does not have — so the copy is
 * checked and retaken.
 */
export function snapshotDist(source, port, { attempts = 5, log = () => {} } = {}) {
  const target = snapshotPath(port);
  let missing = [];
  fs.mkdirSync(path.dirname(target), { recursive: true });
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    const indexPath = path.join(target, 'index.html');
    let html;
    try {
      fs.rmSync(target, { recursive: true, force: true });
      fs.cpSync(source, target, { recursive: true });
      html = fs.readFileSync(indexPath, 'utf8');
    } catch (error) {
      missing = [`(copy failed: ${error.code ?? error.message})`];
      log(`e2e harness: dist snapshot attempt ${attempt} caught a rebuild in flight (${error.code ?? error.message}); retaking`);
      continue;
    }
    missing = referenced(html).filter((rel) => !fs.existsSync(path.join(target, rel)));
    if (missing.length === 0) {
      const served = withoutAutoreload(html);
      if (served !== html) {
        fs.writeFileSync(indexPath, served);
        log('e2e harness: dropped trunk\'s autoreload client from the snapshot (the source dist is a `trunk serve` build)');
      }
      return target;
    }
    log(`e2e harness: dist snapshot attempt ${attempt} caught a rebuild in flight (missing ${missing.join(', ')}); retaking`);
  }
  fs.rmSync(target, { recursive: true, force: true });
  throw new Error(
    `e2e harness: could not take a consistent snapshot of ${source} in ${attempts} attempts ` +
      `(index.html still names ${missing.join(', ')}). Is a \`trunk serve\` rebuilding it in a loop?`,
  );
}
