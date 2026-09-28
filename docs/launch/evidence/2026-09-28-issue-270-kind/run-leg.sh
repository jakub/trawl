#!/usr/bin/env bash
# One leg of the #270 kind proof: fresh Helm install into namespace NS with
# LEG/values.yaml, then pod status, sidecar trust material, and a sign-in
# through trawl-web. NS must already hold the disposable PostgreSQL and the
# two DSN Secrets from mkdb.sh. Secrets (API key, cookie) go to the private
# scratch directory only; this script never prints them.
#
#   run-leg.sh LEG NS     # LEG is auto or cert-manager, a directory beside this script
set -euo pipefail
LEG=$1
NS=$2
EV="$(cd "$(dirname "$0")" && pwd)/$LEG"
S=$HOME/trawl-270-kind-scratch
PRIV=$S/private/$LEG
mkdir -p "$PRIV"
chmod 700 "$PRIV"
export KUBECONFIG=$S/kubeconfig
K=(kubectl --context kind-trawl-270 -n "$NS")
T=$EV/transcript.log
: > "$T"

say() { printf '\n## %s\n' "$*" | tee -a "$T"; }
# Echo the literal command, then run it; stdout and stderr both land in the
# transcript. Commands that touch a secret redirect it before reaching here.
run() { { printf '$'; printf ' %q' "$@"; printf '\n'; } | tee -a "$T"; "$@" 2>&1 | tee -a "$T"; }
# Container logs are saved without their ANSI colour codes.
nocolor() { sed -E $'s/\x1b\\[[0-9;]*m//g'; }
runsh() { printf '$ %s\n' "$1" | tee -a "$T"; bash -c "$1" 2>&1 | tee -a "$T"; }

say "leg $LEG in namespace $NS, $(date -u +%FT%TZ)"
run helm version --short
run kubectl --context kind-trawl-270 version

say "fresh install"
run helm install trawl "$S/src/chart/trawl" --namespace "$NS" -f "$EV/values.yaml"
start=$(date +%s)

say "pod status every 3s until both containers are Ready, then 60s more"
fmt='{.status.phase}{" "}{range .status.containerStatuses[*]}{.name}{":ready="}{.ready}{",restarts="}{.restartCount}{",state="}{.state}{" "}{end}'
ready_since=0
for _ in $(seq 1 120); do
  line=$("${K[@]}" get pod trawl-0 -o jsonpath="$fmt" 2>&1 || true)
  printf '+%03ds %s\n' $(( $(date +%s) - start )) "$line" | tee -a "$T"
  if [[ $line == *"trawl-web:ready=true"* && $line == *"trawld:ready=true"* ]]; then
    (( ready_since == 0 )) && ready_since=$(date +%s)
    (( $(date +%s) - ready_since >= 60 )) && break
  else
    ready_since=0
  fi
  sleep 3
done
run "${K[@]}" get pods -o wide
runsh "kubectl --context kind-trawl-270 -n $NS get pod trawl-0 -o json | jq '[.status.containerStatuses[] | {name, ready, restartCount, image, imageID, state, lastState}]'"
runsh "kubectl --context kind-trawl-270 -n $NS get events --field-selector involvedObject.name=trawl-0 --sort-by=.lastTimestamp -o custom-columns=TYPE:.type,REASON:.reason,MESSAGE:.message"

if [[ $LEG == cert-manager ]]; then
  say "trawld's certificate from the CA Issuer"
  run "${K[@]}" wait certificate/trawl-tls --for=condition=Ready --timeout=120s
  run "${K[@]}" get certificate trawl-tls -o jsonpath='{.spec.dnsNames}{"\n"}{.spec.issuerRef}{"\n"}'
  runsh "kubectl --context kind-trawl-270 -n $NS get secret trawl-tls -o json | jq -c '{type, keys: (.data | keys)}'"
fi

say "rendered pod spec: security contexts, trawl-web mounts, volumes"
"${K[@]}" get statefulset trawl -o json | jq '.spec.template.spec | {podSecurityContext: .securityContext, containers: [.containers[] | {name, securityContext, volumeMounts}], volumes}' > "$EV/pod-spec.json"
echo "(written to $LEG/pod-spec.json)" | tee -a "$T"
"${K[@]}" get configmap trawl -o jsonpath='{.data.trawld\.toml}' > "$EV/trawld.toml"
runsh "sed -n '/^\[web\]/,/^\[/p' '$EV/trawld.toml'"

