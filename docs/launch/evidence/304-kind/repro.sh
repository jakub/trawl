#!/usr/bin/env bash
# Reproduce the two failures of the 2026-10-17 homelab incident (trawl #304)
# on the local kind cluster trawl-i304, or show that a build no longer has
# them.
#
#   repro.sh IMAGE LABEL CHART_DIR
#
#   IMAGE      a trawld image already loaded into the kind cluster
#              (build-image.sh prints it), e.g. trawl-i304:before-b02b8acfb8a2
#   LABEL      before | after; names the namespace i304-LABEL, the private
#              directory private/LABEL and the transcript LABEL-transcript.txt
#   CHART_DIR  the Helm chart from the same commit as IMAGE
#
# One run, end to end, into a fresh namespace:
#   0. the cluster's kubelet refreshes emptyDir usage every 2 s (idempotent;
#      see the step for why);
#   1. a disposable PostgreSQL with the fleet and trawl databases, the two
#      DSN Secrets, and a Helm install of CHART_DIR with values.yaml plus
#      values-load.yaml (a roomy pod for loading);
#   2. a key with query, ingest and server_manage, EVENTS synthetic events
#      (gen-events.py, fixed seed) ingested and compacted, then a helm
#      upgrade to values.yaml alone (the scenario limits);
#   3. starvation: STARVE_WORKERS clients send STARVE_QUERY back to back for
#      STARVE_SECS against a pool of 2 permits, while the transcript records
#      /api/v1/health every second, the pool counters, the pod's Ready
#      condition, trawld's log and the kubelet's events;
#   4. eviction: SPILL_QUERY runs up to SPILL_ATTEMPTS times, SPILL_PAUSE
#      seconds apart, while the transcript records the size of trawld's /tmp
#      emptyDir every 2 s, the responses, the pod and the events, and stops
#      at an eviction.
#
# STOP_AFTER=deploy|ingest ends a run early; it was used for calibration
# only and is never set for a recorded run.
#
# Expected: at b02b8acf, health answers 503 with query_not_started kind=ping
# and the pod goes NotReady in (3), and the kubelet evicts the pod for the
# tmp emptyDir in (4). With the fix, the pod stays Ready in (3), and in (4)
# the spill goes to query scratch on the data volume with nothing evicted:
# an attempt that passes the query budgets gets 503 query_resource_exhausted,
# and one that fits them answers 200.
#
# Secrets: the PostgreSQL passwords, the API key and the curl config holding
# its bearer header are generated into private/LABEL (0700, files 0600) and
# never printed or put on a command line.
set -euo pipefail
IMAGE=$1
LABEL=$2
CHART=$(cd "$3" && pwd)
HERE="$(cd "$(dirname "$0")" && pwd)"
KC=$HERE/kubeconfig
CTX=kind-trawl-i304
NS=i304-$LABEL
PRIV=$HERE/private/$LABEL
T=$HERE/$LABEL-transcript.txt
W=$PRIV/work                        # per-phase raw samples, summarized into T
PORT=${PORT:-15514}
K=(kubectl --kubeconfig "$KC" --context "$CTX" -n "$NS")
H=(helm --kubeconfig "$KC" --kube-context "$CTX" -n "$NS")

# The workload. Identical across runs; change it only together with NOTES.md.
EVENTS=800000
MSG_CHARS=200
SEED=304
STARVE_WORKERS=4
STARVE_SECS=60
STARVE_QUERY='service=i304 | stats count() by message, _raw | stats count()'
SPILL_ATTEMPTS=8
# trawld keeps a spill query's memory for about 12 s before returning it;
# a second attempt inside that window gets the before build OOM-killed at
# 4Gi rather than spilling, so attempts are spaced.
SPILL_PAUSE=20
# k is 32 copies of the event's text, about 8 KB a row: the dedup window
# sorts 800k such rows, about 6.4 GB, through DuckDB's external sort, which
# spills what passes the memory limit. (A hash aggregate over a key this wide
# gets the process OOM-killed instead of spilling, so the query sorts.)
K8='_raw, message, _raw, message, _raw, message, _raw, message'
SPILL_QUERY="service=i304 | let k = concat($K8, $K8, $K8, $K8) | dedup k | stats count()"
NODE=trawl-i304-control-plane
VOLUME_STATS_PERIOD=2s

rm -rf "$PRIV"
mkdir -p "$W"
chmod 700 "$HERE/private" "$PRIV"
: > "$T"

