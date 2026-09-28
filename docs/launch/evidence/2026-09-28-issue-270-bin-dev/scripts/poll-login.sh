#!/usr/bin/env bash
# Poll POST /api/auth/login on the dev trawl-web backend (:8090) until 200.
# Logs a line per status change: UTC time, HTTP status, response body with any
# key or cookie value redacted. The key is read from the fleet-dev state file
# each round and never printed.
set -u
key_file="$HOME/.local/state/fleet/docker/dev-api-key"
deadline=$(( $(date +%s) + ${1:-540} ))
last=""
tls_seen=0
while (( $(date +%s) < deadline )); do
  if (( ! tls_seen )) && [[ -e "$HOME/.trawl/tls/cert.pem" ]]; then
    echo "$(date -u +%T.%3N) cert.pem now exists: $(stat -c '%n mode=%a birth=%w' "$HOME/.trawl/tls/cert.pem")"
    tls_seen=1
  fi
  if [[ -r "$key_file" ]]; then
    body=$(jq -cn --rawfile k "$key_file" '{api_key: ($k | rtrimstr("\n"))}')
    out=$(curl -sS -m 5 -o /tmp/j270c9b/poll-body -w '%{http_code}' \
      -H 'Content-Type: application/json' -H 'Origin: http://localhost:8081' \
      --data-binary "$body" http://127.0.0.1:8090/api/auth/login 2>/dev/null)
    status=${out:-000}
  else
    status="nokeyfile"
  fi
  if [[ "$status" != "$last" ]]; then
    shown=$( [[ -f /tmp/j270c9b/poll-body ]] && head -c 300 /tmp/j270c9b/poll-body | sed -E 's/(api_key|token)"[^,}]*/\1":"<redacted>"/g' )
    echo "$(date -u +%T.%3N) POST /api/auth/login -> $status ${shown}"
    last="$status"
  fi
  rm -f /tmp/j270c9b/poll-body
  [[ "$status" == 200 ]] && exit 0
  sleep 0.5
done
echo "$(date -u +%T.%3N) deadline reached, last status $last"
exit 1
