#!/usr/bin/env bash
# Disposable evidence stack for issue #310: a branch-built trawld on a private
# Postgres, two keys minted from the operate/access.md recipe (the agent role
# and the person's read-only role), a CLI config with one profile for each in
# a private HOME, and a seeded incident for the agent to investigate.
# Everything lives under $TRAWL_EVIDENCE_DIR (default target/evidence-310) and
# is torn down by `stack.sh down`. Copied from docs/evidence/issue-309/stack.sh
# with its own container, ports, roles and seed.
set -euo pipefail
WT=$(git rev-parse --show-toplevel)
E=$(cd "$(dirname "$0")" && pwd)
RUN=${TRAWL_EVIDENCE_DIR:-$WT/target/evidence-310}
PG_CONTAINER=trawl-310-pg-e
PG_PORT=5480
HTTP_PORT=18450
ADMIN=${CARGO_TARGET_DIR:-$WT/target}/debug/fleet-admin
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
  # The two roles of operate/access.md, "Create a key for an agent", verbatim.
  $ADMIN roles create --name trawl-agent --rate-rpm 60 \
    --perm trawl:query --perm trawl:schema_read --perm trawl:validate \
    --perm trawl:investigation_read --perm trawl:investigation_write >/dev/null 2>&1 || true
  $ADMIN roles create --name trawl-investigation-reader \
    --perm trawl:query --perm trawl:investigation_read >/dev/null 2>&1 || true
  $ADMIN roles create --name sender --rate-rpm 100000 --perm trawl:ingest >/dev/null 2>&1 || true
  AGENT_KEY=$($ADMIN keys create --name claude-agent --kind service --role trawl-agent --expires 90d)
  HUMAN_KEY=$($ADMIN keys create --name evidence-human --kind human --role trawl-investigation-reader --expires 12h)
  SENDER_KEY=$($ADMIN keys create --name evidence-sender --kind service --role sender --expires 12h)
  $ADMIN roles show trawl-agent > "$RUN/role-agent.txt" 2>&1
  $ADMIN roles show trawl-investigation-reader > "$RUN/role-human.txt" 2>&1
  umask 077
  printf '%s\n' "$SENDER_KEY" > "$RUN/private/sender-key"
  cat > "$RUN/private/trawld.toml" <<TOML
[server]
http_addr = "127.0.0.1:$HTTP_PORT"
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
url = "https://127.0.0.1:$HTTP_PORT"
ca_cert = "$CERT"
token = "$AGENT_KEY"

[profiles.human]
url = "https://127.0.0.1:$HTTP_PORT"
ca_cert = "$CERT"
token = "$HUMAN_KEY"

[ui]
timezone = "UTC"
TOML
  echo "stack up: trawld pid $(cat "$RUN/trawld.pid"), HOME=$RUN/home, profiles agent and human"
}

seed() {
  # One incident (seed.py): checkout 502s on nginx after a shop-api deploy
  # exhausts its database pool. NDJSON over /api/v1/ingest.
  SENDER_KEY=$(cat "$RUN/private/sender-key")
  python3 -I "$E/seed.py" "$RUN/seed.ndjson"
  curl -sS --cacert "$RUN/private/tls/cert.pem" -H "authorization: Bearer $SENDER_KEY" \
    -H 'content-type: application/x-ndjson' --data-binary @"$RUN/seed.ndjson" https://127.0.0.1:$HTTP_PORT/api/v1/ingest
  echo
}

down() {
  [ -f "$RUN/trawld.pid" ] && kill "$(cat "$RUN/trawld.pid")" 2>/dev/null && sleep 2 || true
  docker rm -f $PG_CONTAINER >/dev/null 2>&1 || true
  rm -rf "$RUN/private" "$RUN/home"
  echo "stack down"
}

case $cmd in up) up ;; seed) seed ;; down) down ;; *) echo "usage: stack.sh up|seed|down"; exit 2 ;; esac
