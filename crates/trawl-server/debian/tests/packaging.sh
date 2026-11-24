#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)

postinst="$repo_root/crates/trawl-server/debian/postinst"
service="$repo_root/crates/trawl-server/debian/trawld.service"
web_service="$repo_root/crates/trawl-server/debian/trawl-web.service"
default_env="$repo_root/crates/trawl-server/debian/trawld.default"
packaged_config="$repo_root/crates/trawl-server/debian/trawld.toml"
crashdump_conf="$repo_root/crates/trawl-server/debian/crashdump.conf"
tmpfiles_conf="$repo_root/crates/trawl-server/debian/trawl.tmpfiles"
sysusers_conf="$repo_root/crates/trawl-server/debian/trawl.sysusers"
cargo_toml="$repo_root/crates/trawl-server/Cargo.toml"
imp_rs="$repo_root/crates/trawl-crashdump/src/imp.rs"
values_yaml="$repo_root/chart/trawl/values.yaml"
docs_page="$repo_root/docs/src/content/docs/reference/crash-dumps.md"
access_page="$repo_root/docs/src/content/docs/operate/access.md"
asset_dest="usr/share/doc/trawl-server/examples/crashdump.conf"

fail() {
  echo "$1" >&2
  exit 1
}

# systemd ignores whitespace around a directive's '=' (systemd.syntax(7)), so
# `AmbientCapabilities = CAP_SYS_PTRACE` grants the capability.
# Only the FIRST '=' is normalized: systemd strips whitespace around the
# directive separator, not inside the value, so `Environment=A = b` sets A to
# the string "A = b" and must not be folded.
normalize_directives() { # normalize_directives <file>
  sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//; s/^([^=]*[^=[:space:]])[[:space:]]*=[[:space:]]*/\1=/' "$1"
}


if ! sh -n "$postinst" 2>/tmp/packaging-sh-n.$$; then
  msg=$(cat /tmp/packaging-sh-n.$$)
  rm -f /tmp/packaging-sh-n.$$
  fail "crates/trawl-server/debian/postinst fails 'sh -n': $msg"
fi
rm -f /tmp/packaging-sh-n.$$


if grep -E '^[[:space:]]*(AmbientCapabilities|CapabilityBoundingSet)[[:space:]]*=' "$service" >/dev/null; then
  fail "crates/trawl-server/debian/trawld.service sets AmbientCapabilities or CapabilityBoundingSet directly — the crash-dump grant belongs in the opt-in debian/crashdump.conf drop-in, not the default unit"
fi
if grep -F 'TRAWL_CRASH_DUMP_' "$service" >/dev/null; then
  fail "crates/trawl-server/debian/trawld.service mentions TRAWL_CRASH_DUMP_ — the default unit must carry no reference, commented or not"
fi


if grep -vE '^[[:space:]]*#' "$default_env" | grep -E 'TRAWL_CRASH_DUMP_[A-Za-z_]*[[:space:]]*=' >/dev/null; then
  fail "crates/trawl-server/debian/trawld.default sets a TRAWL_CRASH_DUMP_* variable uncommented — the unit's EnvironmentFile would make it part of the effective default"
fi


if ! grep -Eq '^[[:space:]]*ProtectSystem[[:space:]]*=[[:space:]]*strict[[:space:]]*$' "$service"; then
  fail "crates/trawl-server/debian/trawld.service dropped ProtectSystem=strict — the crash-dump drop-in relies on that sandbox"
fi
if ! grep -E '^[[:space:]]*ReadWritePaths[[:space:]]*=.*/var/lib/trawl' "$service" >/dev/null; then
  fail "crates/trawl-server/debian/trawld.service has no ReadWritePaths covering /var/lib/trawl — the crash-dump drop-in writes under that tree"
fi

state_modes=$(grep -Ec '^[[:space:]]*StateDirectoryMode[[:space:]]*=[[:space:]]*0750[[:space:]]*$' "$service" || true)
log_modes=$(grep -Ec '^[[:space:]]*LogsDirectoryMode[[:space:]]*=[[:space:]]*0700[[:space:]]*$' "$service" || true)
[[ "$state_modes" -eq 1 ]] || fail "trawld.service needs exactly one active StateDirectoryMode=0750"
[[ "$log_modes" -eq 1 ]] || fail "trawld.service needs exactly one active LogsDirectoryMode=0700"
for directive in StateDirectoryMode LogsDirectoryMode; do
  count=$(grep -Ec "^[[:space:]]*${directive}[[:space:]]*=" "$service" || true)
  [[ "$count" -eq 1 ]] || fail "trawld.service needs exactly one active $directive directive"
