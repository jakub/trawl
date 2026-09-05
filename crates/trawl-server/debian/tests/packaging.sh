#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)

postinst="$repo_root/crates/trawl-server/debian/postinst"
service="$repo_root/crates/trawl-server/debian/trawld.service"
default_env="$repo_root/crates/trawl-server/debian/trawld.default"
crashdump_conf="$repo_root/crates/trawl-server/debian/crashdump.conf"
cargo_toml="$repo_root/crates/trawl-server/Cargo.toml"
imp_rs="$repo_root/crates/trawl-crashdump/src/imp.rs"
values_yaml="$repo_root/chart/trawl/values.yaml"
docs_page="$repo_root/docs/src/content/docs/reference/crash-dumps.md"
asset_dest="usr/share/doc/trawl-server/examples/crashdump.conf"

fail() {
  echo "$1" >&2
  exit 1
}

# -- 1. postinst is valid POSIX sh -----------------------------------------

if ! sh -n "$postinst" 2>/tmp/packaging-sh-n.$$; then
  msg=$(cat /tmp/packaging-sh-n.$$)
  rm -f /tmp/packaging-sh-n.$$
  fail "crates/trawl-server/debian/postinst fails 'sh -n': $msg"
fi
rm -f /tmp/packaging-sh-n.$$

# -- 2. the default unit stays untouched: no active capability grant, and -
#       no mention of the crash-dump env vars anywhere ---------------------

if grep -E '^[[:space:]]*(AmbientCapabilities|CapabilityBoundingSet)=' "$service" >/dev/null; then
  fail "crates/trawl-server/debian/trawld.service sets AmbientCapabilities or CapabilityBoundingSet directly — the crash-dump grant belongs in the opt-in debian/crashdump.conf drop-in, not the default unit"
fi
if grep -F 'TRAWL_CRASH_DUMP_' "$service" >/dev/null; then
  fail "crates/trawl-server/debian/trawld.service mentions TRAWL_CRASH_DUMP_ — the default unit must carry no reference, commented or not"
fi

# -- 3. the default env file carries no active crash-dump assignment ------

if grep -vE '^[[:space:]]*#' "$default_env" | grep -E 'TRAWL_CRASH_DUMP_[A-Za-z_]*=' >/dev/null; then
  fail "crates/trawl-server/debian/trawld.default sets a TRAWL_CRASH_DUMP_* variable uncommented — the unit's EnvironmentFile would make it part of the effective default"
fi

# -- 4. the sandboxing the drop-in depends on is still in place -----------

if ! grep -Fq 'ProtectSystem=strict' "$service"; then
  fail "crates/trawl-server/debian/trawld.service dropped ProtectSystem=strict — the crash-dump drop-in relies on that sandbox"
fi
if ! grep -E '^ReadWritePaths=.*/var/lib/trawl' "$service" >/dev/null; then
  fail "crates/trawl-server/debian/trawld.service has no ReadWritePaths covering /var/lib/trawl — the crash-dump drop-in writes under that tree"
fi

# -- 5. crashdump.conf carries exactly the four directive lines, in order -

if [[ ! -f "$crashdump_conf" ]]; then
  fail "crates/trawl-server/debian/crashdump.conf is missing"
fi

mapfile -t directive_lines < <(grep -vE '^[[:space:]]*$' "$crashdump_conf" | grep -vE '^[[:space:]]*#')

expected_lines=(
  "[Service]"
  "AmbientCapabilities=CAP_SYS_PTRACE"
  "Environment=TRAWL_CRASH_DUMP_DIR=/var/lib/trawl/cores"
  "Environment=TRAWL_CRASH_DUMP_RETAIN=10"
)

