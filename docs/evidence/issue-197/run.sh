#!/usr/bin/env bash
# Issue #197 evidence: the Search page's histogram counts the whole result.
#
# Builds this checkout's trawld, trawl-web, fleet-admin and SPA, then starts
# a disposable stack: a private Postgres container, trawld and trawl-web on
# their own ports, and a data directory of their own. It ingests seed.ndjson
# (120 events 30 s apart, the 30 errors oldest), signs in with a read-only human key,
# and captures the Search page at page 1 and page 2 with Playwright's
# Chromium into page-1.png and page-2.png beside this script. Everything it
# started, the container and the temporary directory with the keys are
# removed on exit, whether the run passes or fails.
#
# Usage: docs/evidence/issue-197/run.sh
# Needs docker, trunk, and `npm ci` plus `npx playwright install chromium`
# in crates/trawl-web-ui/e2e.
set -euo pipefail
WT=$(git rev-parse --show-toplevel)
E=$(cd "$(dirname "$0")" && pwd)
TARGET=${CARGO_TARGET_DIR:-$WT/target}
PG_CONTAINER=trawl-197-evidence
PG_PORT=5473
HTTP_PORT=18497
WEB_PORT=18498
ADMIN=$TARGET/debug/fleet-admin
# Newest first, so page 1 holds the 50 newest events and none of the errors.
# Without a sort the row order is not fixed.
QUERY='service%3Dcheckout%20%7C%20sort%20-_time'
RANGE='2026-09-01T09:55:00Z..2026-09-01T11:05:00Z'

mkdir -p "$TARGET"
RUN=$(mktemp -d "$TARGET/evidence-197.XXXXXX")
chmod 700 "$RUN"

down() {
  for name in trawl-web trawld; do
    if [ -f "$RUN/$name.pid" ]; then
      kill "$(cat "$RUN/$name.pid")" 2>/dev/null || true
    fi
  done
  sleep 2
  docker rm -f $PG_CONTAINER >/dev/null 2>&1 || true
  rm -rf "$RUN"
  echo "stack down"
}
trap down EXIT

echo "-- build"
(cd "$WT" && cargo build --quiet -p trawl-server --bin trawld -p trawl-web --bin trawl-web -p fleet-admin --bin fleet-admin)
(cd "$WT/crates/trawl-web-ui" && env -u NO_COLOR trunk build --dist "$RUN/spa" >"$RUN/trunk.log" 2>&1) \
  || { tail -20 "$RUN/trunk.log"; exit 1; }

echo "-- stack"
docker rm -f $PG_CONTAINER >/dev/null 2>&1 || true
docker run -d --name $PG_CONTAINER -p "127.0.0.1:$PG_PORT:5432" -p "[::1]:$PG_PORT:5432" \
  -e POSTGRES_USER=evidence -e POSTGRES_PASSWORD=evidence -e POSTGRES_DB=fleet \
  --shm-size=1g --tmpfs /var/lib/postgresql:size=512m postgres:18 -c fsync=off >/dev/null
for _ in $(seq 1 60); do
  docker exec $PG_CONTAINER pg_isready -h 127.0.0.1 -U evidence -d fleet >/dev/null 2>&1 && break
  sleep 1
done
docker exec $PG_CONTAINER createdb -U evidence trawl
FLEET_DSN="postgres://evidence:evidence@localhost:$PG_PORT/fleet"
APP_DSN="postgres://evidence:evidence@localhost:$PG_PORT/trawl"
export DATABASE_URL=$FLEET_DSN
$ADMIN migrate >/dev/null
$ADMIN roles create --name reader --perm trawl:query --perm trawl:schema_read >/dev/null 2>&1
$ADMIN roles create --name sender --rate-rpm 100000 --perm trawl:ingest >/dev/null 2>&1
(
  umask 077
  $ADMIN keys create --name evidence-reader --kind human --role reader --expires 1h > "$RUN/reader-key" 2>/dev/null
  $ADMIN keys create --name evidence-sender --kind service --role sender --expires 1h > "$RUN/sender-key" 2>/dev/null
)
SESSION_KEY=$($ADMIN generate-session-key)
cat > "$RUN/trawld.toml" <<TOML
[server]
http_addr = "127.0.0.1:$HTTP_PORT"
max_result_rows = 25000
max_concurrent_queries = 2
shutdown_drain_secs = 3
link_origin = "http://127.0.0.1:$WEB_PORT"
[data]
path = "$RUN/data"
[ingest]
enabled = true
internal_telemetry = false
daily_rollup = false
compaction_interval_secs = 10
compaction_chunk_size = 500
default_env = "prod"
envs = ["prod"]
[retention]
max_age_days = 0
min_free_disk_bytes = 0
[scheduler]
enabled = false
[syslog]
enabled = false
TOML
FLEET_DATABASE_URL=$FLEET_DSN TRAWL_DATABASE_URL=$APP_DSN CARGO_TARGET_DIR=$TARGET \
  setsid "$WT/bin/trawld-dev" --config "$RUN/trawld.toml" > "$RUN/trawld.log" 2>&1 &