ts() { date -u +%FT%T.%3NZ; }
say() { printf '\n## [%s] %s\n' "$(ts)" "$*" | tee -a "$T"; }
note() { printf '[%s] %s\n' "$(ts)" "$*" | tee -a "$T"; }
# Echo the literal command, then run it; stdout and stderr land in T.
run() { { printf '[%s] $' "$(ts)"; printf ' %q' "$@"; printf '\n'; } | tee -a "$T"; "$@" 2>&1 | tee -a "$T"; }
runsh() { printf '[%s] $ %s\n' "$(ts)" "$1" | tee -a "$T"; bash -c "$1" 2>&1 | tee -a "$T"; }
nocolor() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }

# trawld API through the port-forward, with the bearer header from the curl
# config and trawld's own certificate pinned (its SAN includes localhost).
api() { curl -sS --cacert "$PRIV/cert.pem" -K "$PRIV/curl.conf" "$@"; }
health() { curl -sS --cacert "$PRIV/cert.pem" --max-time 5 -o "$1" -w '%{http_code}' "https://localhost:$PORT/api/v1/health" 2>/dev/null || true; }
pod_line() {
  "${K[@]}" get pod trawl-0 -o jsonpath='uid={.metadata.uid} phase={.status.phase} reason={.status.reason} ready={.status.conditions[?(@.type=="Ready")].status} restarts={.status.containerStatuses[?(@.name=="trawld")].restartCount}{"\n"}' 2>&1 | head -n 1
}
events() {
  "${K[@]}" get events --sort-by=.lastTimestamp \
    -o custom-columns=FIRST:.firstTimestamp,LAST:.lastTimestamp,COUNT:.count,TYPE:.type,REASON:.reason,OBJECT:.involvedObject.name,MESSAGE:.message 2>&1
}

# A supervised port-forward to the pod, not the Service: the Service drops a
# NotReady pod from its endpoints, and the transcript needs the 503 health
# answers a NotReady pod gives. kubectl port-forward exits when trawld hangs up
# mid-upload or the pod goes away, so a loop restarts it.
pf_pid=
pf_start() {
  pf_stop
  ( while :; do "${K[@]}" port-forward pod/trawl-0 "$PORT:5514" >> "$W/port-forward.log" 2>&1; sleep 0.5; done ) &
  pf_pid=$!
  for _ in $(seq 1 50); do
    [[ $(health /dev/null) != 000 ]] && return 0
    sleep 0.2
  done
  note "port-forward did not answer"
}
pf_stop() {
  [[ -n $pf_pid ]] || return 0
  pkill -P "$pf_pid" 2>/dev/null || true
  kill "$pf_pid" 2>/dev/null || true
  wait "$pf_pid" 2>/dev/null || true
  pkill -f "port-forward pod/trawl-0 $PORT:5514" 2>/dev/null || true
  pf_pid=
}
bg_pids=()
bg_stop() {  # each sampler is a subshell; stop its children (kubectl logs -f, sleep) too
  for p in "${bg_pids[@]}"; do pkill -P "$p" 2>/dev/null || true; kill "$p" 2>/dev/null || true; done
  for p in "${bg_pids[@]}"; do wait "$p" 2>/dev/null || true; done
  bg_pids=()
}
trap 'bg_stop; pf_stop' EXIT

wait_ready() {  # wait_ready SECONDS: until trawl-0 is Ready, sampling every 3 s
  local start=$SECONDS line
  while (( SECONDS - start < $1 )); do
    line=$(pod_line)
    note "pod trawl-0 $line"
    [[ $line == *"ready=True"* ]] && return 0
    sleep 3
  done
  return 1
}

