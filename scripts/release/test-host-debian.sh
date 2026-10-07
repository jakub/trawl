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

# 3. Boot both services against disposable local databases and inspect actual
#    stored files. CI checks out only scripts/release, so use packaged binaries.
umask 077
scratch="$(mktemp -d)"
probe="$(dirname "${BASH_SOURCE[0]}")/host-debian-probe.py"

on_exit() {
  local status=$? path
  trap - EXIT
  if (( status != 0 )); then
    echo "::error::host verification failed; recent service journal and modes follow" >&2
    sudo journalctl -u trawld -u trawl-web --no-pager -n 200 >&2 || true
    for path in /var/lib/trawl /var/lib/trawl/data /var/lib/trawl/data/wal \
      /var/lib/trawl/web.cookie /var/lib/trawl/tls /var/lib/trawl/tls/cert.pem \
      /var/lib/trawl/tls-key /var/lib/trawl/tls-key/key.pem /var/log/trawl; do
      sudo stat -c '%n owner=%U group=%G mode=%a' "$path" >&2 || true
    done
    for path in "${wal:-}" "${parquet:-}"; do
      if [[ -n "$path" ]]; then
        sudo stat -c '%n owner=%U group=%G mode=%a' "$path" >&2 || true
      fi
    done
  fi
  rm -rf "$scratch"
  rm -f "$log"
  exit "$status"
}
trap on_exit EXIT

wait_until() { # wait_until <label> <seconds> <predicate> [args...]
  local label="$1" seconds="$2" deadline
  shift 2
  deadline=$((SECONDS + seconds))
  while (( SECONDS < deadline )); do
    if "$@"; then return 0; fi
    sleep 1
  done
  fail "$label timed out after ${seconds}s"
}

assert_mode() { # assert_mode <path> <octal>
  local actual
  actual="$(sudo stat -c %a "$1")"
  [[ "$actual" == "$2" ]] || fail "$1 mode is $actual, expected $2"
}

assert_owner() { # assert_owner <path> <user>
  local actual
  actual="$(sudo stat -c %U "$1")"
  [[ "$actual" == "$2" ]] || fail "$1 owner is $actual, expected $2"
}

assert_kind() { # assert_kind <path> <d|f>
  case "$2" in
    d) sudo test -d "$1" || fail "$1 is not a directory" ;;
    f) sudo test -f "$1" || fail "$1 is not a file" ;;
  esac
}

server_healthy() {
  curl --silent --show-error --fail --max-time 3 --cacert "$scratch/cert.pem" \
    -o /dev/null https://127.0.0.1:5514/api/v1/health 2>/dev/null
}

web_healthy() {
  curl --silent --show-error --fail --max-time 3 \
    -o /dev/null http://127.0.0.1:8090/healthz 2>/dev/null
}

has_wal() {
  wal="$(sudo find /var/lib/trawl/data/wal/prod -type f -name '*.ndjson' -print -quit 2>/dev/null)"
  [[ -n "$wal" ]] && sudo test -s "$wal"
}

has_parquet() {
  parquet="$(sudo find /var/lib/trawl/data -type f -name '*.parquet' \
    ! -path '*/scheduled/*' -print -quit 2>/dev/null)"
  [[ -n "$parquet" ]] && sudo test -s "$parquet"
}

signin_query_ready() {
  python3 "$probe" signin-query "$scratch/human.token" 2>/dev/null
}

probe_as() { # probe_as <user> <action> <args...>
  local user="$1"
  shift
  (cd / && sudo -u "$user" python3 -I - "$@" < "$probe")
}

postgres_as() {
  (cd / && sudo -u postgres "$@")
}

# Nobody is stopped by the 0750 state root. trawl-web can traverse that root,
# but the 0700 data root stops it. Grant group traversal to the file, so this
# separate trawl-web denial reaches the file's own 0600 mode.
assert_file_blocks_web() {
  local file="$1" dir
  local -a ancestors=()
  dir="${file%/*}"
  while [[ "$dir" != /var/lib/trawl ]]; do
    assert_mode "$dir" 700
    ancestors+=("$dir")
    dir="${dir%/*}"
  done
  (
    trap 'for dir in "${ancestors[@]}"; do sudo chmod g-x "$dir"; done' EXIT
    for dir in "${ancestors[@]}"; do sudo chmod g+x "$dir"; done
    for dir in "${ancestors[@]}"; do
      (cd / && sudo -u trawl-web test -x "$dir") || fail "trawl-web cannot traverse $dir"
    done
    probe_as trawl-web deny-file "$file"
  )
}

