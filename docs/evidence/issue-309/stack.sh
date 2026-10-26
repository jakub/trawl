#!/usr/bin/env bash
# Disposable evidence stack for issue #309: a branch-built trawld on a private
# Postgres, an agent key with #308's recipe, a CLI config in a private HOME,
# and three seeded scenarios with known counts, one per correlation read.
# Everything lives under $TRAWL_EVIDENCE_DIR (default target/evidence-309) and
# is torn down by `stack.sh down`. Copied from docs/evidence/issue-308/stack.sh
# with its own container, ports and seed.
set -euo pipefail
WT=$(git rev-parse --show-toplevel)
E=$(cd "$(dirname "$0")" && pwd)
RUN=${TRAWL_EVIDENCE_DIR:-$WT/target/evidence-309}
PG_CONTAINER=trawl-309-pg-e
PG_PORT=5455
ADMIN=${CARGO_TARGET_DIR:-$WT/target}/debug/fleet-admin
TRAWL=${CARGO_TARGET_DIR:-$WT/target}/debug/trawl
cmd=${1:-up}

up() {
  mkdir -p "$RUN/private" "$RUN/home/.config/trawl"
  if ! docker inspect $PG_CONTAINER >/dev/null 2>&1; then
    docker run -d --name $PG_CONTAINER -p 127.0.0.1:$PG_PORT:5432 \
      -e POSTGRES_USER=evidence -e POSTGRES_PASSWORD=evidence -e POSTGRES_DB=fleet \
      --shm-size=1g --tmpfs /var/lib/postgresql:size=512m postgres:18 -c fsync=off >/dev/null
  fi
  for i in $(seq 1 60); do docker exec $PG_CONTAINER pg_isready -h 127.0.0.1 -U evidence -d fleet >/dev/null 2>&1 && break; sleep 1; done
  docker exec $PG_CONTAINER createdb -U evidence trawl 2>/dev/null || true
  FLEET_DSN="postgres://evidence:evidence@127.0.0.1:$PG_PORT/fleet"
  APP_DSN="postgres://evidence:evidence@127.0.0.1:$PG_PORT/trawl"
  export DATABASE_URL=$FLEET_DSN
  $ADMIN migrate >/dev/null
  $ADMIN roles create --name agent --rate-rpm 600 --perm trawl:query --perm trawl:schema_read --perm trawl:validate >/dev/null 2>&1 || true
  $ADMIN roles create --name sender --rate-rpm 100000 --perm trawl:ingest >/dev/null 2>&1 || true
  AGENT_KEY=$($ADMIN keys create --name evidence-agent --kind service --role agent --expires 12h)
  SENDER_KEY=$($ADMIN keys create --name evidence-sender --kind service --role sender --expires 12h)
  umask 077
  printf '%s\n' "$SENDER_KEY" > "$RUN/private/sender-key"
  cat > "$RUN/private/trawld.toml" <<TOML
[server]
http_addr = "127.0.0.1:18444"
max_result_rows = 25000
max_concurrent_queries = 2
shutdown_drain_secs = 3
link_origin = "https://trawl.example"
[data]
path = "$RUN/private/data"
[ingest]
enabled = true
internal_telemetry = false
daily_rollup = false
compaction_interval_secs = 10
compaction_chunk_size = 500
default_env = "prod"
envs = ["prod", "lab"]
[retention]
max_age_days = 0
min_free_disk_bytes = 0
[scheduler]
enabled = false
[syslog]
enabled = false
TOML
  FLEET_DATABASE_URL=$FLEET_DSN TRAWL_DATABASE_URL=$APP_DSN CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$WT/target} \
    nohup setsid $WT/bin/trawld-dev --config "$RUN/private/trawld.toml" > "$RUN/trawld.log" 2>&1 &
  echo $! > "$RUN/trawld.pid"
  for i in $(seq 1 60); do grep -q "HTTPS server listening" "$RUN/trawld.log" 2>/dev/null && break; sleep 1; done
  grep -o 'HTTPS server listening[^\n]*' "$RUN/trawld.log" | head -1
  CERT=$RUN/private/tls/cert.pem
  [ -f "$CERT" ] || { echo "no cert at $CERT"; ls "$RUN/private"; exit 1; }
  cat > "$RUN/home/.config/trawl/config.toml" <<TOML
[profiles.agent]
url = "https://127.0.0.1:18444"
ca_cert = "$CERT"
token = "$AGENT_KEY"

[ui]
timezone = "UTC"
TOML
  echo "stack up: trawld pid $(cat "$RUN/trawld.pid"), HOME=$RUN/home, profile agent"
}

seed() {
  # Three scenarios (seed.py), each on its own hosts or level so no read sees
  # another's rows. The NDJSON goes over /api/v1/ingest; seed.py also writes
  # focus.json, the focus row the transcript prompt names. A refused ingest
  # (any HTTP error status) stops the recipe here.
  SENDER_KEY=$(cat "$RUN/private/sender-key")
  python3 "$E/seed.py" "$RUN/seed.ndjson" "$RUN/focus.json"
  curl -sS --fail-with-body --cacert "$RUN/private/tls/cert.pem" -H "authorization: Bearer $SENDER_KEY" \
    -H 'content-type: application/x-ndjson' --data-binary @"$RUN/seed.ndjson" https://127.0.0.1:18444/api/v1/ingest
  echo
}

down() {
  [ -f "$RUN/trawld.pid" ] && kill "$(cat "$RUN/trawld.pid")" 2>/dev/null && sleep 2 || true
  docker rm -f $PG_CONTAINER >/dev/null 2>&1 || true
  rm -rf "$RUN/private" "$RUN/home"
  echo "stack down"
}

case $cmd in up) up ;; seed) seed ;; down) down ;; *) echo "usage: stack.sh up|seed|down"; exit 2 ;; esac