# Background samplers. Each writes timestamped lines to a file in W.
sample_health() {   # every second: the HTTP status and body of /api/v1/health
  local out=$1 code
  while :; do
    code=$(health "$W/health.body")
    printf '%s %s %s\n' "$(ts)" "$code" "$(tr -d '\n' < "$W/health.body" 2>/dev/null)" >> "$out"
    : > "$W/health.body"
    sleep 1
  done
}
sample_pod() { while :; do printf '%s %s\n' "$(ts)" "$(pod_line)" >> "$1"; sleep 2; done; }
sample_pool() {     # every 5 s: pool counters and the held work
  while :; do
    printf '%s stats=%s queries=%s\n' "$(ts)" \
      "$(api --max-time 5 "https://localhost:$PORT/api/v1/stats" 2>&1 | jq -c '{pool_capacity, pool_available, active_queries, pool_retained}' 2>/dev/null || echo unavailable)" \
      "$(api --max-time 5 "https://localhost:$PORT/api/v1/queries" 2>&1 | jq -c '{active: [.active[] | {id, running_ms}], retained: [.retained[] | {id, kind, started, retained_ms}]}' 2>/dev/null || echo unavailable)" >> "$1"
    sleep 5
  done
}
sample_tmp() {      # every 2 s: bytes in trawld's /tmp emptyDir
  while :; do
    printf '%s %s\n' "$(ts)" "$("${K[@]}" exec trawl-0 -c trawld -- du -sb /tmp 2>&1 | head -n 1)" >> "$1"
    sleep 2
  done
}
# After the fix, trawld spills into query scratch on the data volume, not /tmp.
# The before build has no such directory; the sampler runs only where it exists.
SCRATCH=/var/lib/trawl/data.query-scratch
sample_scratch() {  # every 2 s: bytes in trawld's query scratch directory
  while :; do
    printf '%s %s\n' "$(ts)" "$("${K[@]}" exec trawl-0 -c trawld -- du -sb "$SCRATCH" 2>&1 | head -n 1)" >> "$1"
    sleep 2
  done
}
follow_logs() { "${K[@]}" logs -f trawl-0 -c trawld --timestamps --since=1s 2>&1 | nocolor >> "$1"; }

query() {  # query OUT_BODY QUERY [MAX_TIME]: prints "code seconds"
  jq -n --arg q "$2" '{query: $q}' > "$1.req"
  api --max-time "${3:-120}" -o "$1" -w '%{http_code} %{time_total}' -H 'Content-Type: application/json' \
    --data @"$1.req" "https://localhost:$PORT/api/v1/query" 2>/dev/null || true
}

say "run $LABEL: image $IMAGE, chart $CHART, namespace $NS"
run kind version
run helm version --short
run kubectl --kubeconfig "$KC" --context "$CTX" version
run kubectl --kubeconfig "$KC" --context "$CTX" get nodes -o wide
note "workload: EVENTS=$EVENTS MSG_CHARS=$MSG_CHARS SEED=$SEED STARVE_WORKERS=$STARVE_WORKERS STARVE_SECS=$STARVE_SECS SPILL_ATTEMPTS=$SPILL_ATTEMPTS"
note "STARVE_QUERY: $STARVE_QUERY"
note "SPILL_QUERY: $SPILL_QUERY"
runsh "docker image inspect --format '{{.Id}} {{index .Config.Labels \"org.opencontainers.image.revision\"}}' '$IMAGE'"

say "cluster: the kubelet measures emptyDir usage every $VOLUME_STATS_PERIOD"
# The kubelet evicts a pod whose emptyDir passes its sizeLimit, judging usage
# from a cached du that it refreshes every volumeStatsAggPeriod (default 1m,
# jittered up to 2m), and checks every 10 s. On kind's 16 fast cores the spill
# query's /tmp stays past 512Mi for seconds, not the incident's 36 s, so a 1m
# refresh almost never lands inside it. Idempotent: the kubelet restarts only
# when the setting changes.
if ! docker exec "$NODE" grep -qx "volumeStatsAggPeriod: $VOLUME_STATS_PERIOD" /var/lib/kubelet/config.yaml; then
  run docker exec "$NODE" sed -i "s/^volumeStatsAggPeriod: .*/volumeStatsAggPeriod: $VOLUME_STATS_PERIOD/" /var/lib/kubelet/config.yaml
  run docker exec "$NODE" systemctl restart kubelet
  run kubectl --kubeconfig "$KC" --context "$CTX" wait node "$NODE" --for=condition=Ready --timeout=120s
fi
run docker exec "$NODE" grep -E '^(volumeStatsAggPeriod|evictionHard|  nodefs|  imagefs)' /var/lib/kubelet/config.yaml

say "namespace and disposable PostgreSQL"
if "${K[@]}" get namespace "$NS" >/dev/null 2>&1; then
  run kubectl --kubeconfig "$KC" --context "$CTX" delete namespace "$NS" --wait=true --timeout=180s
