#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)

postinst="$repo_root/crates/trawl-server/debian/postinst"
service="$repo_root/crates/trawl-server/debian/trawld.service"
web_service="$repo_root/crates/trawl-server/debian/trawl-web.service"
default_env="$repo_root/crates/trawl-server/debian/trawld.default"
crashdump_conf="$repo_root/crates/trawl-server/debian/crashdump.conf"
tmpfiles_conf="$repo_root/crates/trawl-server/debian/trawl.tmpfiles"
sysusers_conf="$repo_root/crates/trawl-server/debian/trawl.sysusers"
cargo_toml="$repo_root/crates/trawl-server/Cargo.toml"
imp_rs="$repo_root/crates/trawl-crashdump/src/imp.rs"
values_yaml="$repo_root/chart/trawl/values.yaml"
docs_page="$repo_root/docs/src/content/docs/reference/crash-dumps.md"
cutover_page="$repo_root/docs/src/content/docs/reference/fleet-auth-cutover.md"
asset_dest="usr/share/doc/trawl-server/examples/crashdump.conf"

fail() {
  echo "$1" >&2
  exit 1
}

# systemd ignores whitespace around a directive's '=' (systemd.syntax(7)), so
# `AmbientCapabilities = CAP_SYS_PTRACE` grants the capability just as surely as
# the unspaced form. Every check below that looks for a directive has to see
# both spellings, or the guard passes on the file it was written to catch.
# Only the FIRST '=' is normalized: systemd strips whitespace around the
# directive separator, not inside the value, so `Environment=A = b` sets A to
# the string "A = b" and must not be folded.
normalize_directives() { # normalize_directives <file>
  sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//; s/^([^=]*[^=[:space:]])[[:space:]]*=[[:space:]]*/\1=/' "$1"
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

if grep -E '^[[:space:]]*(AmbientCapabilities|CapabilityBoundingSet)[[:space:]]*=' "$service" >/dev/null; then
  fail "crates/trawl-server/debian/trawld.service sets AmbientCapabilities or CapabilityBoundingSet directly — the crash-dump grant belongs in the opt-in debian/crashdump.conf drop-in, not the default unit"
fi
if grep -F 'TRAWL_CRASH_DUMP_' "$service" >/dev/null; then
  fail "crates/trawl-server/debian/trawld.service mentions TRAWL_CRASH_DUMP_ — the default unit must carry no reference, commented or not"
fi

# -- 3. the default env file carries no active crash-dump assignment ------

if grep -vE '^[[:space:]]*#' "$default_env" | grep -E 'TRAWL_CRASH_DUMP_[A-Za-z_]*[[:space:]]*=' >/dev/null; then
  fail "crates/trawl-server/debian/trawld.default sets a TRAWL_CRASH_DUMP_* variable uncommented — the unit's EnvironmentFile would make it part of the effective default"
fi

# -- 4. the sandboxing the drop-in depends on is still in place -----------

if ! grep -Eq '^[[:space:]]*ProtectSystem[[:space:]]*=[[:space:]]*strict[[:space:]]*$' "$service"; then
  fail "crates/trawl-server/debian/trawld.service dropped ProtectSystem=strict — the crash-dump drop-in relies on that sandbox"
fi
if ! grep -E '^[[:space:]]*ReadWritePaths[[:space:]]*=.*/var/lib/trawl' "$service" >/dev/null; then
  fail "crates/trawl-server/debian/trawld.service has no ReadWritePaths covering /var/lib/trawl — the crash-dump drop-in writes under that tree"
fi

# -- 5. crashdump.conf carries exactly the four directive lines, in order -

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

# -- 6. the Cargo.toml asset ships the example as documentation only ------

asset_pattern='\[[[:space:]]*"debian/crashdump\.conf"[[:space:]]*,[[:space:]]*"usr/share/doc/trawl-server/examples/crashdump\.conf"[[:space:]]*,[[:space:]]*"644"[[:space:]]*\]'
if ! grep -Eq "$asset_pattern" "$cargo_toml"; then
  fail "crates/trawl-server/Cargo.toml is missing the asset triple [\"debian/crashdump.conf\", \"$asset_dest\", \"644\"]"
fi
# The old spelling of this rule named two literal prefixes, which left every
# other place systemd reads units from wide open: /lib/systemd/system (still a
# real path on non-usrmerged systems and a symlinked alias everywhere else),
# /etc/systemd/user, /usr/lib/systemd/system-preset, and a drop-in directory for
# any unit but trawld. Walk the asset destinations instead and refuse anything
# systemd reads, allowing only the two unit files the package legitimately owns.
# Whatever the crash-dump example ships as, it is documentation, never
# pre-installed configuration.
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
# and derives <name> from the asset's SOURCE path via with_extension("conf"). If
# that derivation does not land on the file the asset actually installs, the
# generated call names a file nobody installed, systemd-sysusers exits 1, and
# dpkg leaves the package half-configured. The rule: source stem + ".conf" must
# equal the destination filename.
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

# The dump directory is created by systemd-tmpfiles, not by postinst's own
# shell. tmpfiles walks the path with O_NOFOLLOW, which a check-then-`install -d`
# pair cannot do: during an upgrade the still-running trawld owns the parent and
# could swap the directory for a symlink between the two steps.
# `d=` and not plain `d`: the `=` suffix is what removes a wrong-type object
# squatting at the path. Without it a regular file there survives, postinst
# still succeeds, and capture dies on EEXIST at the worst possible moment.
if ! grep -Eq "^d=[[:space:]]+${dir_value}[[:space:]]+0700[[:space:]]+trawl[[:space:]]+trawl([[:space:]]|$)" "$tmpfiles_conf"; then
  fail "crates/trawl-server/debian/trawl.tmpfiles does not carry 'd= $dir_value 0700 trawl trawl -' (the '=' enforces the type; plain 'd' leaves a file or symlink in place)"
fi
# trawl-web runs as the same trawl user with /var/lib/trawl writable, so without
# this line the browser-facing proxy can read and replace minidumps, which are
# verbatim copies of trawld's memory. The mask covers the subdirectory only:
# /var/lib/trawl/web.cookie has to stay reachable or the proxy will not start.
if ! grep -Eq "^[[:space:]]*InaccessiblePaths[[:space:]]*=[[:space:]]*-?${dir_value}[[:space:]]*$" "$web_service"; then
  fail "crates/trawl-server/debian/trawl-web.service does not mask '$dir_value' with InaccessiblePaths — the third denial, after the separate uid and the 0700 mode"
fi
if grep -Eq "^[[:space:]]*InaccessiblePaths[[:space:]]*=[[:space:]]*-?/var/lib/trawl[[:space:]]*$" "$web_service"; then
  fail "crates/trawl-server/debian/trawl-web.service masks the whole of /var/lib/trawl — that hides web.cookie too and the proxy cannot start"
fi

# The mask alone is not enough and never was. Running the proxy as the trawl
# user leaves /proc/<trawld-pid>/root as a way around it: same uid passes the
# kernel's ptrace check, and inside trawld's mount namespace nothing is masked.
# A separate uid is what closes that, so the two settings are checked together.
if ! grep -Eq '^[[:space:]]*User[[:space:]]*=[[:space:]]*trawl-web[[:space:]]*$' "$web_service"; then
  fail "crates/trawl-server/debian/trawl-web.service does not set User=trawl-web — as the trawl user the proxy reaches the dumps through /proc/<trawld-pid>/root regardless of InaccessiblePaths"
fi
if ! grep -Eq '^[[:space:]]*u[[:space:]]+trawl-web[[:space:]]' "$sysusers_conf"; then
  fail "crates/trawl-server/debian/trawl.sysusers does not declare the trawl-web user that trawl-web.service runs as"
fi
if ! grep -Eq '^[[:space:]]*m[[:space:]]+trawl-web[[:space:]]+trawl[[:space:]]*$' "$sysusers_conf"; then
  fail "crates/trawl-server/debian/trawl.sysusers does not add trawl-web to the trawl group — the proxy would not be able to read /var/lib/trawl/web.cookie or /etc/trawl/trawld.toml"
fi
# The key is group-readable on purpose, and only because the group has one
# other member. A recursive or wider grant would hand it to everyone. It has to
# come from tmpfiles: trawl owns /var/lib/trawl, so a root chown of a path under
# it follows any symlink planted there, and a planted `web.cookie -> /etc/shadow`
# would come back 0640 trawl:trawl.
if ! grep -Eq '^z[[:space:]]+/var/lib/trawl/web\.cookie[[:space:]]+0640[[:space:]]+trawl[[:space:]]+trawl([[:space:]]|$)' "$tmpfiles_conf"; then
  fail "crates/trawl-server/debian/trawl.tmpfiles does not carry 'z /var/lib/trawl/web.cookie 0640 trawl trawl -' — trawl-web reads the key through the trawl group and needs it group-readable"
fi
if grep -Eq '^[[:space:]]*(chown|chmod)[[:space:]].*/var/lib/trawl/web\.cookie' "$postinst"; then
  fail "crates/trawl-server/debian/postinst chowns or chmods /var/lib/trawl/web.cookie directly — that dereferences a symlink the trawl user can plant; the z line in debian/trawl.tmpfiles is the no-follow way to do it"
fi

# The fleet SSO runbook tells an operator to overwrite that same key by hand,
# which bypasses tmpfiles entirely. If it keeps saying 0600, following it leaves
# a key trawl-web cannot read and a proxy that will not start.
if [[ ! -f "$cutover_page" ]]; then
  fail "docs/src/content/docs/reference/fleet-auth-cutover.md is missing — it carries the by-hand key install that has to agree with the packaged mode"
fi
if ! grep -Eq 'chmod[[:space:]]+0640[[:space:]]+/var/lib/trawl/web\.cookie' "$cutover_page"; then
  fail "docs/src/content/docs/reference/fleet-auth-cutover.md does not install /var/lib/trawl/web.cookie as 0640 — trawl-web reads it through the trawl group and a 0600 key stops the proxy starting"
fi
if grep -Eq 'chmod[[:space:]]+0?600[[:space:]]+/var/lib/trawl/web\.cookie' "$cutover_page"; then
  fail "docs/src/content/docs/reference/fleet-auth-cutover.md still tells operators to chmod the session key 600 — that predates the trawl-web user split"
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
