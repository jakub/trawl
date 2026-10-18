#!/usr/bin/env bash
# AC19 of trawl #304: a mixed run of ingest, searches, live tail, compaction
# and repin against the head chart at its defaults, on the local kind cluster
# trawl-i304. It records trawld's peak memory, peak query scratch bytes, OOM
# kills, evictions and every response status into mixed-transcript.txt.
#
#   mixed.sh setup IMAGE CHART_DIR   fresh namespace i304-mixed: PostgreSQL,
#                                    helm install with values-mixed.yaml
#                                    (chart defaults), keys, a preloaded corpus
#   mixed.sh run SECS                one chunk of the workload; run it until
#                                    the chunks add up to at least 15 minutes
#   mixed.sh report                  kubelet events, container state, peaks
#                                    over all chunks, and the verdict
#
# Each part is one foreground command that must finish within 10 minutes, so
# the workload runs in chunks on the same pod. The pod, its data, its cgroup
# memory.peak and its compaction state carry across chunks. Each chunk
# records its start and the gap since the previous chunk ended.
#
# A chunk runs, all at once, for SECS:
#   - steady ingest: INGEST_RATE events a second into service i304, one batch
#     every 2 s (mixed-ingest.py), so compaction runs every 10 s and merges
#     into the current hourly file;
#   - SEARCH_CLIENTS clients, each with its own key, sending the SEARCHES
#     below round-robin, at most one a second each (the chart's default
#     rate limit is 100 a minute per key);
#   - one live tail: an aggregate stream over the ingested service;
#   - repins: a dry run at +60 s, and a real repin of the small field `code`
#     (service i304-repin only) at +200 s, both followed to their end;
#   - samplers: trawld's cgroup memory and /proc/1 RSS plus the query scratch
#     directory every 2 s, the pod every 5 s, /api/v1/health every 2 s,
#     /metrics every 10 s, and trawld's log.
# Each worker exits nonzero when its own work failed, and the chunk records
# every worker's exit status.
#
# The report's verdict is PASS only when every check holds, and report then
# exits 0; otherwise it prints FAIL with the failed checks and exits 1:
#   - the samplers covered at least 900 s of workload;
#   - no OOM kill (cgroup oom_kill 0), no trawld restart, the same pod uid
#     as at setup, and no Evicted, OOMKilling or OOMKilled event;
#   - searches: at least one response; every response 200 with at least one
#     row; each of the SEARCHES answered; no query_resource_exhausted;
#   - ingest: in every chunk the worker ran to its end, no batch was
#     dropped, and the events accepted equal the events sent; more than zero
#     events overall (hot_buffer_full answers carry Retry-After and are
#     retried, so they do not fail the run);
#   - live tail: in every chunk HTTP 200, at least one snapshot, and the
#     stream open until curl's time limit (curl exit 28);
#   - compaction: at least one compaction_complete in every chunk, and no
#     trawld ERROR line and no resource_exhausted, compaction_error,
#     catalog_invariant_violation, rollup_unreadable or catalog_conform_skip
#     line in trawld's log;
#   - repin: every repin the driver sent ended succeeded, including at least
#     one dry run and one real repin;
#   - every worker of every chunk exited 0.
#
# trawld is reached through a NodePort Service on the kind node, not a
# port-forward: kubectl port-forward restarts drop every open connection,
# which a dozen concurrent clients make likely. The Service publishes
# NotReady addresses, so health keeps answering if the pod goes NotReady.
# curl and mixed-ingest.py verify trawld's pinned certificate as
# "localhost", a name its SAN carries.
#
# Secrets: the PostgreSQL passwords, the API keys and their curl configs are
# generated into private/mixed (0700, files 0600), never printed or put on a
# command line.
set -euo pipefail
PART=${1:?usage: mixed.sh setup IMAGE CHART_DIR | run SECS | report}
HERE="$(cd "$(dirname "$0")" && pwd)"
KC=$HERE/kubeconfig
CTX=kind-trawl-i304
NS=i304-mixed
PRIV=$HERE/private/mixed
T=$HERE/mixed-transcript.txt
W=$PRIV/work
K=(kubectl --kubeconfig "$KC" --context "$CTX" -n "$NS")
H=(helm --kubeconfig "$KC" --kube-context "$CTX" -n "$NS")
NODE=trawl-i304-control-plane
SCRATCH=/var/lib/trawl/data.query-scratch

# The workload. Change it only together with the README.
PRELOAD_RATE=10000
PRELOAD_SECS=80
INGEST_RATE=500
SEARCH_CLIENTS=6
SEARCH_MIN_SECS=1
SEARCHES=(
  'service=i304 last=5m | stats count() by host'
  'service=i304 last=1h | top 10 host'
  'service=i304 host=gen-3 last=15m | head 100'
  'service=i304 last=1h | timechart span=1m count()'
  'service=i304 status>=500 last=30m | stats count() by status'
  'service=i304 | stats count()'
  'service=i304 last=1d "QQ" | stats count() by host'
  'service=i304 last=10m | stats count(), dc(host) by status'
)
TAIL_QUERY='service=i304 | stats count() by status'