done
if grep -Eq '^[[:space:]]*(StateDirectory|LogsDirectory|StateDirectoryMode|LogsDirectoryMode)[[:space:]]*=' "$web_service"; then
  fail "trawl-web.service must not declare StateDirectory or LogsDirectory directives"
fi


if [[ ! -f "$crashdump_conf" ]]; then
  fail "crates/trawl-server/debian/crashdump.conf is missing"
fi

# Raw lines feed the docs verbatim check further down; the normalized copy is
# what gets compared and parsed, so a whitespace-around-'=' respelling is still
# recognised as the same four directives.
mapfile -t directive_lines < <(grep -vE '^[[:space:]]*$' "$crashdump_conf" | grep -vE '^[[:space:]]*#')
mapfile -t normalized_lines < <(normalize_directives "$crashdump_conf" | grep -vE '^$' | grep -vE '^#')

expected_lines=(
  "[Service]"
  "AmbientCapabilities=CAP_SYS_PTRACE"
  "Environment=TRAWL_CRASH_DUMP_DIR=/var/lib/trawl/cores"
  "Environment=TRAWL_CRASH_DUMP_RETAIN=10"
)

if [[ ${#normalized_lines[@]} -ne ${#expected_lines[@]} ]]; then
  fail "crates/trawl-server/debian/crashdump.conf has ${#normalized_lines[@]} directive lines, expected ${#expected_lines[@]}: ${expected_lines[*]}"
fi
for i in "${!expected_lines[@]}"; do
  if [[ "${normalized_lines[$i]}" != "${expected_lines[$i]}" ]]; then
    fail "crates/trawl-server/debian/crashdump.conf line $((i + 1)) is '${directive_lines[$i]}', expected '${expected_lines[$i]}'"
  fi
done


asset_pattern='\[[[:space:]]*"debian/crashdump\.conf"[[:space:]]*,[[:space:]]*"usr/share/doc/trawl-server/examples/crashdump\.conf"[[:space:]]*,[[:space:]]*"644"[[:space:]]*\]'
if ! grep -Eq "$asset_pattern" "$cargo_toml"; then
  fail "crates/trawl-server/Cargo.toml is missing the asset triple [\"debian/crashdump.conf\", \"$asset_dest\", \"644\"]"
fi
packaged_units="usr/lib/systemd/system/trawld.service usr/lib/systemd/system/trawl-web.service"
while IFS= read -r asset_line; do
  dest=$(printf '%s' "$asset_line" | sed -E 's/^[^"]*"[^"]+"[^"]*"([^"]+)".*/\1/')
  dest_norm="${dest#/}"
  case "$dest_norm" in
    etc/systemd/*|lib/systemd/*|usr/lib/systemd/*|usr/local/lib/systemd/*|run/systemd/*)
      case " $packaged_units " in
        *" $dest_norm "*) ;;
        *) fail "crates/trawl-server/Cargo.toml ships an asset to '$dest', a path systemd reads units and drop-ins from. The crash-dump example is documentation and must never be pre-installed; unit files themselves belong in [package.metadata.deb] systemd-units." ;;
      esac
      ;;
  esac
done < <(grep -E '^[[:space:]]*\[[[:space:]]*"' "$cargo_toml")

# For assets landing in sysusers.d or tmpfiles.d, cargo-deb generates a
# `systemd-sysusers <name>` / `systemd-tmpfiles --create <name>` call in postinst
# and derives <name> from the asset's SOURCE path via with_extension("conf").
generated_dirs=0
while IFS= read -r asset_line; do
  src=$(printf '%s' "$asset_line" | sed -E 's/^[^"]*"([^"]+)".*/\1/')
  dest=$(printf '%s' "$asset_line" | sed -E 's/^[^"]*"[^"]+"[^"]*"([^"]+)".*/\1/')
  src_base="${src##*/}"
  dest_base="${dest##*/}"
  derived="${src_base%.*}.conf"
  if [[ "$derived" != "$dest_base" ]]; then
    fail "crates/trawl-server/Cargo.toml asset [\"$src\", \"$dest\"]: cargo-deb derives the postinst name '$derived' from the source, but the file installs as '$dest_base'. Rename the source to '${dest_base%.conf}.<kind>' so the two agree."
  fi
  generated_dirs=$((generated_dirs + 1))
done < <(grep -E '"usr/lib/(sysusers|tmpfiles)\.d/' "$cargo_toml")

if [[ "$generated_dirs" -lt 2 ]]; then
  fail "crates/trawl-server/Cargo.toml has $generated_dirs sysusers.d/tmpfiles.d assets, expected both (the trawl user and the crash-dump directory)"
fi


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

dir_line="${normalized_lines[2]}"
retain_line="${normalized_lines[3]}"
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

if ! grep -Eq "^d=[[:space:]]+${dir_value}[[:space:]]+0700[[:space:]]+trawl[[:space:]]+trawl([[:space:]]|$)" "$tmpfiles_conf"; then
  fail "crates/trawl-server/debian/trawl.tmpfiles does not carry 'd= $dir_value 0700 trawl trawl -' (the '=' enforces the type; plain 'd' leaves a file or symlink in place)"
fi
if ! grep -Eq "^[[:space:]]*InaccessiblePaths[[:space:]]*=[[:space:]]*-?${dir_value}[[:space:]]*$" "$web_service"; then
  fail "crates/trawl-server/debian/trawl-web.service does not mask '$dir_value' with InaccessiblePaths — the third denial, after the separate uid and the 0700 mode"
fi
if grep -Eq "^[[:space:]]*InaccessiblePaths[[:space:]]*=[[:space:]]*-?/var/lib/trawl[[:space:]]*$" "$web_service"; then
  fail "crates/trawl-server/debian/trawl-web.service masks the whole of /var/lib/trawl — that hides web.cookie too and the proxy cannot start"
fi

if ! grep -Eq '^[[:space:]]*User[[:space:]]*=[[:space:]]*trawl-web[[:space:]]*$' "$web_service"; then
  fail "crates/trawl-server/debian/trawl-web.service does not set User=trawl-web — as the trawl user the proxy reaches the dumps through /proc/<trawld-pid>/root regardless of InaccessiblePaths"
fi
if ! grep -Eq '^[[:space:]]*u[[:space:]]+trawl-web[[:space:]]' "$sysusers_conf"; then
  fail "crates/trawl-server/debian/trawl.sysusers does not declare the trawl-web user that trawl-web.service runs as"
fi
if ! grep -Eq '^[[:space:]]*m[[:space:]]+trawl-web[[:space:]]+trawl[[:space:]]*$' "$sysusers_conf"; then
  fail "crates/trawl-server/debian/trawl.sysusers does not add trawl-web to the trawl group — the proxy would not be able to read /var/lib/trawl/web.cookie or /etc/trawl/trawld.toml"
fi
if ! grep -Eq '^z[[:space:]]+/var/lib/trawl/web\.cookie[[:space:]]+0640[[:space:]]+trawl[[:space:]]+trawl([[:space:]]|$)' "$tmpfiles_conf"; then
  fail "crates/trawl-server/debian/trawl.tmpfiles does not carry 'z /var/lib/trawl/web.cookie 0640 trawl trawl -' — trawl-web reads the key through the trawl group and needs it group-readable"
fi
guard_files=()
while IFS= read -r guard_file; do
  guard_files+=("$guard_file")
done < <(find "$repo_root/docs/src/content/docs" -type f -name '*.md' | sort)
for maintainer_script in preinst postinst prerm postrm; do
  if [[ -f "$repo_root/crates/trawl-server/debian/$maintainer_script" ]]; then
    guard_files+=("$repo_root/crates/trawl-server/debian/$maintainer_script")
  fi
done
if (( ${#guard_files[@]} < 2 )); then
  fail "the chown/chmod guard found ${#guard_files[@]} files to scan; the docs tree or the maintainer scripts moved and the guard is now checking nothing"
fi
for guard_file in "${guard_files[@]}"; do
  if hit=$(grep -nE '(chown|chmod)' "$guard_file" | grep -E '/var/lib/trawl/(web\.cookie|cores)' | head -3); then
    fail "${guard_file#"$repo_root/"} chowns or chmods a path under /var/lib/trawl, which the trawl user owns and can replace with a symlink that root then follows. Use the tmpfiles entry (O_NOFOLLOW) or install (replaces the destination):
$hit"
  fi
done

if [[ ! -f "$access_page" ]]; then
  fail "docs/src/content/docs/operate/access.md is missing — it carries the by-hand key install that has to agree with the packaged mode"
fi
# Mode-form agnostic on purpose: the guide installs the key rather than
# chmodding it, and the next rewrite may reach for something else again.
if ! grep -E '/var/lib/trawl/web\.cookie' "$access_page" | grep -Eq '(^|[^0-9])0640([^0-9]|$)'; then
  fail "docs/src/content/docs/operate/access.md does not put /var/lib/trawl/web.cookie in place as 0640 — trawl-web reads it through the trawl group and a key it cannot read stops the proxy starting"
fi
if grep -E '/var/lib/trawl/web\.cookie' "$access_page" | grep -Eq '(^|[^0-9])0?600([^0-9]|$)'; then
  fail "docs/src/content/docs/operate/access.md still puts the session key in place as 600 — that predates the trawl-web user split"
fi
if ! grep -Eq '^[[:space:]]*systemd-tmpfiles[[:space:]]+--create[[:space:]]+trawl\.conf' "$postinst"; then
  fail "crates/trawl-server/debian/postinst does not run 'systemd-tmpfiles --create trawl.conf' — nothing would create '$dir_value' at install time"
fi
# `.*` and not `[^\n]*`. In an ERE that bracket expression is "any character
# except backslash and n", so it stops at the 'n' in the path it is looking for
# and the guard never fires.
if grep -Eq "install[[:space:]]+-d.*${dir_value}" "$postinst"; then
  fail "crates/trawl-server/debian/postinst creates '$dir_value' with 'install -d' — that is the check-then-act race systemd-tmpfiles replaced"
fi


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


units_block=$(awk '/^[[:space:]]*systemd-units[[:space:]]*=[[:space:]]*\[/{flag=1} flag{print} flag && /^[[:space:]]*\][[:space:]]*$/{exit}' "$cargo_toml")
if [[ -z "$units_block" ]]; then
  fail "crates/trawl-server/Cargo.toml has no [package.metadata.deb] systemd-units array"
fi
unit_entries=$(grep -c 'unit-name' <<<"$units_block" || true)
if [[ "$unit_entries" -ne 2 ]]; then
  fail "crates/trawl-server/Cargo.toml systemd-units declares $unit_entries units, expected exactly trawld and trawl-web"
fi
for unit in trawld trawl-web; do
  entry=$(grep -E "unit-name[[:space:]]*=[[:space:]]*\"${unit}\"" <<<"$units_block" || true)
  if [[ -z "$entry" ]]; then
    fail "crates/trawl-server/Cargo.toml systemd-units does not declare '$unit'"
  fi
  if ! grep -Eq 'enable[[:space:]]*=[[:space:]]*false' <<<"$entry" || grep -Eq 'enable[[:space:]]*=[[:space:]]*true' <<<"$entry"; then
    fail "crates/trawl-server/Cargo.toml systemd-units entry for '$unit' must say enable = false: a fresh install would enable a unit whose config still holds CHANGE_ME database URLs"
  fi
  if ! grep -Eq 'start[[:space:]]*=[[:space:]]*false' <<<"$entry" || grep -Eq 'start[[:space:]]*=[[:space:]]*true' <<<"$entry"; then
    fail "crates/trawl-server/Cargo.toml systemd-units entry for '$unit' must say start = false: a fresh install would start a unit whose config still holds CHANGE_ME database URLs"
  fi
done
if ! grep -Fq 'systemctl enable --now trawld trawl-web' "$postinst"; then
  fail "crates/trawl-server/debian/postinst does not tell the operator to run 'systemctl enable --now trawld trawl-web' after setting the database URLs"
fi


web_section=$(awk '/^\[web\][[:space:]]*$/{flag=1; next} flag && /^\[/{flag=0} flag' "$packaged_config")
if [[ -z "$web_section" ]]; then
  fail "crates/trawl-server/debian/trawld.toml has no [web] section"
fi
if ! grep -Eq '^upstream_ca_path[[:space:]]*=[[:space:]]*"[^"]+"' <<<"$web_section"; then
  fail "crates/trawl-server/debian/trawld.toml does not set [web] upstream_ca_path uncommented: trawl-web would check trawld's self-signed certificate against the platform roots and every sign-in would fail"
fi
# Split so this file does not match its own search.
removed_switch='TRAWL_WEB_''INSECURE_UPSTREAM'
if naming=$(cd "$repo_root" && grep -rlF -- "$removed_switch" crates/trawl-server/debian); then
  fail "these files under crates/trawl-server/debian name the removed $removed_switch switch: ${naming//$'\n'/, }"
fi


assert_one_directive() { # assert_one_directive <unit> <name> <directive> <value>
  local unit=$1 name=$2 directive=$3 value=$4 active
  active=$(normalize_directives "$unit" | grep -E "^${directive}=" || true)
  if [[ "$active" != "${directive}=${value}" ]]; then
    fail "${name} needs exactly one active ${directive}=${value}, found: ${active:-none}"
  fi
}
assert_one_directive "$service" trawld.service LimitNOFILE 65536
assert_one_directive "$service" trawld.service MemoryMax 8G
assert_one_directive "$web_service" trawl-web.service MemoryMax 512M

echo "debian packaging assertions passed"