fi
run kubectl --kubeconfig "$KC" --context "$CTX" create namespace "$NS"
( umask 077
  su=$(openssl rand -hex 24); fp=$(openssl rand -hex 24); tp=$(openssl rand -hex 24)
  printf "CREATE ROLE fleet LOGIN PASSWORD '%s';\nCREATE ROLE trawl LOGIN PASSWORD '%s';\nCREATE DATABASE fleet OWNER fleet;\nCREATE DATABASE trawl OWNER trawl;\n" "$fp" "$tp" > "$PRIV/init.sql"
  # The short Service name: the node's resolv.conf search list (ndots:5 plus
  # tailnet domains) makes an unqualified-looking FQDN take 10 s to resolve.
  printf '%s' "$su" > "$PRIV/superuser-password"
  printf 'postgres://fleet:%s@postgres:5432/fleet' "$fp" > "$PRIV/fleet-url"
  printf 'postgres://trawl:%s@postgres:5432/trawl' "$tp" > "$PRIV/trawl-url" )
note "\$ kubectl create secret generic pg-init|fleet-db|trawl-db --from-file=... (values from $PRIV, not printed)"
"${K[@]}" create secret generic pg-init --from-file=superuser-password="$PRIV/superuser-password" --from-file=init.sql="$PRIV/init.sql" 2>&1 | tee -a "$T"
"${K[@]}" create secret generic fleet-db --from-file=DATABASE_URL="$PRIV/fleet-url" 2>&1 | tee -a "$T"
"${K[@]}" create secret generic trawl-db --from-file=TRAWL_DATABASE_URL="$PRIV/trawl-url" 2>&1 | tee -a "$T"
run "${K[@]}" apply -f "$HERE/postgres.yaml"
run "${K[@]}" rollout status deployment/postgres --timeout=180s

IMG_SET=(--set-string "image.repository=${IMAGE%:*}" --set-string "image.tag=${IMAGE##*:}")
show_spec() {
  "${K[@]}" get configmap trawl -o jsonpath='{.data.trawld\.toml}' > "$W/trawld.toml"
  runsh "sed -n '/^\[server\]/,/^\[server.rate_limit\]/p' '$W/trawld.toml'"
  runsh "kubectl --kubeconfig '$KC' --context $CTX -n $NS get statefulset trawl -o json | jq -c '.spec.template.spec | {trawld_resources: (.containers[] | select(.name==\"trawld\") | .resources), tmp: (.volumes[] | select(.name==\"tmp\")), probes: (.containers[] | select(.name==\"trawld\") | {readiness: .readinessProbe | {periodSeconds, failureThreshold, timeoutSeconds}, liveness: .livenessProbe | {periodSeconds, failureThreshold}})}'"
}

say "helm install with the load values (values.yaml + values-load.yaml)"
run "${H[@]}" install trawl "$CHART" -f "$HERE/values.yaml" -f "$HERE/values-load.yaml" "${IMG_SET[@]}"
show_spec
wait_ready 300 || { note "trawl-0 never became Ready"; run "${K[@]}" describe pod trawl-0; exit 1; }
run "${K[@]}" get pods -o wide

say "API key (query, ingest, server_manage) and trawld's certificate"
run "${K[@]}" exec trawl-0 -c trawld -- sh -c 'DATABASE_URL="$FLEET_DATABASE_URL" exec fleet-admin roles create --name i304 --perm trawl:query --perm trawl:ingest --perm trawl:server_manage'
note "\$ kubectl exec trawl-0 -c trawld -- fleet-admin keys create --name i304 --kind human --role i304 > \$PRIV/token"
( umask 077
  "${K[@]}" exec trawl-0 -c trawld -- sh -c 'DATABASE_URL="$FLEET_DATABASE_URL" exec fleet-admin keys create --name i304 --kind human --role i304' > "$PRIV/token" 2> "$PRIV/keys-create.stderr"
  printf 'Authorization: Bearer %s\n' "$(tr -d '\n' < "$PRIV/token")" > "$PRIV/auth-header"
  printf 'header = "@%s"\n' "$PRIV/auth-header" > "$PRIV/curl.conf" )
tee -a "$T" < "$PRIV/keys-create.stderr"
note "token bytes: $(wc -c < "$PRIV/token")"
"${K[@]}" exec trawl-0 -c trawld -- cat /var/lib/trawl/tls/cert.pem > "$PRIV/cert.pem"
runsh "openssl x509 -in '$PRIV/cert.pem' -noout -subject -ext subjectAltName"
pf_start
code=$(health "$W/h0.json"); note "GET /api/v1/health -> $code $(cat "$W/h0.json")"
runsh "curl -sS --cacert '$PRIV/cert.pem' -K '$PRIV/curl.conf' https://localhost:$PORT/api/v1/whoami | jq -c ."

