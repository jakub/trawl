#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Issue #227, fix 1: a successful scheduled run whose stored result file is
# gone answers with one named unavailable state on every read surface.
#
# Stands up the disposable full-app stack (bin/app-experiment: its own
# Postgres container, keys, TLS, trawld and trawl-web), records two nonempty
# runs of one net and one zero-row run of a control net, moves the NEWER
# nonempty run's parquet file aside, and reads every surface that can be
# asked about it. Then moves the file back and reads the same run again.
#
# Its stdout IS visual-evidence/issue-227/unavailable-run.md. Regenerate with:
#
#   visual-evidence/issue-227/capture-unavailable-run.sh > \
#     visual-evidence/issue-227/unavailable-run.md
#
# The experiment runner needs Docker, Node, Trunk and the Playwright browser
# dependencies, and it builds this checkout; see scripts/app-experiment/README.md.
# Point CARGO_TARGET_DIR at a warm disk-backed cache to shorten that build.
# HOLD_SECONDS is how long
# the runner keeps the verified instance open — the capture happens inside
# that window, and the script waits for the runner's own deadline because an
# early stop is an `interrupted` run, not a passed one.
set -euo pipefail

root="$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)"
cd "$root"

HOLD_SECONDS="${HOLD_SECONDS:-120}"
EVENTS="${EVENTS:-60}"

work="$(mktemp -d)"
runner_log="$work/app-experiment.log"
runner_pid=""

cleanup() {
  local code=$?
  if [ -n "$runner_pid" ] && kill -0 "$runner_pid" 2>/dev/null; then
    # Only reached on failure: a clean pass has already waited for the
    # runner's own deadline. SIGTERM asks it to tear its own stack down.
    kill -TERM "$runner_pid" 2>/dev/null || true
    wait "$runner_pid" 2>/dev/null || true
  fi
  rm -rf "$work"
  exit "$code"
}
trap cleanup EXIT INT TERM

# --- transcript helpers ------------------------------------------------------

LAST_STATUS=""
LAST_BODY=""

# Print a request as the reader would type it, then its status and body.
# $KEY and $UP stay unexpanded on the printed line: the key is a credential
# and the port is this disposable instance's alone.
request() {
  local method="$1" path="$2" body="${3-}" out
  echo '```console'
  if [ -n "$body" ]; then
    printf '$ curl -sS -k -X %s "$UP%s" \\\n' "$method" "$path"
    printf '    -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \\\n'
    printf "    -d '%s'\\n" "$body"
    out="$(curl -sS -k -X "$method" "$UP$path" \
      -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
      -d "$body" -w $'\n%{http_code}')"
  else
    printf '$ curl -sS -k "$UP%s" -H "Authorization: Bearer $KEY"\n' "$path"
    out="$(curl -sS -k "$UP$path" \
      -H "Authorization: Bearer $KEY" -w $'\n%{http_code}')"
  fi
  LAST_STATUS="${out##*$'\n'}"
  LAST_BODY="${out%$'\n'*}"
  printf 'HTTP %s\n' "$LAST_STATUS"
  printf '%s\n' "$LAST_BODY" | jq .
  echo '```'
  echo
}

# A DSL read through POST /api/v1/query.
query() { request POST /api/v1/query "$(jq -nc --arg q "$1" '{query:$q}')"; }

# Fail loudly rather than transcribe a wrong answer.
expect_status() {
  if [ "$LAST_STATUS" != "$1" ]; then
    echo "EXPECTED HTTP $1, GOT $LAST_STATUS: $LAST_BODY" >&2
    exit 1
  fi
}

# Wait for a triggered run to finish, then echo its id.
await_run() {
  local net="$1" run="$2" deadline=$((SECONDS + 60)) status=""
  while [ "$SECONDS" -lt "$deadline" ]; do
    status="$(curl -sS -k "$UP/api/v1/saved/$net/runs/$run" \
      -H "Authorization: Bearer $KEY" | jq -r .status)"
    [ "$status" = running ] || break
    sleep 0.2
  done
  [ "$status" = success ] || { echo "run $run ended $status" >&2; exit 1; }
  echo "$run"
}