if ! command -v pg_lsclusters >/dev/null 2>&1; then
  sudo apt-get update -qq >/dev/null
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends postgresql >/dev/null
fi
clusters="$(pg_lsclusters --no-header)"
if [[ -z "$clusters" ]]; then
  pg_version="$(find /usr/lib/postgresql -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort -V | tail -1)"
  [[ -n "$pg_version" ]] || fail "PostgreSQL installed without a server version"
  sudo pg_createcluster "$pg_version" main --start >/dev/null
  clusters="$(pg_lsclusters --no-header)"
fi
[[ -n "$clusters" ]] || fail "PostgreSQL has no cluster"
read -r pg_version pg_name pg_port pg_status _ <<< "${clusters%%$'\n'*}"
if [[ "$pg_status" != online ]]; then
  sudo pg_ctlcluster "$pg_version" "$pg_name" start
fi
wait_until "PostgreSQL readiness" 60 pg_isready -q -h 127.0.0.1 -p "$pg_port"

# The passwords are hex, so interpolating them into SQL and DSNs cannot add
# SQL syntax or URL delimiters. Neither SQL nor token output enters CI logs.
fleet_password="$(openssl rand -hex 24)"
trawl_password="$(openssl rand -hex 24)"
if ! postgres_as psql -p "$pg_port" -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
CREATE ROLE fleet LOGIN PASSWORD '$fleet_password';
CREATE ROLE trawl LOGIN PASSWORD '$trawl_password';
SQL
then fail "could not create PostgreSQL roles"; fi
postgres_as createdb -p "$pg_port" -O fleet fleet >/dev/null 2>&1 || fail "could not create fleet database"
postgres_as createdb -p "$pg_port" -O trawl trawl >/dev/null 2>&1 || fail "could not create trawl database"
fleet_dsn="postgres://fleet:${fleet_password}@127.0.0.1:${pg_port}/fleet"
trawl_dsn="postgres://trawl:${trawl_password}@127.0.0.1:${pg_port}/trawl"
DATABASE_URL="$fleet_dsn" /usr/bin/fleet-admin migrate >/dev/null 2>&1 || fail "fleet schema migration failed"

# EnvironmentFile is read by systemd as root. tee preserves its packaged
# inode; the explicit mode check catches any packaging drift.
sudo tee /etc/default/trawld >/dev/null <<EOF
FLEET_DATABASE_URL=$fleet_dsn
TRAWL_DATABASE_URL=$trawl_dsn
EOF
sudo chown root:trawl /etc/default/trawld
sudo chmod 0640 /etc/default/trawld

sudo tee /etc/trawl/trawld.toml >/dev/null <<'TOML'
[server]
http_addr = "127.0.0.1:5514"
shutdown_drain_secs = 5

[data]
path = "/var/lib/trawl/data"

[auth]
database_url = "postgres://fleet:CHANGE_ME@db.example.internal:5432/fleet"

[storage]
database_url = "postgres://trawl:CHANGE_ME@db.example.internal:5432/trawl"

[ingest]
enabled = true
default_env = "prod"
envs = ["prod"]
compaction_interval_secs = 600
internal_telemetry = false

[retention]
max_age_days = 0
min_free_disk_bytes = 0

[scheduler]
enabled = false

[syslog]
enabled = false

[web]
bind_addr = "127.0.0.1:8090"
public_origins = ["http://127.0.0.1:8090"]
upstream_url = "https://127.0.0.1:5514"
upstream_ca_path = "/var/lib/trawl/tls/cert.pem"
cookie_secret_path = "/var/lib/trawl/web.cookie"
allow_insecure_cookies = true
TOML
sudo chown root:trawl /etc/trawl/trawld.toml
sudo chmod 0640 /etc/trawl/trawld.toml
assert_mode /etc/default/trawld 640
assert_mode /etc/trawl/trawld.toml 640

DATABASE_URL="$fleet_dsn" /usr/bin/fleet-admin roles create --name host-ci-ingest \
  --perm trawl:ingest >/dev/null 2>&1 || fail "could not create ingest role"
DATABASE_URL="$fleet_dsn" /usr/bin/fleet-admin roles create --name host-ci-query \
  --perm trawl:query >/dev/null 2>&1 || fail "could not create query role"
