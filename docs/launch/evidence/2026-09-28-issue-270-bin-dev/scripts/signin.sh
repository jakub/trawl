#!/usr/bin/env bash
# Sign in through the dev origin and make authenticated requests.
# Redacts the fleet-dev API key and the fleet_session cookie value.
set -u
S=/tmp/j270c9b; JAR=$S/jar.txt; rm -f $JAR
ORIGIN=http://localhost:8081
redact() { sed -E -e 's/flt_[A-Za-z0-9_-]+/flt_<redacted>/g' -e 's/(fleet_session=)[^;[:space:]]+/\1<redacted>/g' -e 's/\r$//'; }
key_file="$HOME/.local/state/fleet/docker/dev-api-key"
run() { echo; echo "\$ $1"; }

run "curl -sS -o /dev/null -w '%{http_code} %{content_type}\n' $ORIGIN/login"
curl -sS -o /dev/null -w '%{http_code} %{content_type}\n' $ORIGIN/login

run "curl -sS -i $ORIGIN/api/auth/me    # before sign-in"
curl -sS -i $ORIGIN/api/auth/me | redact; echo

run "jq -cn --rawfile k ~/.local/state/fleet/docker/dev-api-key '{api_key: (\$k|rtrimstr(\"\\n\"))}' \\
  | curl -sS -i -c \$JAR -H 'Content-Type: application/json' -H 'Origin: $ORIGIN' --data-binary @- $ORIGIN/api/auth/login"
jq -cn --rawfile k "$key_file" '{api_key: ($k|rtrimstr("\n"))}' \
  | curl -sS -i -c $JAR -H 'Content-Type: application/json' -H "Origin: $ORIGIN" --data-binary @- $ORIGIN/api/auth/login | redact; echo

run "curl -sS -i -b \$JAR $ORIGIN/api/auth/me"
curl -sS -i -b $JAR $ORIGIN/api/auth/me | redact; echo

run "curl -sS -i -b \$JAR $ORIGIN/api/v1/whoami"
curl -sS -i -b $JAR $ORIGIN/api/v1/whoami | redact; echo

Q='service=trawld last=1h "TLS certificate" | fields _time, message, cert, key | sort _time'
run "curl -sS -i -b \$JAR -H 'Content-Type: application/json' -H 'Origin: $ORIGIN' \\
  -d '{\"query\": \"$(printf %s "$Q" | sed 's/"/\\\\"/g')\", \"limit\": 20}' $ORIGIN/api/v1/query"
jq -cn --arg q "$Q" '{query: $q, limit: 20}' \
  | curl -sS -i -b $JAR -H 'Content-Type: application/json' -H "Origin: $ORIGIN" --data-binary @- $ORIGIN/api/v1/query | redact; echo
