// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { readFileSync } from 'node:fs';
import path from 'node:path';

export interface ThemeFixture {
  id: string;
  raw: string | null;
  preference: 'light' | 'dark' | 'system';
  media: 'light' | 'dark' | 'unavailable' | 'throws';
  resolved: 'light' | 'dark';
}

const rows: unknown = JSON.parse(readFileSync(path.resolve(__dirname, '../../fleet-ui/src/theme/preference-fixtures.json'), 'utf8'));
if (!Array.isArray(rows) || rows.length === 0) throw new Error('Empty or invalid theme fixture table');
const ids = new Set<string>();
for (const row of rows) {
  if (!row || typeof row !== 'object' || Array.isArray(row)
      || Object.keys(row).sort().join(',') !== 'id,media,preference,raw,resolved'
      || typeof row.id !== 'string' || !row.id || ids.has(row.id)
      || !(row.raw === null || typeof row.raw === 'string')
      || !['light', 'dark', 'system'].includes(row.preference)
      || !['light', 'dark', 'unavailable', 'throws'].includes(row.media)
      || !['light', 'dark'].includes(row.resolved)) {
    throw new Error(`Invalid theme fixture: ${JSON.stringify(row)}`);
  }
  ids.add(row.id);
}
export const themeFixtures: ThemeFixture[] = rows;
