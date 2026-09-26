#!/usr/bin/env bash
# Run only on a disposable GitHub-hosted runner whose PID 1 is systemd, as a
# user with passwordless sudo. It installs trawl-runtime and trawl-server on the
# host itself, because only a running systemd can show that a fresh install
# leaves trawld and trawl-web disabled and inactive. The container verifier
# cannot: its policy-rc.d refuses every start, and no service manager runs.
set -euo pipefail
packages="$(cd "$1" && pwd)"
[[ "${GITHUB_ACTIONS:-}" == true ]] || { echo "refusing: this script mutates the host, run it on a disposable CI runner" >&2; exit 1; }
[[ ! -f /.dockerenv ]]
[[ -d /run/systemd/system ]] || { echo "refusing: systemd is not PID 1 on this host" >&2; exit 1; }
# A policy-rc.d that refuses starts would make "inactive" prove nothing.
[[ ! -e /usr/sbin/policy-rc.d ]] || { echo "refusing: /usr/sbin/policy-rc.d exists and would hide a start" >&2; exit 1; }

units=(trawld trawl-web)
enable_hint='systemctl enable --now trawld trawl-web'

fail() {
  echo "::error::$1" >&2
  exit 1
}

expect_state() { # expect_state <enabled-state> <active-state> <when>
  for unit in "${units[@]}"; do
    # Both commands exit non-zero for disabled or inactive units; the printed
    # state is what is asserted.
    enabled="$(systemctl is-enabled "$unit" 2>&1 || true)"
    active="$(systemctl is-active "$unit" 2>&1 || true)"
    echo "$3: $unit is-enabled=$enabled is-active=$active"
    [[ "$enabled" == "$1" ]] || fail "$3: systemctl is-enabled $unit printed '$enabled', expected '$1'"
    [[ "$active" == "$2" ]] || fail "$3: systemctl is-active $unit printed '$active', expected '$2'"
  done
}

runtime_deb=("$packages"/trawl-runtime_*.deb)
server_deb=("$packages"/trawl-server_*.deb)
[[ ${#runtime_deb[@]} -eq 1 && -f "${runtime_deb[0]}" ]] || fail "expected one trawl-runtime package in $packages"
[[ ${#server_deb[@]} -eq 1 && -f "${server_deb[0]}" ]] || fail "expected one trawl-server package in $packages"
for unit in "${units[@]}"; do
  if systemctl cat "$unit" >/dev/null 2>&1; then
    fail "$unit.service already exists on this host before install"
  fi
done

# 1. Fresh install: nothing enabled, nothing started, and the operator is told
#    what to do next.
log="$(mktemp)"
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
  "${runtime_deb[0]}" "${server_deb[0]}" 2>&1 | tee "$log"
grep -Fq "$enable_hint" "$log" || fail "a fresh install did not print '$enable_hint'"
expect_state disabled inactive "fresh install"

# 2. Reinstall over an operator's enable: dpkg runs postinst configure with the
#    old version, the upgrade path. The units stay enabled, and try-restart does
#    not start a unit that was not running.
sudo systemctl enable "${units[@]}"
sudo DEBIAN_FRONTEND=noninteractive dpkg -i "${server_deb[0]}" 2>&1 | tee "$log"
if grep -Fq "$enable_hint" "$log"; then
  fail "an upgrade repeated the fresh-install message"
fi
expect_state enabled inactive "upgrade over enabled units"

# 3. Purge removes the units and every enable or mask link.
sudo DEBIAN_FRONTEND=noninteractive apt-get purge -y trawl-server
leftovers="$(find /etc/systemd/system -name 'trawld.service' -o -name 'trawl-web.service')"
[[ -z "$leftovers" ]] || fail "purge left systemd links behind: $leftovers"
rm -f "$log"
echo "host Debian service-state assertions passed"