[[ ${STOP_AFTER:-} == deploy ]] && { note "stopping after deploy (STOP_AFTER=deploy, calibration only)"; exit 0; }

say "ingest $EVENTS events of $MSG_CHARS-character random messages"
note "\$ python3 -I gen-events.py https://localhost:$PORT/api/v1/ingest \$PRIV/cert.pem \$PRIV/auth-header $EVENTS $MSG_CHARS $SEED 4000"
python3 -I "$HERE/gen-events.py" "https://localhost:$PORT/api/v1/ingest" "$PRIV/cert.pem" "$PRIV/auth-header" "$EVENTS" "$MSG_CHARS" "$SEED" 4000 2>&1 | tee -a "$T"
note "waiting for the hot buffer to drain into Parquet (trawld's own telemetry keeps a few hundred events there)"
for _ in $(seq 1 150); do
  hb=$(api --max-time 5 "https://localhost:$PORT/metrics" 2>/dev/null | awk '$1=="trawl_hot_buffer_events"{print $2}')
  [[ -n $hb ]] && (( ${hb%.*} < 2000 )) && break
  sleep 2
done
note "trawl_hot_buffer_events=$hb"
code=$(query "$W/count.json" 'service=i304 | stats count()'); note "count query -> $code $(jq -c '.rows // .' "$W/count.json")"
runsh "kubectl --kubeconfig '$KC' --context $CTX -n $NS exec trawl-0 -c trawld -- du -sh /var/lib/trawl/data /tmp"

say "helm upgrade to the scenario values (values.yaml alone)"
pf_stop
run "${H[@]}" upgrade trawl "$CHART" -f "$HERE/values.yaml" "${IMG_SET[@]}"
run "${K[@]}" rollout status statefulset/trawl --timeout=300s
show_spec
wait_ready 300 || { note "trawl-0 never became Ready"; run "${K[@]}" describe pod trawl-0; exit 1; }
pf_start
code=$(health "$W/h1.json"); note "GET /api/v1/health -> $code $(cat "$W/h1.json")"
code=$(query "$W/count1.json" 'service=i304 | stats count()'); note "count query -> $code $(jq -c '.rows // .' "$W/count1.json")"
[[ ${STOP_AFTER:-} == ingest ]] && { note "stopping after ingest (STOP_AFTER=ingest, calibration only)"; exit 0; }

say "phase A, starvation: $STARVE_WORKERS clients send STARVE_QUERY back to back for ${STARVE_SECS}s; max_concurrent_queries=2"
run "${K[@]}" get pod trawl-0 -o jsonpath='{range .status.conditions[*]}{.type}={.status} {end}{"\n"}'
code=$(query "$W/a-solo.json" "$STARVE_QUERY"); note "one STARVE_QUERY alone -> $code $(head -c 300 "$W/a-solo.json")"
A=$W/a; mkdir -p "$A"
sample_health "$A/health.log" & bg_pids+=($!)
sample_pod "$A/pod.log" & bg_pids+=($!)
sample_pool "$A/pool.log" & bg_pids+=($!)
follow_logs "$A/trawld.log" & bg_pids+=($!)
a_start=$(ts)
end=$((SECONDS + STARVE_SECS))
workers=()
for w in $(seq 1 "$STARVE_WORKERS"); do
  ( while (( SECONDS < end )); do
      t0=$(ts); r=$(query "$A/w$w.body" "$STARVE_QUERY" 90)
      printf '%s worker=%s -> %s %s\n' "$t0" "$w" "$r" "$(head -c 160 "$A/w$w.body" | tr -d '\n')" >> "$A/workers.log"
    done ) &
  workers+=($!)
  sleep 0.5
