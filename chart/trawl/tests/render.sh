#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
chart="$repo_root/chart/trawl"
work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT

render() {
  helm template trawl "$chart" \
    --show-only templates/statefulset.yaml \
    --set auth.database.existingSecret=fleet-db \
    --set storage.database.existingSecret=trawl-db \
    --set web.enabled=false \
    "$@"
}

assert_name_count() {
  local expected=$1
  local name=$2
  local manifest=$3
  local actual
  actual=$(grep -Ec "^[[:space:]]*(- )?name: ${name}$" "$manifest" || true)
  if [[ $actual -ne $expected ]]; then
    echo "expected ${expected} '${name}' mount/claim entries, found ${actual}" >&2
    exit 1
  fi
}

assert_followed_by() {
  local anchor=$1
  local expected=$2
  local manifest=$3
  local following
  following=$(grep -A1 -m1 -F -- "$anchor" "$manifest" | tail -n1)
  if [[ $following != *"$expected"* ]]; then
    echo "expected '${anchor}' to be followed by '${expected}', found '${following}'" >&2
    exit 1
  fi
}

assert_claim_template() {
  local name=$1
  local storage_class=$2
  local size=$3
  local manifest=$4
  if ! awk -v name="$name" -v storage_class="$storage_class" -v size="$size" '
    /^  volumeClaimTemplates:$/ { in_templates = 1; next }
    in_templates && /^  [[:alnum:]_][[:alnum:]_-]*:/ { exit }
    in_templates && /^    - metadata:$/ { in_claim = 0 }
    in_templates && $0 == "        name: " name { in_claim = 1; found_name = 1 }
    in_claim && $0 == "          - ReadWriteOnce" { found_mode = 1 }
    in_claim && $0 == "        storageClassName: \"" storage_class "\"" { found_class = 1 }
    in_claim && $0 == "            storage: " size { found_size = 1 }
    END { exit !(found_name && found_mode && found_class && found_size) }
  ' "$manifest"; then
    echo "expected volumeClaimTemplates entry '${name}' with ReadWriteOnce, storage class '${storage_class}', and size '${size}'" >&2
    exit 1
  fi
}

enabled="$work_dir/enabled.yaml"
render \
  --set persistence.enabled=true \
  --set crashDump.enabled=true \
  --set-string crashDump.mountPath=/var/lib/trawl/test-cores \
  --set crashDump.retain=7 \
  --set crashDump.storageClass=test-cores \
  --set crashDump.size=3Gi \
  >"$enabled"
assert_followed_by 'name: TRAWL_CRASH_DUMP_DIR' 'value: "/var/lib/trawl/test-cores"' "$enabled"
assert_followed_by 'name: TRAWL_CRASH_DUMP_RETAIN' 'value: "7"' "$enabled"
assert_followed_by '- name: cores' 'mountPath: "/var/lib/trawl/test-cores"' "$enabled"
assert_name_count 2 data "$enabled"
assert_name_count 2 cores "$enabled"
assert_claim_template cores test-cores 3Gi "$enabled"

invalid_stdout="$work_dir/invalid.yaml"
invalid_stderr="$work_dir/invalid.err"
if render --set persistence.enabled=false --set crashDump.enabled=true \
  >"$invalid_stdout" 2>"$invalid_stderr"; then
  echo "expected crashDump.enabled=true with persistence.enabled=false to fail" >&2
  exit 1
fi
grep -Fq 'crashDump.enabled=true requires persistence.enabled=true' "$invalid_stderr"

disabled="$work_dir/disabled.yaml"
render --set persistence.enabled=true --set crashDump.enabled=false >"$disabled"
if grep -Fq 'TRAWL_CRASH_DUMP_' "$disabled" || grep -Eq '^[[:space:]]*(- )?name: cores$' "$disabled"; then
  echo "crashDump.enabled=false rendered crash-dump environment or storage" >&2
  exit 1
fi

echo "helm render assertions passed"