say "container logs"
"${K[@]}" logs trawl-0 -c trawl-web | nocolor > "$EV/trawl-web.log"
"${K[@]}" logs trawl-0 -c trawld | nocolor > "$EV/trawld.log"
"${K[@]}" logs trawl-0 -c init-auth | nocolor > "$EV/init-auth.log"
runsh "cat '$EV/trawl-web.log'"
runsh "grep -E 'tls|TLS|certificate|listening' '$EV/trawld.log' | head -20"

say "what each container can see"
run "${K[@]}" exec trawl-0 -c trawld -- id
run "${K[@]}" exec trawl-0 -c trawl-web -- id
if [[ $LEG == auto ]]; then
  run "${K[@]}" exec trawl-0 -c trawld -- ls -lnd /var/lib/trawl /var/lib/trawl/tls /var/lib/trawl/tls-key
  run "${K[@]}" exec trawl-0 -c trawld -- ls -ln /var/lib/trawl/tls /var/lib/trawl/tls-key
  run "${K[@]}" exec trawl-0 -c trawl-web -- ls -lnd /var/lib/trawl/tls
  run "${K[@]}" exec trawl-0 -c trawl-web -- ls -ln /var/lib/trawl/tls
  run "${K[@]}" exec trawl-0 -c trawl-web -- ls -ln /var/lib/trawl || true
  run "${K[@]}" exec trawl-0 -c trawl-web -- cat /var/lib/trawl/tls-key/key.pem || true
  run "${K[@]}" exec trawl-0 -c trawl-web -- cat /var/lib/trawl/tls/key.pem || true
  run "${K[@]}" exec trawl-0 -c trawl-web -- sh -c 'touch /var/lib/trawl/tls/probe' || true
  runsh "kubectl --context kind-trawl-270 -n $NS exec trawl-0 -c trawl-web -- cat /var/lib/trawl/tls/cert.pem | openssl x509 -noout -subject -issuer -ext subjectAltName -fingerprint -sha256"
  runsh "kubectl --context kind-trawl-270 -n $NS exec trawl-0 -c trawld -- cat /var/lib/trawl/tls/cert.pem | openssl x509 -noout -fingerprint -sha256"
else
  run "${K[@]}" exec trawl-0 -c trawld -- ls -lnL /etc/trawl/tls
  run "${K[@]}" exec trawl-0 -c trawl-web -- ls -lnL /etc/trawl/upstream-ca
  run "${K[@]}" exec trawl-0 -c trawl-web -- ls -ln /etc/trawl
  run "${K[@]}" exec trawl-0 -c trawl-web -- cat /etc/trawl/tls/tls.key || true
  run "${K[@]}" exec trawl-0 -c trawl-web -- cat /etc/trawl/upstream-ca/tls.key || true
  runsh "kubectl --context kind-trawl-270 -n $NS exec trawl-0 -c trawl-web -- cat /etc/trawl/upstream-ca/ca.crt | openssl x509 -noout -subject -issuer -fingerprint -sha256"
  runsh "kubectl --context kind-trawl-270 -n $NS exec trawl-0 -c trawld -- cat /etc/trawl/tls/tls.crt | openssl x509 -noout -subject -issuer -ext subjectAltName"
fi
run "${K[@]}" exec trawl-0 -c trawl-web -- stat -L -c '%n mode=%a uid=%u gid=%g' /etc/trawl/web.cookie /etc/trawl/trawld.toml
run "${K[@]}" exec trawl-0 -c trawl-web -- sh -c 'test -r /etc/trawl/web.cookie && echo "cookie key readable by uid $(id -u)"'

