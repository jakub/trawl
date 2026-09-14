#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
chart="$repo_root/chart/trawl"
work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT

render_only() {
  local template=$1
  shift
  helm template trawl "$chart" \
    --show-only "templates/${template}" \
    --set auth.database.existingSecret=fleet-db \
    --set storage.database.existingSecret=trawl-db \
    --set web.enabled=false \
    --set-string image.tag=source-render-test \
    "$@"
}

render() {
  render_only statefulset.yaml "$@"
}

# The web sidecar's own defaults: enabled, with the one setting that has
# no default (ADR-0016). Every case below that wants a working sidecar
# passes these, so a case that omits publicOrigins is omitting it on
# purpose.
web_origin="https://trawl.example.com"
web_enabled=(--set web.enabled=true --set-string "web.publicOrigins[0]=${web_origin}")

assert_render_fails() {
  local description=$1
  local expected=$2
  shift 2
  local stdout="$work_dir/fail.yaml"
  local stderr="$work_dir/fail.err"
  if "$@" >"$stdout" 2>"$stderr"; then
    echo "expected ${description} to fail the render" >&2
    exit 1
  fi
  if ! grep -Fq "$expected" "$stderr"; then
    echo "expected ${description} to fail naming '${expected}', got:" >&2
    cat "$stderr" >&2
    exit 1
  fi
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

# The securityContext block of one container in a rendered StatefulSet.
# Container and volume list entries share the same eight-space indent, so the
# scan ends at the next entry or at the next six-space key (containers: ->
# volumes:). Keys inside the securityContext sit two levels deeper, which is
# what tells them apart from the container's own sibling keys.
container_security_context() {
  local manifest=$1
  local container=$2
  awk -v want="        - name: ${container}" '
    $0 == want { inside = 1; in_sc = 0; next }
    inside && (/^        - name: / || /^      [^ ]/) { inside = 0; in_sc = 0 }
    inside && $0 == "          securityContext:" { in_sc = 1; next }
    in_sc && /^          [^ ]/ { in_sc = 0 }
    inside && in_sc { print }
  ' "$manifest"
}

# Counts the lines of one container's securityContext that match a pattern.
# The block is captured ONCE and an empty capture is a failure, not a zero: a
# renamed container, or a `name:` that stops being the entry's first line,
# would otherwise make every `expected 0` assertion pass while reading nothing.
assert_security_context_lines() {
  local expected=$1
  local pattern=$2
  local manifest=$3
  local container=$4
  local block
  block=$(container_security_context "$manifest" "$container")
  if [[ -z $block ]]; then
    echo "no securityContext block found for container '${container}' in ${manifest}" >&2
    exit 1
  fi
  local actual
  actual=$(grep -Ec -- "$pattern" <<<"$block" || true)
  if [[ $actual -ne $expected ]]; then
    echo "expected ${expected} line(s) matching '${pattern}' in the ${container} securityContext, found ${actual}:" >&2
    echo "$block" >&2
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

# -- ADR-0023 ruling 7, amended: the capability is trawld's alone ------

# The added SYS_PTRACE is the whole grant. containerd hands it to the
# container's init process as permitted and effective, and the image's
# cap_sys_ptrace+p file capability keeps the bit across trawld's exec of the
# monitor, so no_new_privs stays on: allowPrivilegeEscalation must render
# false on trawld too. The sidecars must not come along for the ride, hence a
# render with the web sidecar on: render_only defaults it off.
security_enabled="$work_dir/security-enabled.yaml"
render "${web_enabled[@]}" \
  --set persistence.enabled=true \
  --set crashDump.enabled=true \
  >"$security_enabled"
assert_security_context_lines 1 '^ +allowPrivilegeEscalation: false$' \
  "$security_enabled" trawld
assert_security_context_lines 0 '^ +allowPrivilegeEscalation: true$' \
  "$security_enabled" trawld
assert_security_context_lines 1 '^ +- SYS_PTRACE$' "$security_enabled" trawld
for sidecar in init-auth trawl-web; do
  assert_security_context_lines 1 '^ +allowPrivilegeEscalation: false$' \
    "$security_enabled" "$sidecar"
  assert_security_context_lines 0 '^ +- SYS_PTRACE$' "$security_enabled" "$sidecar"
done

# Disabled renders the shared securityContext untouched: an explicit
# allowPrivilegeEscalation: false and no added capability at all, so an
# install that never enables crash dumps stays Restricted-compatible.
security_disabled="$work_dir/security-disabled.yaml"
render "${web_enabled[@]}" \
  --set persistence.enabled=true \
  --set crashDump.enabled=false \
  >"$security_disabled"
assert_security_context_lines 1 '^ +allowPrivilegeEscalation: false$' \
  "$security_disabled" trawld
assert_security_context_lines 0 '^ +allowPrivilegeEscalation: true$' \
  "$security_disabled" trawld
assert_security_context_lines 0 '^ +- SYS_PTRACE$' "$security_disabled" trawld

# The helper appends to whatever the operator put in securityContext, so a
# capability they added survives and SYS_PTRACE is not added twice.
security_operator_caps="$work_dir/security-operator-caps.yaml"
render "${web_enabled[@]}" \
  --set persistence.enabled=true \
  --set crashDump.enabled=true \
  --set-string 'securityContext.capabilities.add[0]=NET_BIND_SERVICE' \
  >"$security_operator_caps"
assert_security_context_lines 1 '^ +- NET_BIND_SERVICE$' \
  "$security_operator_caps" trawld
assert_security_context_lines 1 '^ +- SYS_PTRACE$' "$security_operator_caps" trawld
assert_security_context_lines 1 '^ +allowPrivilegeEscalation: false$' \
  "$security_operator_caps" trawld

# -- ADR-0016: the browser-origin allowlist is stated, never derived -----

# An enabled web sidecar with no origins refuses to render, and says which
# knob to set. There is no default worth having: an empty list would be a
# proxy that 403s every browser.
assert_render_fails "web.enabled with an empty publicOrigins" \
  "web.enabled=true requires web.publicOrigins" \
  render --set web.enabled=true

# An ingress host is not an origin. It carries no scheme, TLS terminates
# wherever the operator put it, and one install answers to several names,
# so setting one must NOT satisfy the requirement.
assert_render_fails "an ingress host without publicOrigins" \
  "web.enabled=true requires web.publicOrigins" \
  render --set web.enabled=true --set ingress.enabled=true \
  --set-string 'ingress.hosts[0].host=trawl.example.com' \
  --set-string 'ingress.hosts[0].paths[0].path=/' \
  --set-string 'ingress.hosts[0].paths[0].pathType=Prefix'

# Stated origins reach both halves: the generated TOML the proxy reads,
# and the env var that survives a config.raw replacing that TOML.
origins_config="$work_dir/origins-config.yaml"
render_only configmap.yaml "${web_enabled[@]}" \
  --set-string 'web.publicOrigins[1]=http://localhost:8090' >"$origins_config"
if ! grep -Fq "public_origins = [\"${web_origin}\", \"http://localhost:8090\"]" "$origins_config"; then
  echo "expected the rendered [web] block to carry both configured origins" >&2
  exit 1
fi

origins_sts="$work_dir/origins-sts.yaml"
render "${web_enabled[@]}" \
  --set-string 'web.publicOrigins[1]=http://localhost:8090' >"$origins_sts"
assert_followed_by 'name: FLEET_SESSION_PUBLIC_ORIGINS' \
  "value: \"${web_origin},http://localhost:8090\"" "$origins_sts"

# config.raw replaces the generated TOML wholesale, so the env var is the
# only thing carrying the allowlist in that topology.
raw_sts="$work_dir/raw-sts.yaml"
render "${web_enabled[@]}" --set-string 'config.raw=[server]' >"$raw_sts"
assert_followed_by 'name: FLEET_SESSION_PUBLIC_ORIGINS' \
  "value: \"${web_origin}\"" "$raw_sts"

raw_config="$work_dir/raw-config.yaml"
render_only configmap.yaml "${web_enabled[@]}" \
  --set-string 'config.raw=[server]' >"$raw_config"
if grep -Fq 'public_origins' "$raw_config"; then
  echo "config.raw must replace the generated TOML, allowlist included" >&2
  exit 1
fi

python3 "$chart/tests/image.py"
python3 "$chart/tests/notes.py"

echo "helm render assertions passed"
