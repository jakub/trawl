#!/usr/bin/env bash
# Disposable evidence stack for issue #311: a branch-built trawld and
# trawl-web serving the release SPA, on a private Postgres; four keys minted
# from the operate/access.md recipe (the agent's role, the person's writer
# role, the person's read-only role, and a sender); a CLI config with the
# agent's profile in a private HOME; and the #310 incident seed.
# Everything lives under $TRAWL_EVIDENCE_DIR (default target/evidence-311)
# and is torn down by `stack.sh down`. Copied from
# docs/evidence/issue-310/stack.sh with its own container and ports, the
# writer and reader roles, and trawl-web in front of trawld.
set -euo pipefail
WT=$(git rev-parse --show-toplevel)
E=$(cd "$(dirname "$0")" && pwd)
RUN=${TRAWL_EVIDENCE_DIR:-$WT/target/evidence-311}
SPA=${TRAWL_EVIDENCE_SPA:-$WT/target/spa-evidence-311}
TARGET=${CARGO_TARGET_DIR:-$WT/target}
PG_CONTAINER=trawl-311-pg-g
PG_PORT=5451
HTTP_PORT=18460
WEB_PORT=18461
ADMIN=$TARGET/debug/fleet-admin
cmd=${1:-up}

up() {
  [ -f "$SPA/index.html" ] || { echo "no release SPA at $SPA"; exit 1; }
  mkdir -p "$RUN/private" "$RUN/home/.config/trawl" "$RUN/signals"
  # Keys and the browser's saved sessions live here. board.mjs runs in its
  # own process with the default umask, so the directory itself is closed.
  chmod 700 "$RUN/private"
  if ! docker inspect $PG_CONTAINER >/dev/null 2>&1; then
    docker run -d --name $PG_CONTAINER -p "[::1]:$PG_PORT:5432" \
      -e POSTGRES_USER=evidence -e POSTGRES_PASSWORD=evidence -e POSTGRES_DB=fleet \
      --shm-size=1g --tmpfs /var/lib/postgresql:size=512m postgres:18 -c fsync=off >/dev/null
  fi
  for i in $(seq 1 60); do docker exec $PG_CONTAINER pg_isready -h 127.0.0.1 -U evidence -d fleet >/dev/null 2>&1 && break; sleep 1; done
  docker exec $PG_CONTAINER createdb -U evidence trawl 2>/dev/null || true
  FLEET_DSN="postgres://evidence:evidence@localhost:$PG_PORT/fleet"
  APP_DSN="postgres://evidence:evidence@localhost:$PG_PORT/trawl"
  export DATABASE_URL=$FLEET_DSN
  $ADMIN migrate >/dev/null
  # The roles of operate/access.md, "Create a key for an agent", verbatim:
  # the agent's, the person's read-only role and the person's writer role.
  $ADMIN roles create --name trawl-agent --rate-rpm 60 \
    --perm trawl:query --perm trawl:schema_read --perm trawl:validate \
    --perm trawl:investigation_read --perm trawl:investigation_write >/dev/null 2>&1 || true
  $ADMIN roles create --name trawl-investigation-reader \
    --perm trawl:query --perm trawl:schema_read --perm trawl:investigation_read >/dev/null 2>&1 || true
  $ADMIN roles create --name trawl-investigation-writer \
    --perm trawl:query --perm trawl:schema_read --perm trawl:investigation_read \
    --perm trawl:investigation_write >/dev/null 2>&1 || true
  $ADMIN roles create --name sender --rate-rpm 100000 --perm trawl:ingest >/dev/null 2>&1 || true
  AGENT_KEY=$($ADMIN keys create --name claude-agent --kind service --role trawl-agent --expires 90d)
  WRITER_KEY=$($ADMIN keys create --name evidence-writer --kind human --role trawl-investigation-writer --expires 12h)
  READER_KEY=$($ADMIN keys create --name evidence-reader --kind human --role trawl-investigation-reader --expires 12h)
  SENDER_KEY=$($ADMIN keys create --name evidence-sender --kind service --role sender --expires 12h)
  for role in trawl-agent trawl-investigation-writer trawl-investigation-reader; do
    echo "\$ fleet-admin roles show $role"
    $ADMIN roles show $role
  done > "$RUN/roles.txt" 2>&1
  SESSION_KEY=$($ADMIN generate-session-key)
  umask 077
  printf '%s\n' "$SENDER_KEY" > "$RUN/private/sender-key"
  printf '%s\n' "$WRITER_KEY" > "$RUN/private/writer-key"
  printf '%s\n' "$READER_KEY" > "$RUN/private/reader-key"
  cat > "$RUN/private/trawld.toml" <<TOML
[server]
http_addr = "127.0.0.1:$HTTP_PORT"
max_result_rows = 25000
max_concurrent_queries = 2
shutdown_drain_secs = 3
link_origin = "http://127.0.0.1:$WEB_PORT"
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
  FLEET_DATABASE_URL=$FLEET_DSN TRAWL_DATABASE_URL=$APP_DSN CARGO_TARGET_DIR=$TARGET \
    nohup setsid $WT/bin/trawld-dev --config "$RUN/private/trawld.toml" > "$RUN/trawld.log" 2>&1 &
  echo $! > "$RUN/trawld.pid"
  for i in $(seq 1 60); do grep -q "HTTPS server listening" "$RUN/trawld.log" 2>/dev/null && break; sleep 1; done
  grep -m1 -o 'HTTPS server listening.*' "$RUN/trawld.log"
  CERT=$RUN/private/tls/cert.pem
  [ -f "$CERT" ] || { echo "no cert at $CERT"; ls "$RUN/private"; exit 1; }
  # trawl-web reads its limits from [server] and pins trawld's generated
  # certificate, as bin/app-experiment does.
  { cat "$RUN/private/trawld.toml"; cat <<TOML
[web]
bind_addr = "127.0.0.1:$WEB_PORT"
upstream_url = "https://127.0.0.1:$HTTP_PORT"
upstream_ca_path = "$CERT"
allow_insecure_cookies = true
public_origins = ["http://127.0.0.1:$WEB_PORT"]
cookie_secret_env = "FLEET_SESSION_AEAD_KEY"
TOML
  } > "$RUN/private/web.toml"
  FLEET_SESSION_AEAD_KEY=$SESSION_KEY TRAWL_WEB_SPA_DIR=$SPA \
    nohup setsid $TARGET/debug/trawl-web --config "$RUN/private/web.toml" > "$RUN/trawl-web.log" 2>&1 &
  echo $! > "$RUN/trawl-web.pid"
  for i in $(seq 1 60); do grep -q "trawl-web listening" "$RUN/trawl-web.log" 2>/dev/null && break; sleep 1; done
  grep -q "trawl-web listening" "$RUN/trawl-web.log" || { echo "trawl-web did not start"; tail -5 "$RUN/trawl-web.log"; exit 1; }
  cat > "$RUN/home/.config/trawl/config.toml" <<TOML
[profiles.agent]
url = "https://127.0.0.1:$HTTP_PORT"
ca_cert = "$CERT"
token = "$AGENT_KEY"

[ui]
timezone = "UTC"
TOML
  # Names, kinds, roles and lookup prefixes of the keys; never the keys.
  $ADMIN keys list > "$RUN/keys.txt" 2>&1
  echo "stack up: trawld pid $(cat "$RUN/trawld.pid"), trawl-web pid $(cat "$RUN/trawl-web.pid") on http://127.0.0.1:$WEB_PORT, HOME=$RUN/home"
}

seed() {
  # The #310 incident (../issue-310/seed.py): checkout 502s on nginx after a
  # shop-api deploy exhausts its database pool. NDJSON over /api/v1/ingest.
  SENDER_KEY=$(cat "$RUN/private/sender-key")
  python3 -I "$E/../issue-310/seed.py" "$RUN/seed.ndjson"
  curl -sS --cacert "$RUN/private/tls/cert.pem" -H "authorization: Bearer $SENDER_KEY" \
    -H 'content-type: application/x-ndjson' --data-binary @"$RUN/seed.ndjson" https://127.0.0.1:$HTTP_PORT/api/v1/ingest
  echo
}

down() {
  for name in trawl-web trawld; do
    [ -f "$RUN/$name.pid" ] && kill "$(cat "$RUN/$name.pid")" 2>/dev/null && sleep 2 || true
    rm -f "$RUN/$name.pid"
  done
  docker rm -f $PG_CONTAINER >/dev/null 2>&1 || true
  rm -rf "$RUN/private" "$RUN/home" "$RUN/signals"
  echo "stack down"
}

case $cmd in up) up ;; seed) seed ;; down) down ;; *) echo "usage: stack.sh up|seed|down"; exit 2 ;; esac
