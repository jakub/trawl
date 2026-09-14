#!/usr/bin/env bash
set -euo pipefail
umask 077
worktree=$(pwd -P)
fixture_dir="$worktree/.tmp/final-tutorial"
package="$(cd "${1:?Pass the prepared distribution directory}" && pwd)"
test -x "$package/bin/trawl"
python3 -m json.tool "$package/distribution.json" > "$fixture_dir/distribution.json"
export PATH="$package/bin:$PATH"
unset LD_LIBRARY_PATH DYLD_LIBRARY_PATH DYLD_FALLBACK_LIBRARY_PATH TRAWL_WEB_SPA_DIR
"$package/bin/trawl" --version > "$fixture_dir/version.txt"
server_pid= web_pid= tutorial_cid= TRAWL_TUTORIAL_DIR=
cleanup() {
  code=$?
  trap - EXIT
  if [[ -n "$web_pid" ]]; then kill -TERM "$web_pid" 2>/dev/null || true; wait "$web_pid" 2>/dev/null || true; fi
  if [[ -n "$server_pid" ]]; then kill -TERM "$server_pid" 2>/dev/null || true; wait "$server_pid" 2>/dev/null || true; fi
  if [[ -n "$tutorial_cid" ]]; then docker rm --force --volumes "$tutorial_cid" >/dev/null; fi
  if [[ -n "$TRAWL_TUTORIAL_DIR" && "$TRAWL_TUTORIAL_DIR" == /tmp/trawl-tutorial.* ]]; then rm -r -- "$TRAWL_TUTORIAL_DIR"; fi
  printf 'Tutorial cleanup complete; test exit=%s\n' "$code"
  exit "$code"
}
trap cleanup EXIT
if docker container inspect trawl-docs-postgres >/dev/null 2>&1; then
  echo 'Refusing existing tutorial container' >&2; exit 1
fi
python3 - <<'PORTCHECK'
import socket
for port in (55439,15514,18090):
 with socket.socket() as s: s.bind(('127.0.0.1',port))
PORTCHECK
source "$fixture_dir/block-0.sh" > "$fixture_dir/start.log" 2>&1
tutorial_cid=$(docker inspect --format '{{.Id}}' trawl-docs-postgres)
run_block() { source "$fixture_dir/block-$1.sh" > "$TRAWL_TUTORIAL_DIR/block-$1.log" 2>&1; printf 'Executed tutorial block %s\n' "$1"; }
for block in 1 2 3 4; do run_block "$block"; done
env -u FLEET_DATABASE_URL -u TRAWL_DATABASE_URL -u TRAWL_HTTP_ADDR trawld --config "$TRAWL_TUTORIAL_DIR/trawld.toml" > "$TRAWL_TUTORIAL_DIR/daemon.log" 2>&1 &
server_pid=$!
for attempt in {1..60}; do
  if curl --fail --silent --cacert "$TRAWL_TUTORIAL_DIR/tls/cert.pem" https://localhost:15514/api/v1/health > "$TRAWL_TUTORIAL_DIR/health.json"; then break; fi
  kill -0 "$server_pid"; sleep 1
done
run_block 5
python3 - "$TRAWL_TUTORIAL_DIR/health.json" <<'HEALTH'
import json,sys
j=json.load(open(sys.argv[1]));assert j['status']=='ok',j
assert all(v=='ok' for v in j['checks'].values()),j
HEALTH
run_block 6
env -u FLEET_SESSION_AEAD_KEY -u FLEET_SESSION_PUBLIC_ORIGINS -u FLEET_SESSION_COOKIE_DOMAIN -u FLEET_SESSION_COOKIE_PATH -u FLEET_SESSION_COOKIE_SECURE -u TRAWL_WEB_BIND_ADDR TRAWL_WEB_INSECURE_UPSTREAM=1 trawl-web --config "$TRAWL_TUTORIAL_DIR/trawld.toml" > "$TRAWL_TUTORIAL_DIR/web.log" 2>&1 &
web_pid=$!
for attempt in {1..60}; do
  if curl --fail --silent http://localhost:18090/login > /dev/null; then break; fi
  kill -0 "$web_pid"; sleep 1
done
for block in 7 8 9 10; do run_block "$block"; done
python3 - "$TRAWL_TUTORIAL_DIR" <<'ORACLE'
import json,pathlib,sys
p=pathlib.Path(sys.argv[1])
assert json.loads((p/'block-8.log').read_text())=={'accepted':3}
assert json.loads((p/'block-9.log').read_text())=={'service':'tutorial','count':3}
assert json.loads((p/'block-10.log').read_text())=={'message':'connection refused','duration':1500}
print('Exact three-event ingest and both CLI query oracles passed')
ORACLE
node "$fixture_dir/browser.mjs" http://localhost:18090 "$TRAWL_TUTORIAL_DIR/reader.token" "$fixture_dir"
python3 "$fixture_dir/tui.py" "$package/bin/trawl" "$TRAWL_TUTORIAL_DIR/client.toml" "$fixture_dir"
run_block 12
trawl query --data "$TRAWL_TUTORIAL_DIR/tutorial.parquet" --format json '* | stats count() by service' > "$TRAWL_TUTORIAL_DIR/export-oracle.json"
python3 -c 'import json,sys; assert json.load(open(sys.argv[1])) == {"service":"tutorial","count":3}' "$TRAWL_TUTORIAL_DIR/export-oracle.json"
python3 - "$TRAWL_TUTORIAL_DIR/block-12.log" <<'EXPORT'
import json,pathlib,sys
s=pathlib.Path(sys.argv[1]).read_text()
assert json.loads(s)=={'service':'tutorial','count':3},s
print('Local Parquet query completed')
EXPORT
printf 'Packaged tutorial smoke passed, including the owned TUI session\n'