say "a key for the sign-in (token to the private directory only)"
run "${K[@]}" exec trawl-0 -c trawld -- sh -c 'DATABASE_URL="$FLEET_DATABASE_URL" exec fleet-admin roles create --name trawl-reader --perm trawl:query --perm trawl:schema_read --perm trawl:validate --perm trawl:export --perm trawl:stream --perm trawl:saved_query --perm trawl:query_cancel'
printf '$ %s\n' "kubectl -n $NS exec trawl-0 -c trawld -- sh -c 'DATABASE_URL=\"\$FLEET_DATABASE_URL\" exec fleet-admin keys create --name kind-proof --kind human --role trawl-reader' > \$PRIV/token" | tee -a "$T"
( umask 077
  "${K[@]}" exec trawl-0 -c trawld -- sh -c 'DATABASE_URL="$FLEET_DATABASE_URL" exec fleet-admin keys create --name kind-proof --kind human --role trawl-reader' > "$PRIV/token" 2> "$PRIV/keys-create.stderr"
  tee -a "$T" < "$PRIV/keys-create.stderr"
  jq -n --rawfile k "$PRIV/token" '{api_key: ($k | rtrimstr("\n"))}' > "$PRIV/login.json" )
sleep 1
echo "token bytes: $(wc -c < "$PRIV/token")" | tee -a "$T"

say "sign-in through trawl-web (port-forward svc/trawl 8090)"
"${K[@]}" port-forward svc/trawl 8090:8090 > "$PRIV/port-forward.log" 2>&1 &
pf=$!
trap 'kill $pf 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do curl -fsS -o /dev/null http://localhost:8090/healthz 2>/dev/null && break; sleep 0.2; done
redact() { sed -E 's/(fleet_session=)[^;[:space:]]+/\1<redacted>/g'; }
O='Origin: http://localhost:8090'
run curl -sS -o /dev/null -w 'GET /healthz -> %{http_code}\n' http://localhost:8090/healthz
run curl -sS -o /dev/null -w 'GET / -> %{http_code} %{content_type}\n' http://localhost:8090/
run curl -sS -w '\n-> %{http_code}\n' -H "$O" http://localhost:8090/api/v1/whoami
printf '$ %s\n' "curl -sS -D - -H '$O' -H 'Content-Type: application/json' --data @\$PRIV/login.json -c \$PRIV/cookies http://localhost:8090/api/auth/login | redact" | tee -a "$T"
curl -sS -D - -w '\n-> %{http_code}\n' -H "$O" -H 'Content-Type: application/json' --data @"$PRIV/login.json" -c "$PRIV/cookies" http://localhost:8090/api/auth/login | redact | tee -a "$T"
printf '$ %s\n' "curl -sS -b \$PRIV/cookies -H '$O' http://localhost:8090/api/auth/me" | tee -a "$T"
curl -sS -w '\n-> %{http_code}\n' -b "$PRIV/cookies" -H "$O" http://localhost:8090/api/auth/me | tee -a "$T"
printf '$ %s\n' "curl -sS -b \$PRIV/cookies -H '$O' http://localhost:8090/api/v1/whoami" | tee -a "$T"
curl -sS -w '\n-> %{http_code}\n' -b "$PRIV/cookies" -H "$O" http://localhost:8090/api/v1/whoami | tee -a "$T"
printf '$ %s\n' "curl -sS -o \$PRIV/schema.json -b \$PRIV/cookies -H '$O' http://localhost:8090/api/v1/schema; jq -c keys \$PRIV/schema.json" | tee -a "$T"
curl -sS -o "$PRIV/schema.json" -w '-> %{http_code}\n' -b "$PRIV/cookies" -H "$O" http://localhost:8090/api/v1/schema | tee -a "$T"
jq -c 'if type == "object" then keys else type end' "$PRIV/schema.json" | tee -a "$T"
kill $pf 2>/dev/null || true
wait $pf 2>/dev/null || true
trap - EXIT

say "after the sign-in"
run "${K[@]}" get pod trawl-0 -o jsonpath='{range .status.containerStatuses[*]}{.name}{" ready="}{.ready}{" restarts="}{.restartCount}{"\n"}{end}'
"${K[@]}" logs trawl-0 -c trawl-web | nocolor > "$EV/trawl-web.log"
runsh "tail -n 5 '$EV/trawl-web.log'"
say "done $(date -u +%FT%TZ)"