done
note "workers started at $a_start; waiting for them to finish their last query"
for p in "${workers[@]}"; do wait "$p" || true; done
sleep 15   # one more readiness period after the last query
bg_stop
note "phase A ended"
say "phase A: STARVE_QUERY responses (start time, worker, HTTP status, seconds, body head)"
runsh "sort '$A/workers.log'"
runsh "awk '{print \$4}' '$A/workers.log' | sort | uniq -c"
say "phase A: GET /api/v1/health every second (time, HTTP status, body)"
runsh "cat '$A/health.log'"
runsh "awk '{print \$2}' '$A/health.log' | sort | uniq -c"
say "phase A: pool counters and held work every 5 s"
runsh "cat '$A/pool.log'"
say "phase A: pod trawl-0 every 2 s"
runsh "cat '$A/pod.log'"
say "phase A: trawld log lines about refused, timed-out or retained work"
runsh "grep -E 'query_not_started|query_permit|reclaim|retained|outcome=\"timeout\"|status=504|route=\"/api/v1/health\"' '$A/trawld.log' | head -n 60"
runsh "grep -c 'event_type=\"query_not_started\".*kind=\"ping\"' '$A/trawld.log' || true"
runsh "grep -oE 'outcome=\"[a-z_]+\"' '$A/trawld.log' | sort | uniq -c"
say "phase A: kubelet events"
events | tee -a "$T" >/dev/null
run "${K[@]}" get pod trawl-0 -o jsonpath='{range .status.conditions[*]}{.type}={.status} {.lastTransitionTime}{"\n"}{end}'
run "${K[@]}" get pods -o wide
note "waiting for trawl-0 to be Ready before phase B"
wait_ready 300 || note "trawl-0 not Ready after 300 s"
pf_start

say "phase B, eviction: SPILL_QUERY up to $SPILL_ATTEMPTS times, ${SPILL_PAUSE}s apart; trawld's /tmp is a 512Mi emptyDir"
uid0=$("${K[@]}" get pod trawl-0 -o jsonpath='{.metadata.uid}')
note "trawl-0 uid before phase B: $uid0"
B=$W/b; mkdir -p "$B"
sample_tmp "$B/tmp.log" & bg_pids+=($!)
has_scratch=no
if "${K[@]}" exec trawl-0 -c trawld -- test -d "$SCRATCH" 2>/dev/null; then
  has_scratch=yes; sample_scratch "$B/scratch.log" & bg_pids+=($!)
fi
note "query scratch $SCRATCH present: $has_scratch"
sample_pod "$B/pod.log" & bg_pids+=($!)
sample_health "$B/health.log" & bg_pids+=($!)
follow_logs "$B/trawld.log" & bg_pids+=($!)
evicted=no
for i in $(seq 1 "$SPILL_ATTEMPTS"); do
  t0=$(ts); r=$(query "$B/spill$i.json" "$SPILL_QUERY" 120)
  note "SPILL_QUERY attempt $i at $t0 -> $r $(head -c 400 "$B/spill$i.json" | tr -d '\n')"
  if events | grep -q 'Evicted'; then evicted=yes; break; fi
  uid=$("${K[@]}" get pod trawl-0 -o jsonpath='{.metadata.uid}' 2>/dev/null || true)
  if [[ $uid != "$uid0" ]]; then evicted=replaced; break; fi
  if (( i < SPILL_ATTEMPTS )); then sleep "$SPILL_PAUSE"; fi
done
note "attempts done (evicted=$evicted); sampling 45 s more"
sleep 45
if [[ $evicted == no ]] && events | grep -q 'Evicted'; then evicted=yes; fi
bg_stop
say "phase B: trawld /tmp usage every 2 s (time, bytes, path)"
runsh "cat '$B/tmp.log'"
if [[ $has_scratch == yes ]]; then
  say "phase B: trawld query scratch usage every 2 s (time, bytes, path)"
  runsh "cat '$B/scratch.log'"
  runsh "awk '{print \$2}' '$B/scratch.log' | sort -n | tail -n 1"
fi
say "phase B: pod trawl-0 every 2 s"
runsh "cat '$B/pod.log'"
say "phase B: GET /api/v1/health every second (time, HTTP status, body)"
runsh "awk '{print \$1, \$2, \$3}' '$B/health.log' | uniq -f1 -c | head -n 60"
say "phase B: trawld log lines about the query, errors and shutdown"
runsh "grep -E 'query_start|query_complete|query_failed|query_error_text|query_timing|http_failure|ERROR|shutdown|signal' '$B/trawld.log' | grep -v 'route=\"/api/v1/health\"' | cut -c1-600 | head -n 80"
say "phase B: kubelet events"
events | tee -a "$T" >/dev/null
run "${K[@]}" get pods -o wide
run "${K[@]}" get pod trawl-0 -o jsonpath='{.metadata.uid} {.status.phase} {.status.reason} {.status.message}{"\n"}'
note "evicted=$evicted"

say "done $LABEL"