echo $! > "$RUN/trawld.pid"
for _ in $(seq 1 60); do grep -q "HTTPS server listening" "$RUN/trawld.log" 2>/dev/null && break; sleep 1; done
grep -q "HTTPS server listening" "$RUN/trawld.log" || { echo "trawld did not start"; tail -20 "$RUN/trawld.log"; exit 1; }
CERT=$RUN/tls/cert.pem
{ cat "$RUN/trawld.toml"; cat <<TOML
[web]
bind_addr = "127.0.0.1:$WEB_PORT"
upstream_url = "https://127.0.0.1:$HTTP_PORT"
upstream_ca_path = "$CERT"
allow_insecure_cookies = true
public_origins = ["http://127.0.0.1:$WEB_PORT"]
cookie_secret_env = "FLEET_SESSION_AEAD_KEY"
TOML
} > "$RUN/web.toml"
FLEET_SESSION_AEAD_KEY=$SESSION_KEY TRAWL_WEB_SPA_DIR=$RUN/spa \
  setsid "$TARGET/debug/trawl-web" --config "$RUN/web.toml" > "$RUN/trawl-web.log" 2>&1 &
echo $! > "$RUN/trawl-web.pid"
for _ in $(seq 1 60); do grep -q "trawl-web listening" "$RUN/trawl-web.log" 2>/dev/null && break; sleep 1; done
grep -q "trawl-web listening" "$RUN/trawl-web.log" || { echo "trawl-web did not start"; tail -20 "$RUN/trawl-web.log"; exit 1; }

echo "-- seed"
curl -sS --fail-with-body --cacert "$CERT" -H "authorization: Bearer $(cat "$RUN/sender-key")" \
  -H 'content-type: application/x-ndjson' --data-binary @"$E/seed.ndjson" \
  "https://127.0.0.1:$HTTP_PORT/api/v1/ingest"
echo

echo "-- capture"
# The key reaches the browser through a file name, never an argument or a
# log line. Playwright's debug log would print the fill() value, so it is off.
env -u DEBUG -u DEBUG_FILE -u PWDEBUG \
  ORIGIN="http://127.0.0.1:$WEB_PORT" KEY_FILE="$RUN/reader-key" OUT="$E" \
  SEARCH="/search?q=$QUERY&r=$RANGE" PLAYWRIGHT_FROM="$WT/crates/trawl-web-ui/e2e/package.json" \
  node --input-type=module <<'JS'
import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
import path from 'node:path';

const { ORIGIN, KEY_FILE, OUT, SEARCH, PLAYWRIGHT_FROM } = process.env;
const { chromium } = createRequire(PLAYWRIGHT_FROM)('@playwright/test');
const browser = await chromium.launch({ headless: true, channel: 'chromium' });
try {
  const page = await browser.newPage({ viewport: { width: 1400, height: 900 } });
  await page.goto(`${ORIGIN}/login`);
  try {
    await page.getByLabel('API key', { exact: true }).fill(readFileSync(KEY_FILE, 'utf8').trim());
    await page.getByRole('button', { name: 'Sign in', exact: true }).click();
    await page.waitForURL('**/search');
  } catch {
    throw new Error('the reader key did not sign in');
  }

  // Ingest is acknowledged before the rows are queryable, so ask again until
  // the whole seed is counted.
  const summary = page.locator('.results .results-footer .results-summary');
  const caption = page.locator('.histo .histo-scope');
  const want = 'All 120 events in this result';
  for (let attempt = 0; ; attempt++) {
    await page.goto(`${ORIGIN}${SEARCH}`);
    await caption.or(page.locator('.histo .histo-hint')).first().waitFor();
    if ((await caption.count()) && (await caption.textContent()).startsWith(want)) break;
    if (attempt === 30) throw new Error(`the strip never counted the seed: ${await page.locator('.histo').textContent()}`);
    await page.waitForTimeout(1000);
  }

  const read = async (n) => {
    await summary.filter({ hasText: `Page ${n} · showing 50 rows` }).waitFor();
    await page.locator('.histo .bar').nth(47).waitFor();
    const tips = await page.locator('.histo .bar .tip').allTextContents();
    const errors = tips
      .map(t => Number((t.match(/ · ([\d,]+) errors$/)?.[1] ?? '0').replaceAll(',', '')))
      .reduce((a, b) => a + b, 0);
    const errorRows = await page.locator('.results-table tbody .lvl-error').count();
    const text = await caption.textContent();
    await page.mouse.move(0, 0);
    await page.screenshot({ path: path.join(OUT, `page-${n}.png`) });
    console.log(`page ${n}: caption "${text}"; strip errors ${errors}; error rows on the page ${errorRows}`);
    return { text, tips, errors, errorRows };
  };

  const first = await read(1);
  await page.locator('.results .results-footer').getByRole('button', { name: 'Next' }).click();
  const second = await read(2);
  const same = first.text === second.text && JSON.stringify(first.tips) === JSON.stringify(second.tips);
  console.log(`same caption and bins on both pages: ${same}`);
  if (!same || first.errors !== 30 || first.errorRows !== 0) process.exitCode = 1;
} finally {
  await browser.close();
}
JS
echo "-- wrote $E/page-1.png and $E/page-2.png"
