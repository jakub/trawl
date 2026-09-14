// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The harness serves a private copy of the trunk dist, never the build
// directory itself.
//
// Two producers write `crates/trawl-web-ui/dist`: the gate's `trunk
// build`, and any `trunk serve` running from the same checkout (bin/dev's
// stack keeps one, and it rebuilds on every save). Reading the live
// directory therefore photographs a moving target, and the suite reads
// the dev server's accidents as the app's:
//
//   * a rebuild that lands mid-run replaces the hashed wasm index.html
//     names, so the next navigation fails with `TypeError: Failed to
//     execute 'compile' on 'WebAssembly': HTTP status code is not ok`;
//   * a serve build injects trunk's autoreload client, whose
//     `{{__TRUNK_ADDRESS__}}` placeholder only trunk's own HTTP server
//     substitutes. Served statically it opens a WebSocket to a literal
//     placeholder host, and the failure lands in `console.error`, where
//     every "the page logged nothing" assertion counts it as the SPA's.
//
// So: copy the tree once at startup (a reflink clone on btrfs, cheap
// enough to do per run), check the copy against itself, and drop the
// autoreload client if the source happened to be a serve build. What the
// suite then loads cannot change under it for the length of the run.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HARNESS_DIR = path.dirname(fileURLToPath(import.meta.url));

/** Where the snapshot for one server lives. Keyed by port so parallel
 * suites in one worktree (E2E_PORT) own separate copies. */
export function snapshotPath(port) {
  return path.join(HARNESS_DIR, '..', `.dist-snapshot-${port}`);
}

/** Local assets index.html names, as dist-relative paths. */
function referenced(html) {
  return [...html.matchAll(/(?:href|src)="\/([^"'?#]+)"/g)].map((m) => m[1]);
}

/** Trunk's autoreload client, identified by the placeholder only trunk's
 * own server can fill in. A `trunk build` index.html has no such script
 * and comes through untouched. */
function withoutAutoreload(html) {
  return html.replace(/<script\b[^>]*>[\s\S]*?<\/script>/g, (script) =>
    script.includes('__TRUNK_ADDRESS__') ? '' : script,
  );
}

/**
 * Copy `source` to this port's snapshot directory and return the path.
 *
 * A copy taken while trunk is applying a new distribution can be torn —
 * an index.html naming assets the copy does not have — so the copy is
 * checked and retaken. The check is also the only thing standing between
 * a half-written dist and a suite-wide cascade of 404s.
 */
export function snapshotDist(source, port, { attempts = 5, log = () => {} } = {}) {
  const target = snapshotPath(port);
  let missing = [];
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    fs.rmSync(target, { recursive: true, force: true });
    fs.cpSync(source, target, { recursive: true });
    const indexPath = path.join(target, 'index.html');
    const html = fs.readFileSync(indexPath, 'utf8');
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
