#!/usr/bin/env bash
# Scratch: create namespace NS with a disposable PostgreSQL and the two DSN
# Secrets the chart needs. Passwords are generated here and never printed.
set -euo pipefail
S=$HOME/trawl-270-kind-final-scratch
NS=$1
export KUBECONFIG=$S/kubeconfig
K=(kubectl --context kind-trawl-270-final -n "$NS")
su=$(openssl rand -hex 24); fp=$(openssl rand -hex 24); tp=$(openssl rand -hex 24)
kubectl --context kind-trawl-270-final create namespace "$NS"
umask 077
cat > "$S/private/$NS-init.sql" <<SQL
CREATE ROLE fleet LOGIN PASSWORD '$fp';
CREATE ROLE trawl LOGIN PASSWORD '$tp';
CREATE DATABASE fleet OWNER fleet;
CREATE DATABASE trawl OWNER trawl;
SQL
"${K[@]}" create secret generic pg-init --from-literal=superuser-password="$su" --from-file=init.sql="$S/private/$NS-init.sql" >/dev/null
"${K[@]}" create secret generic fleet-db --from-literal=DATABASE_URL="postgres://fleet:$fp@postgres.$NS.svc.cluster.local:5432/fleet" >/dev/null
"${K[@]}" create secret generic trawl-db --from-literal=TRAWL_DATABASE_URL="postgres://trawl:$tp@postgres.$NS.svc.cluster.local:5432/trawl" >/dev/null
printf '%s\n%s\n%s\n' "$su" "$fp" "$tp" > "$S/private/$NS-secrets.txt"
echo "secrets created in $NS: pg-init, fleet-db (DATABASE_URL), trawl-db (TRAWL_DATABASE_URL)"
"${K[@]}" apply -f "$S/postgres.yaml"
"${K[@]}" rollout status deployment/postgres --timeout=180s
