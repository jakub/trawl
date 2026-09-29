#!/usr/bin/env bash
# Install cert-manager v1.21.1 from its checksum-verified release manifest,
# then build the leg-2 namespace: disposable PostgreSQL and the issuer chain.
set -euo pipefail
S=$HOME/trawl-270-kind-final-scratch
EV="$(cd "$(dirname "$0")" && pwd)"
NS=trawl-cm
export KUBECONFIG=$S/kubeconfig
say() { printf '\n## %s\n' "$*"; }
run() { printf '$'; printf ' %q' "$@"; printf '\n'; "$@" 2>&1; }
say "cert-manager v1.21.1, $(date -u +%FT%TZ)"
run sha256sum "$S/cert-manager.yaml"
echo "5f6a499b8c1857d57f560f536e0dcc830914b45c420899fe7ad0692c8624e408  $S/cert-manager.yaml" | sha256sum --check --strict
run kubectl --context kind-trawl-270-final apply -f "$S/cert-manager.yaml" > /dev/null
run kubectl --context kind-trawl-270-final -n cert-manager wait --for=condition=Available deployment --all --timeout=300s
run kubectl --context kind-trawl-270-final -n cert-manager get pods -o custom-columns=NAME:.metadata.name,READY:.status.containerStatuses[0].ready,IMAGE:.status.containerStatuses[0].imageID
say "namespace $NS with PostgreSQL"
run "$EV/mkdb.sh" "$NS"
say "issuer chain"
# The webhook can refuse for a few seconds after Available.
for i in $(seq 1 30); do
  if kubectl --context kind-trawl-270-final -n "$NS" apply -f "$EV/cert-manager/issuers.yaml" 2>&1; then break; fi
  sleep 2
done
run kubectl --context kind-trawl-270-final -n "$NS" wait certificate/trawl-ca --for=condition=Ready --timeout=120s
run kubectl --context kind-trawl-270-final -n "$NS" wait issuer/trawl-ca-issuer --for=condition=Ready --timeout=120s
run kubectl --context kind-trawl-270-final -n "$NS" get issuers,certificates