trigger() {
  local net="$1" run
  run="$(curl -sS -k -X POST "$UP/api/v1/saved/$net/run" \
    -H "Authorization: Bearer $KEY" | jq -r .id)"
  await_run "$net" "$run"
}

new_net() {
  curl -sS -k -X POST "$UP/api/v1/saved" \
    -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
    -d "$(jq -nc --arg n "$1" --arg q "$2" '{name:$n,query:$q}')" | jq -r .id
}

# --- stand the stack up ------------------------------------------------------

head_sha="$(git rev-parse HEAD)"
# The application source has to be the committed source, or the transcript
# describes a build nobody can check out. This script lives outside that set,
# so it can be written and run before its own commit.
if [ -n "$(git status --porcelain -- crates xtask bin Cargo.toml Cargo.lock rust-toolchain.toml)" ]; then
  echo "refusing to capture: the application source is not clean at $head_sha" >&2
  exit 1
fi

bin/app-experiment --seed 42 --events "$EVENTS" --batch-size 20 --rate 100 \
  --hold-seconds "$HOLD_SECONDS" >"$runner_log" 2>&1 &
runner_pid=$!

deadline=$((SECONDS + 900))
until grep -q 'Verified instance held' "$runner_log" 2>/dev/null; do
  if ! kill -0 "$runner_pid" 2>/dev/null; then
    echo "the experiment runner exited before its hold:" >&2
    tail -30 "$runner_log" >&2
    exit 1
  fi
  [ "$SECONDS" -lt "$deadline" ] || { echo "the runner never reached its hold" >&2; exit 1; }
  sleep 1
done

run_dir="$(sed -n 's/^Experiment .*\. Private artifacts: //p' "$runner_log" | head -1)"
run_id="$(jq -r .runId "$run_dir/instance.json")"
UP="$(jq -r .upstream "$run_dir/instance.json")"
KEY="$(tr -d '\n' <"$run_dir/private/browser-key")"
data_dir="$run_dir/private/data"

# --- record the runs ---------------------------------------------------------

bounds="experiment_run=\"$run_id\" earliest=\"2026-01-01T00:00:00Z\" latest=\"2026-01-02T00:00:00Z\""
report_net="$(new_net unavailable_demo "$bounds experiment_seq<3 | fields experiment_seq, status | sort experiment_seq")"
control_net="$(new_net zero_row_control "$bounds experiment_seq<0 | fields experiment_seq, status")"
for net in "$report_net" "$control_net"; do
  curl -sS -k -X PUT "$UP/api/v1/saved/$net/schedule" \
    -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
    -d '{"interval":"1h","enabled":true}' >/dev/null
done

older="$(trigger "$report_net")"
newer="$(trigger "$report_net")"
control_run="$(trigger "$control_net")"

newer_path="$(curl -sS -k "$UP/api/v1/saved/$report_net/runs/$newer" \
  -H "Authorization: Bearer $KEY" | jq -r .result_path)"
[ "$newer_path" != null ] || { echo "the newer run recorded no parquet path" >&2; exit 1; }
control_path="$(curl -sS -k "$UP/api/v1/saved/$control_net/runs/$control_run" \
  -H "Authorization: Bearer $KEY" | jq -r .result_path)"
[ "$control_path" = null ] || { echo "the zero-row control should have no path" >&2; exit 1; }

# --- the transcript ----------------------------------------------------------

cat <<MD
# A run whose stored result is gone — issue #227