ts() { date -u +%FT%T.%3NZ; }
say() { printf '\n## [%s] %s\n' "$(ts)" "$*" | tee -a "$T"; }
note() { printf '[%s] %s\n' "$(ts)" "$*" | tee -a "$T"; }
# Echo the literal command, then run it; stdout and stderr land in T.
run() { { printf '[%s] $' "$(ts)"; printf ' %q' "$@"; printf '\n'; } | tee -a "$T"; "$@" 2>&1 | tee -a "$T"; }
runsh() { printf '[%s] $ %s\n' "$(ts)" "$1" | tee -a "$T"; bash -c "$1" 2>&1 | tee -a "$T"; }
nocolor() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }

pod_line() {
  "${K[@]}" get pod trawl-0 -o jsonpath='uid={.metadata.uid} phase={.status.phase} reason={.status.reason} ready={.status.conditions[?(@.type=="Ready")].status} restarts={.status.containerStatuses[?(@.name=="trawld")].restartCount}{"\n"}' 2>&1 | head -n 1
}
events() {
  "${K[@]}" get events --sort-by=.lastTimestamp \
    -o custom-columns=FIRST:.firstTimestamp,LAST:.lastTimestamp,COUNT:.count,TYPE:.type,REASON:.reason,OBJECT:.involvedObject.name,MESSAGE:.message 2>&1
}
bg_pids=()
bg_stop() {  # each sampler is a subshell; stop its children (kubectl logs -f, sleep) too
  for p in "${bg_pids[@]}"; do pkill -P "$p" 2>/dev/null || true; kill "$p" 2>/dev/null || true; done
  for p in "${bg_pids[@]}"; do wait "$p" 2>/dev/null || true; done
  bg_pids=()
}
trap 'bg_stop' EXIT

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

# trawld's API through the NodePort. KEY names a key made by make_key.
addr() { cat "$PRIV/addr"; }
url() { printf 'https://localhost:%s%s' "${ADDR##*:}" "$1"; }
api() {  # api KEY CURL_ARGS...
  local key=$1; shift
  curl -sS --cacert "$PRIV/cert.pem" --resolve "localhost:${ADDR##*:}:${ADDR%:*}" -K "$PRIV/$key.conf" "$@"
}
health() {  # health OUT_BODY: prints the HTTP status
  curl -sS --cacert "$PRIV/cert.pem" --resolve "localhost:${ADDR##*:}:${ADDR%:*}" --max-time 5 \
    -o "$1" -w '%{http_code}' "$(url /api/v1/health)" 2>/dev/null || true
}
query() {  # query KEY OUT_BODY QUERY [MAX_TIME]: prints "code seconds"
  jq -n --arg q "$3" '{query: $q}' > "$2.req"
  api "$1" --max-time "${4:-120}" -o "$2" -w '%{http_code} %{time_total}' -H 'Content-Type: application/json' \
    --data @"$2.req" "$(url /api/v1/query)" 2>/dev/null || true
}
make_key() {  # make_key NAME ROLE: a key, its bearer header file and curl config in PRIV
  note "\$ kubectl exec trawl-0 -c trawld -- fleet-admin keys create --name $1 --kind human --role $2 > \$PRIV/$1.token"
  ( umask 077
    "${K[@]}" exec trawl-0 -c trawld -- sh -c "DATABASE_URL=\"\$FLEET_DATABASE_URL\" exec fleet-admin keys create --name $1 --kind human --role $2" > "$PRIV/$1.token" 2> "$PRIV/$1.stderr"
    printf 'Authorization: Bearer %s\n' "$(tr -d '\n' < "$PRIV/$1.token")" > "$PRIV/$1.header"
    printf 'header = "@%s"\n' "$PRIV/$1.header" > "$PRIV/$1.conf" )
  tee -a "$T" < "$PRIV/$1.stderr"
  note "key $1: token bytes $(wc -c < "$PRIV/$1.token")"
}
metrics_line() {
  api ops --max-time 5 "$(url /metrics)" 2>/dev/null \
    | grep -E '^trawl_(query_permits_in_use|query_permits_retained|query_pool_size|engine_sample_age_seconds|hot_buffer_events|maintenance_resource_exhausted_total|compaction_operation_failures_total)' \
    | tr '\n' ' '
}
cgroup_line() {  # one exec: cgroup memory, trawld's RSS and high-water mark, scratch bytes
  "${K[@]}" exec trawl-0 -c trawld -- sh -c '
    c=$(cat /sys/fs/cgroup/memory.current); p=$(cat /sys/fs/cgroup/memory.peak)
    a=$(grep "^anon " /sys/fs/cgroup/memory.stat | cut -d" " -f2); f=$(grep "^file " /sys/fs/cgroup/memory.stat | cut -d" " -f2)
    r=$(grep "^VmRSS:" /proc/1/status | tr -s " " | cut -d" " -f2); h=$(grep "^VmHWM:" /proc/1/status | tr -s " " | cut -d" " -f2)
    s=$(du -sb '"$SCRATCH"' | cut -f1)
    echo "current=$c anon=$a file=$f peak=$p rss_kib=$r hwm_kib=$h scratch=$s"' 2>&1 | head -n 1
}
# Pick key=value N from a sampler line, as a number (0 when absent).
field_max() {  # field_max FILE KEY: the largest KEY=<n> in FILE
  grep -oE "(^| )$2=[0-9]+" "$1" | cut -d= -f2 | sort -n | tail -n 1
}

