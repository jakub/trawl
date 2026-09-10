// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

const ENTRIES = ['editor', 'toolbar'];
const ASSERTIONS = [
  'Save snapshot preview must equal the exact editor buffer',
  'Save snapshot POST must contain the exact editor buffer once',
];

/** Both controls must fail their snapshot assertion. A timeout is not a kill. */
export function saveSnapshotMutationKilled(report) {
  if (!Array.isArray(report?.suites)) return false;
  const specs = [];
  function walk(suite) {
    if (Array.isArray(suite?.specs)) specs.push(...suite.specs);
    if (Array.isArray(suite?.suites)) suite.suites.forEach(walk);
  }
  report.suites.forEach(walk);
  return ENTRIES.every((entry) => {
    const prefix = `Save captures editor buffer: ${entry} `;
    const matches = specs.filter((spec) => typeof spec?.title === 'string' && spec.title.startsWith(prefix));
    if (matches.length !== 1) return false;
    const tests = matches[0].tests;
    return Array.isArray(tests) && tests.some((test) =>
      Array.isArray(test?.results) && test.results.some((result) =>
        result?.status === 'failed' && Array.isArray(result.errors) &&
        result.errors.some((error) => typeof error?.message === 'string' &&
          ASSERTIONS.some((assertion) => error.message.includes(assertion))),
      ),
    );
  });
}

if (process.argv[1] && pathToFileURL(process.argv[1]).href === import.meta.url) {
  const report = JSON.parse(readFileSync(process.argv[2], 'utf8'));
  if (saveSnapshotMutationKilled(report)) {
    console.log('Save mutation: editor and toolbar both failed the exact snapshot assertion.');
  } else {
    console.error('Save mutation: both named entry tests must fail a snapshot assertion; report rejected.');
    process.exitCode = 1;
  }
}