Captured by \`visual-evidence/issue-227/capture-unavailable-run.sh\` against a
disposable full-app stack (\`bin/app-experiment\`: its own Postgres container,
keys, TLS, \`trawld\` and \`trawl-web\`). This file is that script's stdout.

- commit: \`$head_sha\`
- experiment run: \`$run_id\`
- \`\$UP\` is the disposable daemon's loopback HTTPS origin, \`\$KEY\` its
  browser key. Neither is printed: the key is a credential, and the instance
  is gone by the time you read this.

## The state under test

One net, \`unavailable_demo\`, with two successful runs of three rows each:

- run **$older**, the older
- run **$newer**, the newer — \`run=latest\` selects this one and never falls
  back to the one behind it (ADR-0018 ruling 13)

One control net, \`zero_row_control\`, whose run **$control_run** succeeded and
found nothing. A zero-row result has no schema to write a parquet from, so it
is recorded as a blob and has no file to lose. It is read at every step below,
unchanged, so "the file is gone" can be told apart from "the report is empty".

Every run's stored \`result_path\` is relative to the data root. The capture
moves run $newer's file aside — the same thing an operator does by repointing
\`data_dir\` after an epoch bump, at the scale of one file.

## Both runs read while the file is there

MD

request GET "/api/v1/saved/$report_net/runs/$newer"
expect_status 200
query "| from saved unavailable_demo run=latest | stats count()"
expect_status 200
query "| from saved unavailable_demo run=all | stats count()"
expect_status 200

echo '## The newer run'"'"'s stored result goes'
echo
echo '```console'
echo '$ mv "$DATA/'"$newer_path"'" "$ASIDE"'
echo '```'
echo

mv "$data_dir/$newer_path" "$work/held-aside.parquet"

cat <<MD
## Every read surface names run $newer

The direct run endpoint. Before this fix it answered 200 with \`result: null\`
beside a non-zero \`row_count\`, which the browser drawer rendered as "No
result data (error or still running)" and the TUI as "Run has no result data" —
both false for a run that succeeded.

MD

request GET "/api/v1/saved/$report_net/runs/$newer"
expect_status 409

echo "\`run=$newer\`, naming the run outright."
echo
query "| from saved unavailable_demo run=$newer | stats count()"
expect_status 409

cat <<MD
\`run=latest\`. The older run's three rows are still on disk, and the refusal
says in so many words that they were not substituted.

MD
query "| from saved unavailable_demo run=latest | stats count()"
expect_status 409

cat <<MD
\`run=all\`. One missing member fails the whole union, naming that member: not
a partial union of the survivors, and not zero rows.

MD
query "| from saved unavailable_demo run=all | stats count()"
expect_status 409

cat <<MD
The older run is unaffected — it is readable, which is what makes "no older run
was substituted" an observation rather than a hope.

MD
query "| from saved unavailable_demo run=$older | stats count()"
expect_status 200

cat <<MD
## The control is unchanged throughout

A genuine zero-row success still answers as an empty typed result, with its
column names, and still counts zero. Absence of a file and absence of rows stay
two different answers.

MD

request GET "/api/v1/saved/$control_net/runs/$control_run"
expect_status 200
query "| from saved zero_row_control run=latest | stats count()"
expect_status 200

echo '## The file comes back'
echo
echo '```console'
echo '$ mv "$ASIDE" "$DATA/'"$newer_path"'"'
echo '```'
echo

mv "$work/held-aside.parquet" "$data_dir/$newer_path"

cat <<MD
Nothing was persisted about the absence, so nothing has to be reconciled: the
next read stats the file and finds it.

MD

request GET "/api/v1/saved/$report_net/runs/$newer"
expect_status 200
query "| from saved unavailable_demo run=latest | table experiment_seq, status"
expect_status 200
query "| from saved unavailable_demo run=all | stats count()"
expect_status 200

# --- let the runner finish its own hold and tear the stack down --------------

wait "$runner_pid"
runner_pid=""

status="$(jq -r .status "$run_dir/report.json")"
cleanup_ok="$(jq -c .cleanup "$run_dir/report.json")"

cat <<MD
## The stack

The capture ran inside the experiment runner's hold. The runner's own scenario
— ingest, browser search, live tail, compaction, restart — passed before the
hold opened, and the runner tore its container, processes and secrets down
afterwards.

\`\`\`console
\$ jq -r '.status' "\$RUN/report.json"
$status
\$ jq -c '.cleanup' "\$RUN/report.json"
$cleanup_ok
\`\`\`
MD

[ "$status" = passed ] || { echo "the experiment did not pass" >&2; exit 1; }