if [[ ${#directive_lines[@]} -ne ${#expected_lines[@]} ]]; then
  fail "crates/trawl-server/debian/crashdump.conf has ${#directive_lines[@]} directive lines, expected ${#expected_lines[@]}: ${expected_lines[*]}"
fi
for i in "${!expected_lines[@]}"; do
  if [[ "${directive_lines[$i]}" != "${expected_lines[$i]}" ]]; then
    fail "crates/trawl-server/debian/crashdump.conf line $((i + 1)) is '${directive_lines[$i]}', expected '${expected_lines[$i]}'"
  fi
done

# -- 6. the Cargo.toml asset ships the example as documentation only ------

asset_pattern='\[[[:space:]]*"debian/crashdump\.conf"[[:space:]]*,[[:space:]]*"usr/share/doc/trawl-server/examples/crashdump\.conf"[[:space:]]*,[[:space:]]*"644"[[:space:]]*\]'
if ! grep -Eq "$asset_pattern" "$cargo_toml"; then
  fail "crates/trawl-server/Cargo.toml is missing the asset triple [\"debian/crashdump.conf\", \"$asset_dest\", \"644\"]"
fi
if grep -E '"etc/systemd/system/|"usr/lib/systemd/system/trawld\.service\.d' "$cargo_toml" >/dev/null; then
  fail "crates/trawl-server/Cargo.toml ships an asset under a systemd unit directory (etc/systemd/system/ or usr/lib/systemd/system/trawld.service.d/) — the crash-dump example must ship as documentation only, never pre-installed"
fi

# -- 7. drift guard: the drop-in's env var names and retain default track -
#       the crate's own constants, not a hand-copied literal -------------

dir_env=$(grep -oE 'const DIR_ENV: &str = "[^"]*"' "$imp_rs" | sed -E 's/.*"([^"]*)".*/\1/')
retain_env=$(grep -oE 'const RETAIN_ENV: &str = "[^"]*"' "$imp_rs" | sed -E 's/.*"([^"]*)".*/\1/')
default_retain=$(grep -oE 'const DEFAULT_RETAIN: usize = [0-9]+' "$imp_rs" | grep -oE '[0-9]+$')

if [[ -z "$dir_env" ]]; then
  fail "crates/trawl-crashdump/src/imp.rs: could not find the DIR_ENV constant"
fi
if [[ -z "$retain_env" ]]; then
  fail "crates/trawl-crashdump/src/imp.rs: could not find the RETAIN_ENV constant"
fi
if [[ -z "$default_retain" ]]; then
  fail "crates/trawl-crashdump/src/imp.rs: could not find the DEFAULT_RETAIN constant"
fi

dir_line="${directive_lines[2]}"
retain_line="${directive_lines[3]}"
dir_rest="${dir_line#Environment=}"
dir_name="${dir_rest%%=*}"
dir_value="${dir_rest#*=}"
retain_rest="${retain_line#Environment=}"
retain_name="${retain_rest%%=*}"
retain_value="${retain_rest#*=}"

if [[ "$dir_name" != "$dir_env" ]]; then
  fail "crates/trawl-server/debian/crashdump.conf names the dir variable '$dir_name', but crates/trawl-crashdump/src/imp.rs's DIR_ENV is '$dir_env'"
fi
if [[ "$retain_name" != "$retain_env" ]]; then
  fail "crates/trawl-server/debian/crashdump.conf names the retain variable '$retain_name', but crates/trawl-crashdump/src/imp.rs's RETAIN_ENV is '$retain_env'"
fi
if [[ "$retain_value" != "$default_retain" ]]; then
  fail "crates/trawl-server/debian/crashdump.conf sets the retain default to '$retain_value', but crates/trawl-crashdump/src/imp.rs's DEFAULT_RETAIN is '$default_retain'"
fi

# -- 8. cross-channel parity: deb, chart and postinst agree on the path ---

chart_block=$(awk '/^crashDump:$/{flag=1; next} flag && /^[^[:space:]]/{flag=0} flag' "$values_yaml")
chart_mount=$(echo "$chart_block" | grep -E '^[[:space:]]*mountPath:' | sed -E 's/^[[:space:]]*mountPath:[[:space:]]*//' | head -1)
chart_retain=$(echo "$chart_block" | grep -E '^[[:space:]]*retain:' | sed -E 's/^[[:space:]]*retain:[[:space:]]*//' | head -1)

if [[ -z "$chart_mount" ]]; then
  fail "chart/trawl/values.yaml: could not find the crashDump.mountPath default"
fi
if [[ -z "$chart_retain" ]]; then
  fail "chart/trawl/values.yaml: could not find the crashDump.retain default"
fi
if [[ "$chart_mount" != "$dir_value" ]]; then
  fail "crates/trawl-server/debian/crashdump.conf's TRAWL_CRASH_DUMP_DIR ('$dir_value') disagrees with chart/trawl/values.yaml's crashDump.mountPath default ('$chart_mount')"
fi
if [[ "$chart_retain" != "$retain_value" ]]; then
  fail "crates/trawl-server/debian/crashdump.conf's TRAWL_CRASH_DUMP_RETAIN ('$retain_value') disagrees with chart/trawl/values.yaml's crashDump.retain default ('$chart_retain')"
fi

if ! grep -F -- "$dir_value" "$postinst" | grep -Fq -- '-o trawl -g trawl'; then
  fail "crates/trawl-server/debian/postinst does not create '$dir_value' with -o trawl -g trawl"
fi

# -- 9. the docs page documents the drop-in verbatim -----------------------

if [[ ! -f "$docs_page" ]]; then
  fail "docs/src/content/docs/reference/crash-dumps.md is missing — the crash-dump docs page has not landed yet"
fi
if ! grep -Fq -- "${directive_lines[1]}" "$docs_page"; then
  fail "docs/src/content/docs/reference/crash-dumps.md does not quote '${directive_lines[1]}' verbatim"
fi
if ! grep -Fq -- "${directive_lines[2]}" "$docs_page"; then
  fail "docs/src/content/docs/reference/crash-dumps.md does not quote '${directive_lines[2]}' verbatim"
fi
if ! grep -Fq -- "${directive_lines[3]}" "$docs_page"; then
  fail "docs/src/content/docs/reference/crash-dumps.md does not quote '${directive_lines[3]}' verbatim"
fi
if ! grep -Fq -- "$asset_dest" "$docs_page"; then
  fail "docs/src/content/docs/reference/crash-dumps.md does not mention '$asset_dest'"
fi

echo "debian crash-dump packaging assertions passed"