setup() {
  local image=$1 chart
  chart=$(cd "$2" && pwd)
  rm -rf "$PRIV"
  mkdir -p "$W"
  chmod 700 "$HERE/private" "$PRIV"
  : > "$T"
  say "mixed run setup: image $image, chart $chart, namespace $NS"
  run kind version
  run helm version --short
  run kubectl --kubeconfig "$KC" --context "$CTX" version
  run kubectl --kubeconfig "$KC" --context "$CTX" get nodes -o wide
  runsh "docker image inspect --format '{{.Id}} {{index .Config.Labels \"org.opencontainers.image.revision\"}}' '$image'"
  note "workload: PRELOAD_RATE=$PRELOAD_RATE PRELOAD_SECS=$PRELOAD_SECS INGEST_RATE=$INGEST_RATE SEARCH_CLIENTS=$SEARCH_CLIENTS SEARCH_MIN_SECS=$SEARCH_MIN_SECS"
  for q in "${SEARCHES[@]}"; do note "SEARCH: $q"; done
  note "TAIL_QUERY: $TAIL_QUERY"
  run docker exec "$NODE" grep -E '^(volumeStatsAggPeriod|evictionHard|  nodefs|  imagefs)' /var/lib/kubelet/config.yaml

  say "namespace and disposable PostgreSQL"
  if kubectl --kubeconfig "$KC" --context "$CTX" get namespace "$NS" >/dev/null 2>&1; then
    run kubectl --kubeconfig "$KC" --context "$CTX" delete namespace "$NS" --wait=true --timeout=180s
  fi
  run kubectl --kubeconfig "$KC" --context "$CTX" create namespace "$NS"
  ( umask 077
    su=$(openssl rand -hex 24); fp=$(openssl rand -hex 24); tp=$(openssl rand -hex 24)
    printf "CREATE ROLE fleet LOGIN PASSWORD '%s';\nCREATE ROLE trawl LOGIN PASSWORD '%s';\nCREATE DATABASE fleet OWNER fleet;\nCREATE DATABASE trawl OWNER trawl;\n" "$fp" "$tp" > "$PRIV/init.sql"
    # The short Service name: see NOTES.md on the node's resolv.conf.
    printf '%s' "$su" > "$PRIV/superuser-password"
    printf 'postgres://fleet:%s@postgres:5432/fleet' "$fp" > "$PRIV/fleet-url"
    printf 'postgres://trawl:%s@postgres:5432/trawl' "$tp" > "$PRIV/trawl-url" )
  note "\$ kubectl create secret generic pg-init|fleet-db|trawl-db --from-file=... (values from \$PRIV, not printed)"
  "${K[@]}" create secret generic pg-init --from-file=superuser-password="$PRIV/superuser-password" --from-file=init.sql="$PRIV/init.sql" 2>&1 | tee -a "$T"
  "${K[@]}" create secret generic fleet-db --from-file=DATABASE_URL="$PRIV/fleet-url" 2>&1 | tee -a "$T"
  "${K[@]}" create secret generic trawl-db --from-file=TRAWL_DATABASE_URL="$PRIV/trawl-url" 2>&1 | tee -a "$T"
  run "${K[@]}" apply -f "$HERE/postgres.yaml"
  run "${K[@]}" rollout status deployment/postgres --timeout=180s

  say "helm install at the chart defaults (values-mixed.yaml: image pull policy, DB secrets, no web sidecar)"
  run "${H[@]}" install trawl "$chart" -f "$HERE/values-mixed.yaml" \
    --set-string "image.repository=${image%:*}" --set-string "image.tag=${image##*:}"
  "${K[@]}" get configmap trawl -o jsonpath='{.data.trawld\.toml}' > "$W/trawld.toml"
  runsh "grep -E '^\[|max_concurrent|query_|compaction_memory|min_free|_rpm|hot_buffer_max|timeout_secs' '$W/trawld.toml'"
  runsh "kubectl --kubeconfig '$KC' --context $CTX -n $NS get statefulset trawl -o json | jq -c '.spec.template.spec | {terminationGracePeriodSeconds, trawld_resources: (.containers[] | select(.name==\"trawld\") | .resources), tmp: (.volumes[] | select(.name==\"tmp\")), probes: (.containers[] | select(.name==\"trawld\") | {startup: .startupProbe | {periodSeconds, failureThreshold}, readiness: .readinessProbe | {periodSeconds, failureThreshold, timeoutSeconds}, liveness: .livenessProbe | {periodSeconds, failureThreshold}}), log: (.containers[] | select(.name==\"trawld\") | .env[] | select(.name==\"RUST_LOG\") | .value)}'"
  wait_ready 360 || { note "trawl-0 never became Ready"; run "${K[@]}" describe pod trawl-0; exit 1; }
  run "${K[@]}" get pods -o wide

  say "NodePort Service to trawld (publishes NotReady addresses)"
  "${K[@]}" apply -f - <<'EOF' 2>&1 | tee -a "$T"
apiVersion: v1
kind: Service
metadata:
  name: trawl-mixed-nodeport
spec:
  type: NodePort
  publishNotReadyAddresses: true
  selector:
    app.kubernetes.io/name: trawl
    app.kubernetes.io/instance: trawl
  ports:
    - port: 5514
      targetPort: 5514
EOF
  local nip np
  nip=$(kubectl --kubeconfig "$KC" --context "$CTX" get node "$NODE" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
  np=$("${K[@]}" get service trawl-mixed-nodeport -o jsonpath='{.spec.ports[0].nodePort}')
  printf '%s:%s' "$nip" "$np" > "$PRIV/addr"
  ADDR=$(addr)
  note "trawld at $ADDR (TLS name localhost)"

  say "roles, keys and trawld's certificate"
  run "${K[@]}" exec trawl-0 -c trawld -- sh -c 'DATABASE_URL="$FLEET_DATABASE_URL" exec fleet-admin roles create --name mixed-ingest --perm trawl:ingest'
  run "${K[@]}" exec trawl-0 -c trawld -- sh -c 'DATABASE_URL="$FLEET_DATABASE_URL" exec fleet-admin roles create --name mixed-search --perm trawl:query --perm trawl:stream'
  run "${K[@]}" exec trawl-0 -c trawld -- sh -c 'DATABASE_URL="$FLEET_DATABASE_URL" exec fleet-admin roles create --name mixed-ops --perm trawl:query --perm trawl:schema_read --perm trawl:schema_write --perm trawl:server_manage'
  make_key ingest mixed-ingest
  make_key ops mixed-ops
  make_key tail mixed-search
  for c in $(seq 1 "$SEARCH_CLIENTS"); do make_key "s$c" mixed-search; done
  "${K[@]}" exec trawl-0 -c trawld -- cat /var/lib/trawl/tls/cert.pem > "$PRIV/cert.pem"
  runsh "openssl x509 -in '$PRIV/cert.pem' -noout -subject -ext subjectAltName"
  code=$(health "$W/h0.json"); note "GET /api/v1/health -> $code $(cat "$W/h0.json")"
  note "whoami (ops) -> $(api ops "$(url /api/v1/whoami)" | jq -c .)"
  note "trawld container: pid 1 is $("${K[@]}" exec trawl-0 -c trawld -- cat /proc/1/comm), nproc $("${K[@]}" exec trawl-0 -c trawld -- nproc 2>&1), memory.max $("${K[@]}" exec trawl-0 -c trawld -- cat /sys/fs/cgroup/memory.max), cpu.max $("${K[@]}" exec trawl-0 -c trawld -- cat /sys/fs/cgroup/cpu.max)"
  note "pool and engine metrics: $(metrics_line)"

  say "preload: service i304 at $PRELOAD_RATE events/s for ${PRELOAD_SECS}s, then 5000 events of service i304-repin carrying the integer field code"
  note "\$ python3 -I mixed-ingest.py $ADDR \$PRIV/cert.pem \$PRIV/ingest.header $PRELOAD_RATE $PRELOAD_SECS 304 i304 > \$W/preload.log"
  python3 -I "$HERE/mixed-ingest.py" "$ADDR" "$PRIV/cert.pem" "$PRIV/ingest.header" "$PRELOAD_RATE" "$PRELOAD_SECS" 304 i304 > "$W/preload.log" 2>&1 \
    || { runsh "tail -n 5 '$W/preload.log'"; note "preload failed"; exit 1; }
  runsh "awk '{print \$2}' '$W/preload.log' | sort | uniq -c; tail -n 1 '$W/preload.log'"
  note "\$ python3 -I mixed-ingest.py $ADDR \$PRIV/cert.pem \$PRIV/ingest.header 2500 2 305 i304-repin code > \$W/preload-repin.log"
  python3 -I "$HERE/mixed-ingest.py" "$ADDR" "$PRIV/cert.pem" "$PRIV/ingest.header" 2500 2 305 i304-repin code > "$W/preload-repin.log" 2>&1 \
    || { runsh "cat '$W/preload-repin.log'"; note "preload of i304-repin failed"; exit 1; }
  runsh "cat '$W/preload-repin.log'"
  note "waiting for the hot buffer to drain into Parquet"
  local hb=
  for _ in $(seq 1 150); do
    hb=$(api ops --max-time 5 "$(url /metrics)" 2>/dev/null | awk '$1=="trawl_hot_buffer_events"{print $2}')
    [[ -n $hb ]] && (( ${hb%.*} < 2000 )) && break
    sleep 2
  done
  note "trawl_hot_buffer_events=$hb"
  for q in 'service=i304 | stats count()' 'service=i304-repin | stats count(), min(code), max(code)'; do
    code=$(query ops "$W/count.json" "$q"); note "$q -> $code $(jq -c '.rows // .' "$W/count.json")"
  done
  runsh "kubectl --kubeconfig '$KC' --context $CTX -n $NS exec trawl-0 -c trawld -- sh -c 'du -sh /var/lib/trawl/data $SCRATCH /tmp; ls /var/lib/trawl/data'"

  say "each search once (client s1)"
  local i=0
  for q in "${SEARCHES[@]}"; do
    i=$((i + 1))
    r=$(query s1 "$W/check$i.json" "$q"); note "search $i -> $r $(head -c 200 "$W/check$i.json" | tr -d '\n')"
  done
  "${K[@]}" get pod trawl-0 -o jsonpath='{.metadata.uid}' > "$PRIV/uid0"
  note "trawl-0 uid at setup: $(cat "$PRIV/uid0")"
  note "cgroup and scratch: $(cgroup_line)"
  echo 0 > "$PRIV/chunks"
  date -u +%s > "$PRIV/last-end"
  say "setup done"
}

# Samplers for one chunk. Each writes timestamped lines to a file.
sample_cgroup() { while :; do printf '%s %s\n' "$(ts)" "$(cgroup_line)" >> "$1"; sleep 2; done; }
sample_pod() { while :; do printf '%s %s\n' "$(ts)" "$(pod_line)" >> "$1"; sleep 5; done; }
sample_health() {
  local code
  while :; do
    code=$(health "$1.body")
    printf '%s %s %s\n' "$(ts)" "$code" "$(tr -d '\n' < "$1.body" 2>/dev/null)" >> "$1"
    : > "$1.body"
    sleep 2
  done
}
sample_metrics() { while :; do printf '%s %s\n' "$(ts)" "$(metrics_line)" >> "$1"; sleep 10; done; }
follow_logs() { "${K[@]}" logs -f trawl-0 -c trawld --timestamps --since=1s 2>&1 | nocolor >> "$1"; }

# A search succeeds when it answers 200, with no error code and at least one
# row. Each log line ends "-> HTTP SECONDS ERROR_CODE rows=N".
SEARCH_OK=' -> 200 [0-9.]+ - rows=[1-9][0-9]*$'
search_client() {  # search_client N END OUT_LOG: SEARCHES round-robin from offset N until END
  local n=$1 end=$2 out=$3 i=$(( $1 - 1 )) q t0 s0 r code failed=0
  while (( SECONDS < end )); do
    q=${SEARCHES[$(( i % ${#SEARCHES[@]} ))]}
    t0=$(ts); s0=$EPOCHREALTIME
    : > "$out.body"
    r=$(query "s$n" "$out.body" "$q" 60)
    code=$(jq -r '"\(.error.code // "-") rows=\((.rows // []) | length)"' "$out.body" 2>/dev/null || echo "unparsed rows=-")
    printf '%s client=%s search=%s -> %s %s\n' "$t0" "$n" "$(( i % ${#SEARCHES[@]} + 1 ))" "$r" "$code" >> "$out"
    tail -n 1 "$out" | grep -qE "$SEARCH_OK" || failed=1
    i=$((i + 1))
    sleep "$(awk -v a="$s0" -v b="$EPOCHREALTIME" -v m="$SEARCH_MIN_SECS" 'BEGIN { d = m - (b - a); print (d > 0 ? d : 0) }')"
  done
  return "$failed"
}
repin() {  # repin OUT_LOG JSON_BODY END: POST a repin, follow a 202 to its end; 0 when it succeeded
  local out=$1 body=$2 end=$3 r id st dry
  printf '%s POST /api/v1/schema/repin %s\n' "$(ts)" "$body" >> "$out"
  : > "$out.body"
  r=$(api ops --max-time 60 -o "$out.body" -w '%{http_code} %{time_total}' -H 'Content-Type: application/json' \
    --data "$body" "$(url /api/v1/schema/repin)" 2>/dev/null || true)
  printf '%s -> %s %s\n' "$(ts)" "$r" "$(jq -c '.job // .' "$out.body" 2>/dev/null | head -c 900)" >> "$out"
  id=$(jq -r '.job.id // empty' "$out.body" 2>/dev/null || true)
  st=$(jq -r '.job.status // empty' "$out.body" 2>/dev/null || true)
  while [[ $st == running && -n $id ]] && (( SECONDS < end )); do
    sleep 5
    api ops --max-time 10 -o "$out.body" "$(url "/api/v1/schema/repin/status?id=$id")" 2>/dev/null || true
    st=$(jq -r '.job.status // empty' "$out.body" 2>/dev/null || true)
    printf '%s status id=%s -> %s\n' "$(ts)" "$id" "$(jq -c '.job // .' "$out.body" 2>/dev/null | head -c 900)" >> "$out"
  done
  dry=$(jq -r '.dry_run // false' <<< "$body")
  printf '%s result id=%s dry_run=%s status=%s\n' "$(ts)" "${id:--}" "$dry" "${st:-none}" >> "$out"
  [[ $st == succeeded ]]
}
repin_driver() {  # repin_driver CHUNK START END OUT_LOG: 0 when both repins succeeded
  local chunk=$1 start=$2 end=$3 out=$4 to failed=0
  # The real repin flips code between VARCHAR and BIGINT; each chunk reads the
  # current pin from the previous job, so a rerun keeps alternating. The dry
  # run asks what pinning status, an integer field of every event, as text
  # would do.
  to=BIGINT; (( chunk % 2 == 1 )) && to=VARCHAR
  while (( SECONDS < start + 60 )); do sleep 1; done
  repin "$out" '{"field":"status","to":"VARCHAR","dry_run":true}' "$end" || failed=1
  while (( SECONDS < start + 200 )); do sleep 1; done
  repin "$out" "{\"field\":\"code\",\"to\":\"$to\"}" "$end" || failed=1
  return "$failed"
}

chunk() {
  local secs=$1 n start end c last
  ADDR=$(addr)
  n=$(( $(cat "$PRIV/chunks") + 1 ))
  last=$(cat "$PRIV/last-end")
  c=$W/c$n; mkdir -p "$c"
  say "chunk $n: the mixed workload for ${secs}s ($(( $(date -u +%s) - last ))s since the previous part ended)"
  note "pod trawl-0 $(pod_line)"
  note "cgroup and scratch: $(cgroup_line)"
  sample_cgroup "$c/cgroup.log" & bg_pids+=($!)
  sample_pod "$c/pod.log" & bg_pids+=($!)
  sample_health "$c/health.log" & bg_pids+=($!)
  sample_metrics "$c/metrics.log" & bg_pids+=($!)
  follow_logs "$c/trawld.log" & bg_pids+=($!)
  start=$SECONDS
  end=$((SECONDS + secs))
  local work=() names=() p rc k
  python3 -I "$HERE/mixed-ingest.py" "$ADDR" "$PRIV/cert.pem" "$PRIV/ingest.header" "$INGEST_RATE" "$secs" "$((400 + n))" i304 > "$c/ingest.log" 2>&1 &
  work+=($!); names+=(ingest)
  # The tail succeeds when it answers 200 and stays open until curl's limit.
  ( rc=0
    api tail -N --max-time "$secs" -o "$c/tail.sse" -w '%{http_code}' -G --data-urlencode "query=$TAIL_QUERY" "$(url /api/v1/stream)" > "$c/tail.status" 2>"$c/tail.stderr" || rc=$?
    echo " exit=$rc" >> "$c/tail.status"
    [[ $(cat "$c/tail.status") == "200 exit=28" ]] ) &
  work+=($!); names+=(tail)
  for s in $(seq 1 "$SEARCH_CLIENTS"); do
    search_client "$s" "$end" "$c/search-$s.log" &
    work+=($!); names+=("search-$s")
  done
  repin_driver "$n" "$start" "$end" "$c/repin.log" &
  work+=($!); names+=(repin)
  note "workers started: ingest, live tail, $SEARCH_CLIENTS search clients, repin driver; running ${secs}s"
  : > "$c/workers.log"
  for k in "${!work[@]}"; do
    p=${work[$k]}
    if wait "$p"; then rc=0; else rc=$?; fi
    echo "${names[$k]} exit=$rc" >> "$c/workers.log"
  done
  sleep 5
  bg_stop
  note "chunk $n workers finished"
  runsh "cat '$c/workers.log'"
  cat "$c"/search-*.log | sort > "$c/searches.log"

  say "chunk $n: search responses (HTTP status, error code) per search"
  runsh "awk '{print \$3, \$5, \$7, (\$8 ~ /^rows=[1-9]/ ? \"rows>0\" : \$8)}' '$c/searches.log' | sort | uniq -c"
  runsh "awk '{print \$5, \$7, (\$8 ~ /^rows=[1-9]/ ? \"rows>0\" : \$8)}' '$c/searches.log' | sort | uniq -c"
  runsh "grep -vE '$SEARCH_OK' '$c/searches.log' | head -n 40 || true"
  runsh "grep -c query_resource_exhausted '$c/searches.log' || true"
  runsh "awk '{print \$6}' '$c/searches.log' | sort -n | awk '{a[NR]=\$1} END {print \"search seconds: n=\" NR, \"p50=\" a[int(NR*0.5)+1], \"p95=\" a[int(NR*0.95)+1], \"max=\" a[NR]}'"
  say "chunk $n: ingest responses"
  runsh "awk '{print \$2, (\$3 ~ /^[0-9]+\$/ ? \"accepted\" : \$3)}' '$c/ingest.log' | sort | uniq -c"
  runsh "grep -vE ' 200 [0-9]+ ' '$c/ingest.log' | head -n 20 || true"
  say "chunk $n: live tail"
  runsh "cat '$c/tail.status'; cat '$c/tail.stderr'; grep -c '^event: snapshot' '$c/tail.sse' || true; grep -E '^event: ' '$c/tail.sse' | sort | uniq -c; grep '^data: ' '$c/tail.sse' | tail -n 1 | cut -c1-400"
  say "chunk $n: repins"
  runsh "cat '$c/repin.log'"
  say "chunk $n: trawld cgroup memory, RSS (KiB) and query scratch bytes every 2 s"
  runsh "cat '$c/cgroup.log'"
  note "chunk $n peaks: memory.current $(field_max "$c/cgroup.log" current), anon $(field_max "$c/cgroup.log" anon), file $(field_max "$c/cgroup.log" file), memory.peak $(field_max "$c/cgroup.log" peak), VmRSS $(field_max "$c/cgroup.log" rss_kib) KiB, VmHWM $(field_max "$c/cgroup.log" hwm_kib) KiB, scratch $(field_max "$c/cgroup.log" scratch) bytes"
  say "chunk $n: pod trawl-0 every 5 s"
  runsh "awk '{\$1=\"\"; print}' '$c/pod.log' | sort | uniq -c"
  say "chunk $n: GET /api/v1/health every 2 s (HTTP status, duckdb check)"
  runsh "awk '{s=\$2; d=\"-\"; if (match(\$0, /\"duckdb\":\"[a-z]+\"/)) d=substr(\$0, RSTART, RLENGTH); print s, d}' '$c/health.log' | sort | uniq -c"
  runsh "grep -v ' 200 ' '$c/health.log' | head -n 20 || true"
  say "chunk $n: /metrics every 10 s"
  runsh "cat '$c/metrics.log'"
  say "chunk $n: trawld log: compaction, maintenance, repin, warnings and errors"
  runsh "grep -c 'event_type=\"compaction_complete\"' '$c/trawld.log' || true"
  runsh "grep 'event_type=\"compaction_complete\"' '$c/trawld.log' | grep -oE 'merged=[a-z]+' | sort | uniq -c"
  runsh "grep 'event_type=\"compaction_complete\"' '$c/trawld.log' | grep 'service=\"i304\"' | tail -n 3 | cut -c1-500"
  runsh "grep -E 'repin' '$c/trawld.log' | grep -vE 'route=\"/api/v1/schema/repin/status\"' | cut -c1-500 | head -n 30"
  runsh "grep -E ' (WARN|ERROR) ' '$c/trawld.log' | grep -oE 'event_type=\"[a-z_]+\"' | sort | uniq -c"
  runsh "grep -E ' ERROR |resource_exhausted|catalog_invariant_violation|rollup_unreadable|catalog_conform_skip|compaction_error' '$c/trawld.log' | cut -c1-500 | head -n 30"
  say "chunk $n: kubelet events"
  events | tee -a "$T" >/dev/null
  echo "$n" > "$PRIV/chunks"
  date -u +%s > "$PRIV/last-end"
  say "chunk $n done"
}

report() {
  local n uid0 uid all
  ADDR=$(addr)
  n=$(cat "$PRIV/chunks")
  uid0=$(cat "$PRIV/uid0")
  say "report over $n chunks"
  events | tee -a "$T" >/dev/null
  run "${K[@]}" get pods -o wide
  runsh "kubectl --kubeconfig '$KC' --context $CTX -n $NS get pod trawl-0 -o json | jq -c '{uid: .metadata.uid, phase: .status.phase, reason: .status.reason, trawld: (.status.containerStatuses[] | select(.name==\"trawld\") | {ready, restartCount, state, lastState})}'"
  runsh "kubectl --kubeconfig '$KC' --context $CTX -n $NS exec trawl-0 -c trawld -- sh -c 'echo memory.peak \$(cat /sys/fs/cgroup/memory.peak); echo memory.max \$(cat /sys/fs/cgroup/memory.max); cat /sys/fs/cgroup/memory.events; grep -E \"^Vm(HWM|RSS):\" /proc/1/status'"
  uid=$("${K[@]}" get pod trawl-0 -o jsonpath='{.metadata.uid}')
  all=$W/all-cgroup.log
  cat "$W"/c*/cgroup.log > "$all"
  cat "$W"/c*/searches.log > "$W/all-searches.log"
  local secs=0 c s0 s1
  for c in "$W"/c*/; do
    s0=$(head -n 1 "$c/cgroup.log" | cut -d' ' -f1); s1=$(tail -n 1 "$c/cgroup.log" | cut -d' ' -f1)
    secs=$(( secs + $(date -ud "$s1" +%s) - $(date -ud "$s0" +%s) ))
  done
  local oomk evict searches nok rex peak hwm restarts
  oomk=$("${K[@]}" exec trawl-0 -c trawld -- sh -c 'grep "^oom_kill " /sys/fs/cgroup/memory.events | cut -d" " -f2')
  restarts=$("${K[@]}" get pod trawl-0 -o jsonpath='{.status.containerStatuses[?(@.name=="trawld")].restartCount}')
  evict=$(events | grep -cE 'Evicted|OOMKilling|OOMKilled' || true)
  searches=$(wc -l < "$W/all-searches.log")
  nok=$(grep -cvE "$SEARCH_OK" "$W/all-searches.log" || true)
  rex=$(grep -c query_resource_exhausted "$W/all-searches.log" || true)
  peak=$("${K[@]}" exec trawl-0 -c trawld -- cat /sys/fs/cgroup/memory.peak)
  hwm=$("${K[@]}" exec trawl-0 -c trawld -- sh -c 'grep "^VmHWM:" /proc/1/status | tr -s " " | cut -d" " -f2')
  say "summary"
  note "workload sampled for ${secs}s over $n chunks (the gaps between chunks excluded)"
  note "trawl-0 uid at setup $uid0, now $uid; trawld restarts $restarts; cgroup oom_kill $oomk"
  note "kubelet events naming Evicted, OOMKilling or OOMKilled: $evict"
  note "searches: $searches responses, $nok not 200 with rows, $rex query_resource_exhausted"
  runsh "awk '{print \$5, \$7, (\$8 ~ /^rows=[1-9]/ ? \"rows>0\" : \$8)}' '$W/all-searches.log' | sort | uniq -c"
  runsh "cat '$W'/c*/ingest.log | awk '{print \$2, (\$3 ~ /^[0-9]+\$/ ? \"accepted\" : \$3)}' | sort | uniq -c"
  runsh "cat '$W'/c*/ingest.log | awk '\$2==200 && \$3 ~ /^[0-9]+\$/ {s+=\$3} END {print \"events accepted in chunks:\", s}'"
  runsh "grep -h ' done ' '$W'/c*/ingest.log"
  runsh "cat '$W'/c*/health.log | awk '{print \$2}' | sort | uniq -c"
  runsh "cat '$W'/c*/pod.log | grep -oE 'ready=[A-Za-z]+ restarts=[0-9]+' | sort | uniq -c"
  runsh "cat '$W'/c*/trawld.log | grep -c 'event_type=\"compaction_complete\"'; cat '$W'/c*/trawld.log | grep 'event_type=\"compaction_complete\"' | grep -oE 'merged=[a-z]+' | sort | uniq -c"
  runsh "grep -hE ' -> [0-9]+ ' '$W'/c*/repin.log | cut -c1-300; grep -h 'status id=' '$W'/c*/repin.log | grep -v '\"status\":\"running\"' | cut -c1-300"
  runsh "grep -h ' result ' '$W'/c*/repin.log"
  runsh "grep -H . '$W'/c*/workers.log"
  note "peak memory.current $(field_max "$all" current) bytes; peak anon $(field_max "$all" anon) bytes; peak file $(field_max "$all" file) bytes"
  note "cgroup memory.peak $peak bytes (includes page cache); trawld VmHWM $hwm KiB (peak RSS since start)"
  note "peak query scratch $(field_max "$all" scratch) bytes (du -sb of $SCRATCH every 2 s)"

  say "verdict checks"
  local failed=() d
  check() {  # check NAME VALUE CONDITION...: CONDITION is a command; a failure joins the verdict
    local name=$1 value=$2; shift 2
    if "$@"; then note "check $name: $value ok"; else note "check $name: $value FAIL"; failed+=("$name"); fi
  }
  # Per-chunk facts, gathered from the chunk files.
  local chunks_ingest_done=0 ingest_sent=0 ingest_acc=0 ingest_dropped=0 tail_ok=0 compact_chunks=0
  local compactions=0 trawld_errors=0 repin_total=0 repin_ok=0 repin_dry_ok=0 repin_real_ok=0 workers_bad=0 asked k
  for c in "$W"/c*/; do
    d=$(grep ' done sent=' "$c/ingest.log" | tail -n 1 || true)
    if [[ -n $d ]]; then
      chunks_ingest_done=$((chunks_ingest_done + 1))
      ingest_sent=$(( ingest_sent + $(sed -E 's/.* sent=([0-9]+).*/\1/' <<< "$d") ))
      ingest_acc=$(( ingest_acc + $(sed -E 's/.* accepted=([0-9]+).*/\1/' <<< "$d") ))
    fi
    ingest_dropped=$(( ingest_dropped + $(grep -cE '^[^ ]+ [0-9]{3} dropped' "$c/ingest.log" || true) ))
    [[ $(cat "$c/tail.status" 2>/dev/null) == "200 exit=28" ]] && (( $(grep -c '^event: snapshot' "$c/tail.sse" 2>/dev/null || true) > 0 )) && tail_ok=$((tail_ok + 1))
    k=$(grep -c 'event_type="compaction_complete"' "$c/trawld.log" || true)
    compactions=$((compactions + k)); (( k > 0 )) && compact_chunks=$((compact_chunks + 1))
    trawld_errors=$(( trawld_errors + $(grep -cE ' ERROR |resource_exhausted|compaction_error|catalog_invariant_violation|rollup_unreadable|catalog_conform_skip' "$c/trawld.log" || true) ))
    repin_total=$(( repin_total + $(grep -c ' result ' "$c/repin.log" || true) ))
    repin_ok=$(( repin_ok + $(grep -c ' result .* status=succeeded$' "$c/repin.log" || true) ))
    repin_dry_ok=$(( repin_dry_ok + $(grep -c ' result .* dry_run=true status=succeeded$' "$c/repin.log" || true) ))
    repin_real_ok=$(( repin_real_ok + $(grep -c ' result .* dry_run=false status=succeeded$' "$c/repin.log" || true) ))
    workers_bad=$(( workers_bad + $(grep -cv ' exit=0$' "$c/workers.log" || true) ))
  done
  asked=$(awk '{print $3}' "$W/all-searches.log" | sort -u | wc -l)
  check workload_seconds "$secs (need >= 900)" test "$secs" -ge 900
  check oom_kill "$oomk" test "$oomk" = 0
  check restarts "$restarts" test "$restarts" = 0
  check same_pod "$uid0 -> $uid" test "$uid" = "$uid0"
  check evict_or_oom_events "$evict" test "$evict" = 0
  check search_responses "$searches" test "$searches" -gt 0
  check searches_not_200_with_rows "$nok" test "$nok" = 0
  check searches_answered "$asked of ${#SEARCHES[@]}" test "$asked" = "${#SEARCHES[@]}"
  check query_resource_exhausted "$rex" test "$rex" = 0
  check ingest_ran_to_end "$chunks_ingest_done of $n chunks" test "$chunks_ingest_done" = "$n"
  check ingest_dropped_batches "$ingest_dropped" test "$ingest_dropped" = 0
  check ingest_accepted "$ingest_acc of $ingest_sent sent" eval '(( ingest_acc > 0 && ingest_acc == ingest_sent ))'
  check live_tail "$tail_ok of $n chunks with 200, snapshots and the stream open to the end" test "$tail_ok" = "$n"
  check compaction "$compactions compaction_complete, in $compact_chunks of $n chunks" test "$compact_chunks" = "$n"
  check trawld_error_lines "$trawld_errors" test "$trawld_errors" = 0
  check repin "$repin_ok of $repin_total succeeded ($repin_dry_ok dry, $repin_real_ok real)" eval '(( repin_total > 0 && repin_ok == repin_total && repin_dry_ok > 0 && repin_real_ok > 0 ))'
  check workers "$workers_bad nonzero exits" test "$workers_bad" = 0
  local verdict=PASS
  (( n > 0 && ${#failed[@]} == 0 )) || verdict="FAIL (${failed[*]:-no chunks})"
  note "verdict: $verdict (no OOM kill, no eviction, no query_resource_exhausted for ordinary searches, and every workload part succeeded)"
  say "report done"
  [[ $verdict == PASS ]]
}

case $PART in
  setup) setup "${2:?IMAGE}" "${3:?CHART_DIR}" ;;
  run) chunk "${2:?SECS}" ;;
  report) report ;;
  *) echo "unknown part $PART" >&2; exit 2 ;;
esac
