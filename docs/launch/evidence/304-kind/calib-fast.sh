#!/usr/bin/env bash
# Supplementary calibration for AC1 phase C of trawl #304: which budget stops
# SPILL_BIG_QUERY. ADR-0056 gives memory exhaustion and spill exhaustion the
# same public answer, 503 query_resource_exhausted, and the head logs only the
# safe message, so the after transcript cannot name the budget. Its 2 s
# scratch sampler also misses the last seconds before a failure, when scratch
# grows by hundreds of MB a second. Here a loop inside the trawld container
# records the query scratch bytes and the cgroup's anon and total memory
# every 0.2 s while each query runs.
#
#   STOP_AFTER=ingest repro.sh IMAGE LABEL CHART_DIR   a throwaway namespace
#                                                      i304-LABEL: the AC1
#                                                      values and corpus
#   calib-fast.sh LABEL PAUSE NAME...                  run each named repro.sh
#                                                      query once, PAUSE s apart
#
# NAME is SPILL_QUERY or SPILL_BIG_QUERY. The queries, the spill budget and
# the block slack are read from repro.sh, so they match the after run. The
# script writes LABEL-fast-transcript.txt. It stops only what it started:
# its port-forward runs in a process group of its own, and each in-pod
# sampler ends at a stop file this script creates, or after 180 s.
set -euo pipefail
LABEL=${1:?usage: calib-fast.sh LABEL PAUSE NAME...}; PAUSE=${2:?PAUSE}; shift 2
(( $# > 0 )) || { echo "name at least one query: SPILL_QUERY or SPILL_BIG_QUERY" >&2; exit 2; }
HERE="$(cd "$(dirname "$0")" && pwd)"
KC=$HERE/kubeconfig
CTX=kind-trawl-i304
NS=i304-$LABEL
PRIV=$HERE/private/$LABEL
T=$HERE/$LABEL-fast-transcript.txt
W=$PRIV/work/fast
PORT=${PORT:-15514}
K=(kubectl --kubeconfig "$KC" --context "$CTX" -n "$NS")
eval "$(grep -E '^(K8|SPILL_QUERY|SPILL_BIG_QUERY|SPILL_BUDGET_BYTES|SPILL_BLOCK_SLACK_BYTES|SCRATCH)=' "$HERE/repro.sh")"
[[ -s $PRIV/curl.conf ]] || { echo "set up $NS first: STOP_AFTER=ingest repro.sh IMAGE $LABEL CHART_DIR" >&2; exit 1; }
mkdir -p "$W"
: > "$T"

ts() { date -u +%FT%T.%3NZ; }
say() { printf '\n## [%s] %s\n' "$(ts)" "$*" | tee -a "$T"; }
note() { printf '[%s] %s\n' "$(ts)" "$*" | tee -a "$T"; }
run() { { printf '[%s] $' "$(ts)"; printf ' %q' "$@"; printf '\n'; } | tee -a "$T"; "$@" 2>&1 | tee -a "$T"; }
runsh() { printf '[%s] $ %s\n' "$(ts)" "$1" | tee -a "$T"; bash -c "$1" 2>&1 | tee -a "$T"; }
api() { curl -sS --cacert "$PRIV/cert.pem" -K "$PRIV/curl.conf" "$@"; }
health() { curl -sS --cacert "$PRIV/cert.pem" --max-time 5 -o /dev/null -w '%{http_code}' "https://localhost:$PORT/api/v1/health" 2>/dev/null || true; }
events() {
  "${K[@]}" get events --sort-by=.lastTimestamp \
    -o custom-columns=FIRST:.firstTimestamp,LAST:.lastTimestamp,COUNT:.count,TYPE:.type,REASON:.reason,OBJECT:.involvedObject.name,MESSAGE:.message 2>&1
}

# The port-forward loop leads its own process group, as in repro.sh.
pf_pid=
pf_stop() {
  [[ -n $pf_pid ]] || return 0
  kill -TERM -- "-$pf_pid" 2>/dev/null || true
  wait "$pf_pid" 2>/dev/null || true
  pf_pid=
}
sampler_pid=
sampler_stop() {
  [[ -n $sampler_pid ]] || return 0
  "${K[@]}" exec trawl-0 -c trawld -- touch "$1" 2>/dev/null || kill "$sampler_pid" 2>/dev/null || true
  wait "$sampler_pid" 2>/dev/null || true
  sampler_pid=
}
stop=
trap 'sampler_stop "$stop"; pf_stop' EXIT

say "calib-fast $LABEL: namespace $NS, queries $*, ${PAUSE}s apart"
run "${K[@]}" get pod trawl-0 -o jsonpath='uid={.metadata.uid} image={.spec.containers[?(@.name=="trawld")].image} imageID={.status.containerStatuses[?(@.name=="trawld")].imageID} restarts={.status.containerStatuses[?(@.name=="trawld")].restartCount}{"\n"}'
runsh "kubectl --kubeconfig '$KC' --context $CTX -n $NS get statefulset trawl -o json | jq -c '.spec.template.spec.containers[] | select(.name==\"trawld\") | .resources'"
"${K[@]}" get configmap trawl -o jsonpath='{.data.trawld\.toml}' > "$W/trawld.toml"
runsh "grep -E '^query_spill_max_bytes|^query_memory_limit|^query_threads|^max_concurrent_queries' '$W/trawld.toml'"
run "${K[@]}" exec trawl-0 -c trawld -- sh -c 'echo memory.max $(cat /sys/fs/cgroup/memory.max) nproc $(nproc)'
note "spill budget $SPILL_BUDGET_BYTES bytes + block slack $SPILL_BLOCK_SLACK_BYTES = $((SPILL_BUDGET_BYTES + SPILL_BLOCK_SLACK_BYTES)); query_memory_limit 2GiB = $((2 << 30)) bytes"
for name in "$@"; do note "$name: ${!name}"; done

setsid bash -c 'log=$1; shift; while :; do "$@" >> "$log" 2>&1; sleep 0.5; done' pf \
  "$W/port-forward.log" "${K[@]}" port-forward pod/trawl-0 "$PORT:5514" &
pf_pid=$!
for _ in $(seq 1 50); do [[ $(health) != 000 ]] && break; sleep 0.2; done
note "GET /api/v1/health -> $(health)"

i=0
for name in "$@"; do
  i=$((i + 1))
  [[ $name == SPILL_QUERY || $name == SPILL_BIG_QUERY ]] || { note "unknown query $name"; exit 2; }
  say "run $i: $name, with scratch and memory sampled in the pod every 0.2 s"
  stop=/tmp/calib-fast-stop-$i
  # One shell loop inside the container: no kubectl exec per sample.
  "${K[@]}" exec trawl-0 -c trawld -- sh -c '
    end=$(( $(date +%s) + 180 ))
    while [ ! -e "$1" ] && [ "$(date +%s)" -lt "$end" ]; do
      echo "$(date -u +%H:%M:%S.%N | cut -c1-12) scratch=$(du -sb "$2" | cut -f1) anon=$(grep "^anon " /sys/fs/cgroup/memory.stat | cut -d" " -f2) current=$(cat /sys/fs/cgroup/memory.current)"
      sleep 0.2
    done' sh "$stop" "$SCRATCH" > "$W/fast$i.log" 2>&1 &
  sampler_pid=$!
  sleep 1
  jq -n --arg q "${!name}" '{query: $q}' > "$W/q$i.req"
  t0=$(ts)
  r=$(api --max-time 120 -o "$W/q$i.out" -w '%{http_code} %{time_total}' -H 'Content-Type: application/json' \
    --data @"$W/q$i.req" "https://localhost:$PORT/api/v1/query" 2>/dev/null || true)
  t1=$(ts)
  sleep 1
  sampler_stop "$stop"
  note "$name sent $t0, answered $t1 -> $r $(head -c 300 "$W/q$i.out" | tr -d '\n')"
  say "run $i: samples (UTC time, scratch bytes, cgroup anon bytes, cgroup memory.current bytes)"
  runsh "cat '$W/fast$i.log'"
  peak=$(grep -oE 'scratch=[0-9]+' "$W/fast$i.log" | cut -d= -f2 | sort -n | tail -n 1)
  peak=${peak:-0}
  note "run $i: $(wc -l < "$W/fast$i.log") samples; scratch peak $peak bytes at $(grep -m1 " scratch=$peak " "$W/fast$i.log" | cut -d' ' -f1); peak - budget = $((peak - SPILL_BUDGET_BYTES)) bytes; within budget + slack: $( (( peak <= SPILL_BUDGET_BYTES + SPILL_BLOCK_SLACK_BYTES )) && echo yes || echo no)"
  note "run $i: anon peak $(grep -oE 'anon=[0-9]+' "$W/fast$i.log" | cut -d= -f2 | sort -n | tail -n 1) bytes; memory.current peak $(grep -oE 'current=[0-9]+' "$W/fast$i.log" | cut -d= -f2 | sort -n | tail -n 1) bytes"
  if (( i < $# )); then sleep "$PAUSE"; fi
done

say "after the runs"
run "${K[@]}" get pod trawl-0 -o jsonpath='uid={.metadata.uid} phase={.status.phase} ready={.status.conditions[?(@.type=="Ready")].status} restarts={.status.containerStatuses[?(@.name=="trawld")].restartCount} lastState={.status.containerStatuses[?(@.name=="trawld")].lastState}{"\n"}'
run "${K[@]}" exec trawl-0 -c trawld -- cat /sys/fs/cgroup/memory.events
events | tee -a "$T" >/dev/null
say "done calib-fast $LABEL"