DATABASE_URL="$fleet_dsn" /usr/bin/fleet-admin keys create --name host-ci-collector \
  --kind service --role host-ci-ingest > "$scratch/ingest.token" 2>/dev/null || fail "could not create ingest key"
DATABASE_URL="$fleet_dsn" /usr/bin/fleet-admin keys create --name host-ci-reader \
  --kind human --role host-ci-query > "$scratch/human.token" 2>/dev/null || fail "could not create human key"
[[ -s "$scratch/ingest.token" && -s "$scratch/human.token" ]] || fail "fleet-admin returned an empty token"

sudo systemctl start trawld
wait_until "generated certificate" 60 sudo test -s /var/lib/trawl/tls/cert.pem
sudo install -m 0600 -o "$(id -un)" /var/lib/trawl/tls/cert.pem "$scratch/cert.pem"
wait_until "trawld health" 60 server_healthy
sudo systemctl start trawl-web
wait_until "trawl-web health" 60 web_healthy

assert_kind /var/lib/trawl d
assert_owner /var/lib/trawl trawl
assert_mode /var/lib/trawl 750
assert_kind /var/lib/trawl/data d
assert_owner /var/lib/trawl/data trawl
assert_mode /var/lib/trawl/data 700
assert_kind /var/lib/trawl/data/wal d
assert_owner /var/lib/trawl/data/wal trawl
assert_mode /var/lib/trawl/data/wal 700
assert_kind /var/log/trawl d
assert_owner /var/log/trawl trawl
assert_mode /var/log/trawl 700
assert_kind /var/lib/trawl/web.cookie f
assert_owner /var/lib/trawl/web.cookie trawl
assert_mode /var/lib/trawl/web.cookie 640
assert_kind /var/lib/trawl/tls d
assert_owner /var/lib/trawl/tls trawl
assert_mode /var/lib/trawl/tls 755
assert_kind /var/lib/trawl/tls/cert.pem f
assert_owner /var/lib/trawl/tls/cert.pem trawl
assert_mode /var/lib/trawl/tls/cert.pem 644
assert_kind /var/lib/trawl/tls-key d
assert_owner /var/lib/trawl/tls-key trawl
assert_mode /var/lib/trawl/tls-key 700
assert_kind /var/lib/trawl/tls-key/key.pem f
assert_owner /var/lib/trawl/tls-key/key.pem trawl
assert_mode /var/lib/trawl/tls-key/key.pem 600
sudo -u trawl-web test -r /var/lib/trawl/web.cookie || fail "trawl-web cannot read the session key"
sudo -u trawl-web test -r /var/lib/trawl/tls/cert.pem || fail "trawl-web cannot read the pinned certificate"
probe_as trawl-web can-read /var/lib/trawl/web.cookie
probe_as trawl-web can-read /var/lib/trawl/tls/cert.pem

python3 "$probe" ingest "$scratch/ingest.token" "$scratch/cert.pem"
wait_until "durable WAL file" 30 has_wal
assert_kind "$wal" f
assert_owner "$wal" trawl
assert_mode "$wal" 600
for user in nobody trawl-web; do
  probe_as "$user" deny-list /var/lib/trawl/data
  probe_as "$user" deny-file "$wal"
done
assert_file_blocks_web "$wal"
probe_as trawl can-read "$wal"

# Keep the first WAL stable, then use a short tick to publish its event.
sudo sed -i 's/compaction_interval_secs = 600/compaction_interval_secs = 1/' /etc/trawl/trawld.toml
sudo systemctl restart trawld
wait_until "trawld health after compaction restart" 60 server_healthy
wait_until "published Parquet file" 90 has_parquet
assert_kind "$parquet" f
assert_owner "$parquet" trawl
assert_mode "$parquet" 600
for user in nobody trawl-web; do
  probe_as "$user" deny-list /var/lib/trawl/data
  probe_as "$user" deny-file "$parquet"
done
assert_file_blocks_web "$parquet"
probe_as trawl can-read "$parquet"
wait_until "browser sign-in and stored-event query" 60 signin_query_ready

# 4. Simulate an old installation, then exercise the package's running-unit
#    upgrade path. A world-readable sentinel proves no recursive chmod ran.
sudo systemctl stop trawl-web trawld
sentinel=/var/lib/trawl/data/upgrade-sentinel
sudo -u trawl install -m 0644 /dev/null "$sentinel"
printf 'legacy file stays unchanged\n' | sudo -u trawl tee "$sentinel" >/dev/null
sentinel_inode="$(sudo stat -c %i "$sentinel")"
sentinel_sha="$(sudo sha256sum "$sentinel" | cut -d ' ' -f 1)"
# The shipped unit is a non-conffile, so dpkg replaces this edited copy. Its
# missing mode directives reproduce the old unit's 0755 defaults at start.
unit=/usr/lib/systemd/system/trawld.service
[[ -f "$unit" ]] || fail "installed trawld unit is missing"
[[ "$(readlink -f "$(systemctl show trawld -p FragmentPath --value)")" == "$(readlink -f "$unit")" ]] || \
  fail "systemd is not loading the installed trawld unit"
sudo sed -i '/^[[:space:]]*StateDirectoryMode[[:space:]]*=/d; /^[[:space:]]*LogsDirectoryMode[[:space:]]*=/d' "$unit"
sudo systemctl daemon-reload
unit_text="$(systemctl cat trawld)" || fail "could not read the old-unit simulation"
if grep -E '^[[:space:]]*(StateDirectoryMode|LogsDirectoryMode)[[:space:]]*=' <<< "$unit_text" >/dev/null; then
  fail "old-unit simulation left an active directory mode directive"
fi
sudo chmod 0755 /var/lib/trawl /var/lib/trawl/data /var/log/trawl
sudo systemctl start trawld
wait_until "trawld health after old-mode start" 60 server_healthy
assert_mode /var/lib/trawl 755
assert_mode /var/log/trawl 755
before_upgrade="$(systemctl show trawld -p InvocationID --value)"
[[ -n "$before_upgrade" ]] || fail "trawld has no InvocationID before upgrade"
# This package already has the new binary: it closes data at start even under
# the old unit. Reopen data while that same invocation runs.
sudo chmod 0755 /var/lib/trawl/data
assert_mode /var/lib/trawl/data 755
assert_mode "$sentinel" 644
[[ "$(systemctl show trawld -p InvocationID --value)" == "$before_upgrade" ]] || \
  fail "trawld restarted before the package upgrade"
sudo DEBIAN_FRONTEND=noninteractive dpkg -i "${server_deb[0]}" 2>&1 | tee "$log"
grep -Eq '^[[:space:]]*StateDirectoryMode[[:space:]]*=[[:space:]]*0750[[:space:]]*$' "$unit" || \
  fail "upgrade did not restore StateDirectoryMode=0750"
grep -Eq '^[[:space:]]*LogsDirectoryMode[[:space:]]*=[[:space:]]*0700[[:space:]]*$' "$unit" || \
  fail "upgrade did not restore LogsDirectoryMode=0700"
invocation_changed() {
  local after_upgrade
  after_upgrade="$(systemctl show trawld -p InvocationID --value)"
  [[ -n "$after_upgrade" && "$after_upgrade" != "$before_upgrade" ]] && \
    systemctl is-active --quiet trawld
}
wait_until "trawld restart on package upgrade" 60 invocation_changed
wait_until "trawld health after package upgrade" 60 server_healthy
assert_mode /var/lib/trawl 750
assert_mode /var/lib/trawl/data 700
assert_mode /var/lib/trawl/data/wal 700
assert_mode /var/log/trawl 700
assert_mode "$sentinel" 644
[[ "$(sudo stat -c %i "$sentinel")" == "$sentinel_inode" ]] || fail "upgrade replaced the legacy sentinel"
[[ "$(sudo sha256sum "$sentinel" | cut -d ' ' -f 1)" == "$sentinel_sha" ]] || fail "upgrade changed the legacy sentinel"
for user in nobody trawl-web; do
  probe_as "$user" deny-file "$sentinel"
done
probe_as trawl can-read "$sentinel"
sudo systemctl start trawl-web
wait_until "trawl-web health after upgrade" 60 web_healthy
wait_until "browser sign-in and query after upgrade" 60 signin_query_ready
echo "host owner-only storage assertions passed"

# 5. Purge removes the units and every enable or mask link.
sudo DEBIAN_FRONTEND=noninteractive apt-get purge -y trawl-server
leftovers="$(find /etc/systemd/system -name 'trawld.service' -o -name 'trawl-web.service')"
[[ -z "$leftovers" ]] || fail "purge left systemd links behind: $leftovers"
rm -f "$log"
echo "host Debian service-state assertions passed"
