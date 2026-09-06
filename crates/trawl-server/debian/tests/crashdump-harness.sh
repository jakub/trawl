#!/usr/bin/env bash
#
# crashdump-harness.sh — end-to-end proof that the .deb's crash-dump drop-in
# works on a real systemd host.
#
# The unit tests in packaging.sh assert the packaging *contract* (the drop-in
# ships inert, at the documented path, with the four expected directives).
# They cannot tell you whether copying that file into place actually makes a
# minidump appear when trawld faults. This harness does: it builds the .deb,
# installs it into a privileged systemd container, crashes the daemon with a
# real SIGSEGV, and looks at what landed in /var/lib/trawl/cores.
#
# Usage:
#   crashdump-harness.sh [--deb PATH] [--scopes 1,2] [--allow-host-sysctl]
#                        [--no-negative] [--keep] [--out FILE] [--allow-dirty]
#
#   --deb PATH            use an existing .deb instead of building one
#   --scopes LIST         yama ptrace_scope values to exercise (default 1,2)
#   --allow-host-sysctl   permit RAISING the host's kernel.yama.ptrace_scope
#   --no-negative         skip phase E (the capability-removed control)
#   --keep                leave containers running after the run
#   --out FILE            tee the transcript to FILE
#   --allow-dirty         run against a dirty working tree
#
# The container shares the host kernel, so /proc/sys/kernel/yama/ptrace_scope
# is the HOST's setting. The harness reads it first, never writes a value below
# it, needs --allow-host-sysctl to raise it, and restores it from an EXIT/INT/
# TERM trap — including under --keep.
#
# Two build-tool pins, both deliberate:
#
#   * The .deb is built inside a pinned rust:1.98-trixie container, not on the
#     host. cargo-deb runs dpkg-shlibdeps to resolve `depends = "$auto"`, so the
#     resulting Depends line describes whatever glibc the build host has. Only a
#     trixie build produces a package that installs on trixie.
#
#   * cargo-deb is pinned to 3.8.0, the newest release, because the point of
#     this harness is to exercise the package release.yml actually builds.
#     release.yml installs cargo-deb through taiki-e/install-action with no
#     version, so it gets the newest one; a harness pinned to an older release
#     would happily certify a package nobody ships. It is pinned rather than
#     floating so a run is reproducible, and moving the pin forward is a
#     deliberate step that comes with a full run.
#
#     3.8.0 is also why debian/trawl.sysusers and debian/trawl.tmpfiles are
#     spelled without .conf. It generates `systemd-sysusers <name>` and
#     `systemd-tmpfiles --create <name>` calls in postinst, deriving <name> from
#     the asset's SOURCE path via with_extension("conf"). Under the old
#     debian/trawl.sysusers.conf spelling the generated call named a file nobody
#     installed, exited 1, and left the package half-configured. packaging.sh
#     guards that statically now; this harness is what catches it end to end.

set -Eeuo pipefail

# ---------------------------------------------------------------- constants --

readonly RUST_IMAGE="rust:1.98-trixie@sha256:620dbcd124499c59e2406d3741574b5c5838cf9eb9656f0c3a03948f79b02959"
readonly DEBIAN_IMAGE="debian:trixie@sha256:f324c7ff54321e8d9c588493a20244965938ce0aa50bbd1022d38010e9ffc4b1"
readonly POSTGRES_IMAGE="postgres:18@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280"
readonly CARGO_DEB_VERSION="3.8.0"

readonly BUILDER_IMAGE="trawl-crashdump-builder:cargo-deb-${CARGO_DEB_VERSION}"
readonly NODE_IMAGE="trawl-crashdump-node:trixie"
readonly NET="trawl-crashdump-net"
readonly PG="trawl-crashdump-pg"
readonly NODE="trawl-crashdump-node"
readonly CARGO_VOLUME="trawl-crashdump-cargo-registry"

readonly FLEET_DSN="postgres://fleet:fleetpw@${PG}:5432/fleet"
readonly TRAWL_DSN="postgres://trawl:trawlpw@${PG}:5432/trawl"

readonly YAMA="/proc/sys/kernel/yama/ptrace_scope"
readonly CORES="/var/lib/trawl/cores"

# Distinct exit codes, so a caller can tell an assertion failure from the two
# outcomes that need a human rather than a rerun.
readonly EXIT_SYSCTL_RESTORE_FAILED=3
readonly EXIT_PARTIAL=4
# CAP_SYS_PTRACE is capability number 19.
readonly PTRACE_CAP_BIT=19

# docker cp cannot write into a tmpfs mount, and the node container runs with
# --tmpfs /tmp, so the .deb is staged under /root instead.
readonly DEB_IN_NODE="/root/trawl-server.deb"

# How long to wait for a dump to appear after a fault. RestartSec=5s, so this
# also has to cover the restart that follows.
readonly DUMP_WAIT_SECS=45
readonly ACTIVE_WAIT_SECS=60

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)
readonly repo_root
readonly DOCS_PAGE="$repo_root/docs/src/content/docs/reference/crash-dumps.md"

# The enable command from docs/.../crash-dumps.md, reproduced verbatim. The
# harness diffs this against the doc before running it, so the two cannot drift.
readonly ENABLE_CMD='sudo install -D -m 0644 \
  /usr/share/doc/trawl-server/examples/crashdump.conf \
  /etc/systemd/system/trawld.service.d/crashdump.conf
sudo systemctl daemon-reload && sudo systemctl restart trawld'

# ------------------------------------------------------------------- options --

DEB=""
SCOPES_ARG="1,2"
ALLOW_HOST_SYSCTL=0
RUN_NEGATIVE=1
KEEP=0
OUT=""
ALLOW_DIRTY=0
ARGV=("$@")

usage() {
  cat <<'EOF'
crashdump-harness.sh [--deb PATH] [--scopes 1,2] [--allow-host-sysctl]
                     [--no-negative] [--keep] [--out FILE] [--allow-dirty]

  --deb PATH            use an existing .deb instead of building one
  --scopes LIST         yama ptrace_scope values to exercise (default 1,2)
  --allow-host-sysctl   permit RAISING the host's kernel.yama.ptrace_scope
  --no-negative         skip phase E (the capability-removed control)
  --keep                leave containers running after the run
  --out FILE            tee the transcript to FILE
  --allow-dirty         run against a dirty working tree
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --deb) DEB="${2:?--deb needs a path}"; shift 2 ;;
    --scopes) SCOPES_ARG="${2:?--scopes needs a list}"; shift 2 ;;
    --allow-host-sysctl) ALLOW_HOST_SYSCTL=1; shift ;;
    --no-negative) RUN_NEGATIVE=0; shift ;;
    --keep) KEEP=1; shift ;;
    --out) OUT="${2:?--out needs a path}"; shift 2 ;;
    --allow-dirty) ALLOW_DIRTY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

IFS=',' read -r -a SCOPES <<< "$SCOPES_ARG"
for s in "${SCOPES[@]}"; do
  case "$s" in
    0|1|2) ;;
    3) echo "refusing scope 3: yama locks that value until reboot" >&2; exit 2 ;;
    *) echo "invalid scope '$s' (expected 0, 1 or 2)" >&2; exit 2 ;;
  esac
done
# Ascending, so the run only ever raises the scope. The single lowering is the
# trap's restore back to the value the host started with.
mapfile -t SCOPES < <(printf '%s\n' "${SCOPES[@]}" | sort -n -u)

# --------------------------------------------------------------- transcript --

TEE_FD_SAVED=0
if [[ -n "$OUT" ]]; then
  mkdir -p "$(dirname "$OUT")"
  exec 3>&1
  exec > >(tee "$OUT") 2>&1
  TEE_FD_SAVED=1
fi

finish_transcript() {
  if [[ "$TEE_FD_SAVED" == 1 ]]; then
    exec 1>&3 2>&3
    # Give tee a moment to drain the pipe before the shell exits.
    sleep 0.3
  fi
}

phase() { printf '\n\n## phase %s\n\n' "$*"; }
note()  { printf '  %s\n' "$*"; }
run()   { printf '\n$ %s\n' "$*"; "$@"; }
# Same, but the command's stdout is noise (image ids, container ids).
runq()  { printf '\n$ %s\n' "$*"; "$@" >/dev/null; }
# Run a script inside the node container, echoing it line by line first: these
# are multi-line, and %q would render them as one backslash-mangled blob.
nsh() {
  printf '\n[%s]\n' "$NODE"
  printf '%s\n' "$1" | sed 's/^/$ /'
  docker exec "$NODE" bash -c "$1"
}
# Same, silent: for values the caller formats itself.
nshq()  { docker exec "$NODE" bash -c "$1"; }
die()   { printf '\nFAIL: %s\n' "$*" >&2; exit 1; }

# ------------------------------------------------------------------ cleanup --

ORIG_SCOPE=""
# Set to 1 BEFORE the write, never after: a write that lands and then fails to
# report, or a signal delivered mid-write, must still reach the restore path.
SCOPE_MODIFIED=0
# The value this run last wrote and read back. The restore compares against it
# so a value some other process changed while we ran is reported rather than
# quietly overwritten.
SCOPE_EXPECTED=""

set_host_scope() {
  local value="$1"
  if docker inspect -f '{{.State.Running}}' "$NODE" 2>/dev/null | grep -qx true; then
    docker exec "$NODE" bash -c "echo $value > $YAMA"
  else
    # The node container is gone (cleanup path). A throwaway privileged
    # container reaches the same kernel knob.
    docker run --rm --privileged "$NODE_IMAGE" bash -c "echo $value > $YAMA"
  fi
}

# The host file and the container's view are the same kernel knob. Teardown
# reads the host directly, because by then the container may already be gone.
host_scope() { cat "$YAMA" 2>/dev/null || true; }

restore_host_scope() { # 0 restored or nothing to do, 1 needs a human
  local cur
  if [[ "$SCOPE_MODIFIED" != 1 || -z "$ORIG_SCOPE" ]]; then
    note "host ptrace_scope was never modified"
    return 0
  fi

  cur=$(host_scope)
  if [[ -n "$SCOPE_EXPECTED" && "$cur" != "$SCOPE_EXPECTED" ]]; then
    printf '\n!! NOT RESTORING %s: it reads %s, this run last set %s.\n' \
      "$YAMA" "$cur" "$SCOPE_EXPECTED"
    printf '!! Something else changed it. Decide by hand; it was %s before this run.\n' "$ORIG_SCOPE"
    return 1
  fi

  note "restoring host ptrace_scope to $ORIG_SCOPE"
  if ! set_host_scope "$ORIG_SCOPE"; then
    printf '\n!! COULD NOT WRITE %s — set it to %s by hand !!\n' "$YAMA" "$ORIG_SCOPE"
    return 1
  fi

  cur=$(host_scope)
  if [[ "$cur" != "$ORIG_SCOPE" ]]; then
    printf '\n!! RESTORE DID NOT TAKE: %s reads %s, wanted %s. Set it by hand !!\n' \
      "$YAMA" "$cur" "$ORIG_SCOPE"
    return 1
  fi
  return 0
}

cleanup() {
  local rc=$?
  trap - EXIT INT TERM
  phase "down: teardown"

  local restore_failed=0
  restore_host_scope || restore_failed=1

  if [[ "$KEEP" == 1 ]]; then
    note "--keep: leaving $NODE, $PG and network $NET in place"
  else
    runq docker rm -f "$NODE" "$PG" 2>/dev/null || true
    runq docker network rm "$NET" 2>/dev/null || true
  fi

  printf '\n$ cat %s   # host, after restore\n' "$YAMA"
  cat "$YAMA" 2>/dev/null || echo "(yama not present)"

  # A leaked sysctl outranks whatever else went wrong: the machine is left in a
  # state the operator did not ask for, and only a human can settle it.
  if (( restore_failed )); then
    rc="$EXIT_SYSCTL_RESTORE_FAILED"
    printf '\nexit status: %s (host ptrace_scope needs manual attention)\n' "$rc"
  else
    printf '\nexit status: %s\n' "$rc"
  fi
  finish_transcript
  exit "$rc"
}
trap cleanup EXIT INT TERM

# ------------------------------------------------------------- small helpers --

wait_for() { # wait_for <secs> <description> <bash test command>
  local secs="$1" what="$2" cmd="$3" i
  for ((i = 1; i <= secs; i++)); do
    if eval "$cmd" >/dev/null 2>&1; then
      note "$what after ${i}s"
      return 0
    fi
    sleep 1
  done
  return 1
}

wait_active() {
  wait_for "$ACTIVE_WAIT_SECS" "trawld is-active=active" \
    "[[ \$(docker exec $NODE systemctl is-active trawld 2>/dev/null) == active ]]" && return 0
  docker exec "$NODE" systemctl status trawld --no-pager -l | head -40 || true
  docker exec "$NODE" journalctl -u trawld --no-pager -n 40 || true
  die "trawld did not reach active"
}

main_pid() { docker exec "$NODE" systemctl show trawld -p MainPID --value | tr -d '\r'; }

# The daemon and its monitor are both /usr/bin/trawld, so the monitor is found
# by parentage plus the one environment key that distinguishes it. Only that key
# is ever printed: the daemon's environment carries the postgres DSNs.
find_monitor() {
  docker exec "$NODE" bash -c '
    main=$1
    for d in /proc/[0-9]*; do
      [[ -r "$d/status" ]] || continue
      ppid=$(awk "/^PPid:/{print \$2}" "$d/status" 2>/dev/null) || continue
      [[ "$ppid" == "$main" ]] || continue
      if tr "\0" "\n" < "$d/environ" 2>/dev/null | grep -qx "TRAWL_CRASHDUMP_MONITOR=1"; then
        echo "${d#/proc/}"
        exit 0
      fi
    done
    exit 1
  ' bash "$1"
}

# Print 0 or 1: is CAP_SYS_PTRACE set in <field> (CapEff/CapAmb/...) for <pid>?
cap_bit() { # cap_bit <pid> <field>
  docker exec "$NODE" bash -c '
    v=$(awk "/^$2:/{print \$2}" "/proc/$1/status")
    echo $(( (0x$v >> '"$PTRACE_CAP_BIT"') & 1 ))
  ' bash "$1" "$2" | tr -d ' \r'
}

caps_report() { # caps_report <pid> <label>
  docker exec "$NODE" bash -c '
    pid=$1; label=$2; bit=$3
    eff=$(awk "/^CapEff:/{print \$2}" "/proc/$pid/status")
    amb=$(awk "/^CapAmb:/{print \$2}" "/proc/$pid/status")
    nnp=$(awk "/^NoNewPrivs:/{print \$2}" "/proc/$pid/status")
    printf "  %-8s pid=%-6s CapEff=0x%s bit%d=%d  CapAmb=0x%s bit%d=%d  NoNewPrivs=%s\n" \
      "$label" "$pid" "$eff" "$bit" "$(( (0x$eff >> bit) & 1 ))" \
      "$amb" "$bit" "$(( (0x$amb >> bit) & 1 ))" "$nnp"
  ' bash "$1" "$2" "$PTRACE_CAP_BIT"
}

dump_count() { nshq "ls -1 $CORES/*.dmp 2>/dev/null | wc -l" | tr -d ' \r'; }
newest_dump() { nshq "ls -1t $CORES/*.dmp 2>/dev/null | head -1" | tr -d '\r'; }

# A minidump that could not ptrace its target still parses: minidump-writer
# treats a failed PTRACE_ATTACH as a soft error and drops the thread. So the
# useful question is not "is there a file" but "did it capture any threads".
# MDMP header: magic(4) version(4) stream_count(4) directory_rva(4); each
# directory entry is type(4) size(4) rva(4); ThreadListStream is 3, and its
# payload starts with a u32 count. MemoryListStream is 5, same shape.
install_dump_reader() {
  docker exec -i "$NODE" bash -c 'cat > /root/mdmp-summary && chmod +x /root/mdmp-summary' <<'READER'
#!/usr/bin/env bash
set -euo pipefail
u32() { od -An -tu4 -j "$2" -N4 "$1" | tr -d ' '; }
f="$1"
magic=$(dd if="$f" bs=1 count=4 2>/dev/null)
streams=$(u32 "$f" 8); rva=$(u32 "$f" 12)
threads=-1; mem=-1
for ((i = 0; i < streams; i++)); do
  off=$((rva + 12 * i))
  t=$(u32 "$f" "$off"); d=$(u32 "$f" $((off + 8)))
  [[ "$t" == 3 ]] && threads=$(u32 "$f" "$d")
  [[ "$t" == 5 ]] && mem=$(u32 "$f" "$d")
done
printf 'path=%s magic=%s bytes=%s owner=%s mode=%s streams=%s threads=%s memory_regions=%s\n' \
  "$f" "$magic" "$(stat -c %s "$f")" "$(stat -c %U:%G "$f")" "$(stat -c %a "$f")" \
  "$streams" "$threads" "$mem"
READER
}

dump_summary() { nshq "/root/mdmp-summary '$1'"; }
dump_field() { dump_summary "$1" | tr ' ' '\n' | awk -F= -v k="$2" '$1==k{print $2}'; }

# Force a real fault: gdb parks the main thread's program counter at address 0
# and detaches, so the next instruction fetch is a genuine SIGSEGV rather than a
# signal the kernel delivered on our behalf.
force_fault() {
  local pid="$1"
  printf '\n$ docker exec %s gdb -q -n -batch -p %s -ex '\''set $pc = 0'\'' -ex detach\n' "$NODE" "$pid"
  if docker exec "$NODE" gdb -q -n -batch -p "$pid" -ex 'set $pc = 0' -ex detach 2>&1 | tail -3; then
    return 0
  fi
  note "gdb did not complete; falling back to kill -SEGV (a delivered signal, not a faulting instruction)"
  run docker exec "$NODE" kill -SEGV "$pid"
}

journal_crash_lines() {
  nsh "journalctl -u trawld --no-pager --since '-3min' | grep -iE 'crashdump|minidump|FATAL signal|Main process exited|Scheduled restart' | tail -12"
}

# ------------------------------------------------------- phase 1: the header --

phase "1 header"

if [[ "$ALLOW_DIRTY" == 0 ]]; then
  if [[ -n "$(git -C "$repo_root" status --porcelain)" ]]; then
    git -C "$repo_root" status --short
    die "working tree is dirty; re-run with --allow-dirty if that is intended"
  fi
  note "working tree is clean"
else
  note "--allow-dirty: not checking the working tree"
  git -C "$repo_root" status --short || true
fi

run git -C "$repo_root" rev-parse HEAD
printf '\nargv: %s %s\n' "$(basename "${BASH_SOURCE[0]}")" "${ARGV[*]}"
run date -u
run uname -r
run docker --version

printf '\npinned images:\n'
note "rust     $RUST_IMAGE"
note "debian   $DEBIAN_IMAGE"
note "postgres $POSTGRES_IMAGE"
note "cargo-deb $CARGO_DEB_VERSION (see the header comment for why this is pinned)"

if [[ ! -e "$YAMA" ]]; then
  ORIG_SCOPE=""
  note "yama is not present on this kernel; every scope case will be skipped"
else
  ORIG_SCOPE=$(cat "$YAMA")
  printf '\n$ cat %s   # host, before anything runs\n%s\n' "$YAMA" "$ORIG_SCOPE"
fi

# -------------------------------------------------------- phase 2: the build --

phase "2 build"

if [[ -n "$DEB" ]]; then
  [[ -f "$DEB" ]] || die "--deb $DEB does not exist"
  DEB=$(cd "$(dirname "$DEB")" && pwd)/$(basename "$DEB")
  note "skipped: using --deb $DEB"
else
  target_dir="$repo_root/target/deb-harness"

  printf '\n$ docker build %s   # FROM %s\n' "$BUILDER_IMAGE" "$RUST_IMAGE"
  docker build -t "$BUILDER_IMAGE" -q - <<EOF | sed 's/^/  /'
FROM $RUST_IMAGE
RUN rustup component add clippy rustfmt rust-analyzer
RUN cargo install cargo-deb --version $CARGO_DEB_VERSION --locked \\
 && chmod -R a+rwX "\$CARGO_HOME" "\$RUSTUP_HOME"
EOF

  runq docker volume create "$CARGO_VOLUME"
  # A fresh named volume is root-owned; the build runs as the invoking uid.
  docker run --rm --user 0:0 -v "$CARGO_VOLUME:/usr/local/cargo/registry" \
    "$BUILDER_IMAGE" chown -R "$(id -u):$(id -g)" /usr/local/cargo/registry
  note "cargo registry cache: docker volume $CARGO_VOLUME at /usr/local/cargo/registry"
  note "CARGO_TARGET_DIR: $target_dir (inside the worktree; /tmp here is a 16G tmpfs)"

  # rust-embed scans this directory at macro-expansion time, so trawl-web will
  # not compile without it. A real SPA needs trunk + a wasm toolchain, which
  # this harness has no use for: nothing it asserts touches the web UI.
  dist="$repo_root/crates/trawl-web-ui/dist"
  if [[ -f "$dist/index.html" ]]; then
    note "SPA dist present at $dist (not stubbed)"
  else
    mkdir -p "$dist"
    printf '%s\n' '<!doctype html><title>trawl</title><p>crashdump-harness stub SPA</p>' > "$dist/index.html"
    note "STUBBED the SPA: wrote a one-line placeholder to $dist/index.html"
    note "the embedded web UI in this .deb is a stub, not a real build"
  fi

  mkdir -p "$target_dir"
  # Exactly the four binaries crates/trawl-server/Cargo.toml lists as assets.
  run docker run --rm --user "$(id -u):$(id -g)" \
    -v "$repo_root:/w" -w /w \
    -v "$CARGO_VOLUME:/usr/local/cargo/registry" \
    -e CARGO_TARGET_DIR=/w/target/deb-harness \
    "$BUILDER_IMAGE" \
    cargo build --release -p trawl-server -p trawl-admin -p fleet-admin -p trawl-web \
      --bin trawld --bin trawl-admin --bin fleet-admin --bin trawl-web

  run docker run --rm --user "$(id -u):$(id -g)" \
    -v "$repo_root:/w" -w /w \
    -v "$CARGO_VOLUME:/usr/local/cargo/registry" \
    -e CARGO_TARGET_DIR=/w/target/deb-harness \
    "$BUILDER_IMAGE" \
    cargo deb -p trawl-server --no-build --no-strip

  DEB=$(find "$target_dir/debian" -name 'trawl-server_*.deb' -print -quit)
  [[ -n "$DEB" ]] || die "cargo deb produced no package under $target_dir/debian"
fi

run sha256sum "$DEB"
run dpkg-deb -f "$DEB" Package Version Architecture Depends

# ----------------------------------------------------------- phase 3: bring up --

phase "3 up"

runq docker network create "$NET"

printf '\n$ docker run -d --name %s --network %s %s\n' "$PG" "$NET" "$POSTGRES_IMAGE"
docker run -d --name "$PG" --network "$NET" \
  -e POSTGRES_PASSWORD=harness -e POSTGRES_USER=postgres -e POSTGRES_DB=postgres \
  "$POSTGRES_IMAGE" >/dev/null
wait_for 60 "postgres accepting connections" \
  "docker exec $PG pg_isready -U postgres" || die "postgres never came up"

# trawld needs two databases: the fleet-auth keystore ([auth] database_url,
# migrated by the packaged fleet-admin) and its own app-state database
# ([storage] database_url, which trawld migrates itself at boot).
run docker exec "$PG" psql -U postgres -v ON_ERROR_STOP=1 \
  -c "CREATE ROLE fleet LOGIN PASSWORD 'fleetpw';" \
  -c "CREATE DATABASE fleet OWNER fleet;" \
  -c "CREATE ROLE trawl LOGIN PASSWORD 'trawlpw';" \
  -c "CREATE DATABASE trawl OWNER trawl;"

printf '\n$ docker build %s   # FROM %s\n' "$NODE_IMAGE" "$DEBIAN_IMAGE"
docker build -t "$NODE_IMAGE" -q - <<EOF | sed 's/^/  /'
FROM $DEBIAN_IMAGE
ENV DEBIAN_FRONTEND=noninteractive container=docker
RUN apt-get update \\
 && apt-get install -y --no-install-recommends \\
      systemd systemd-sysv dbus sudo gdb procps ca-certificates
RUN systemctl mask \\
      systemd-udevd.service systemd-udev-trigger.service \\
      systemd-networkd.service systemd-resolved.service \\
      systemd-firstboot.service systemd-modules-load.service \\
      getty@tty1.service
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
EOF

printf '\n$ docker run -d --name %s --privileged --cgroupns=private --tmpfs /run --tmpfs /tmp --network %s %s /sbin/init\n' \
  "$NODE" "$NET" "$NODE_IMAGE"
docker run -d --name "$NODE" --privileged --cgroupns=private \
  --tmpfs /run --tmpfs /tmp --network "$NET" \
  "$NODE_IMAGE" /sbin/init >/dev/null
wait_for 60 "systemd up in $NODE" \
  "docker exec $NODE systemctl is-system-running --wait 2>/dev/null | grep -qE 'running|degraded'" \
  || die "systemd never finished booting in $NODE"

run docker exec "$NODE" systemctl is-system-running
nsh "dpkg-query -W -f='  \${Package} \${Version}\n' systemd gdb sudo procps libc6"
run docker exec "$PG" postgres --version
install_dump_reader

# --------------------------------------------------------- phase 4: install --

phase "4 install"

# /tmp in the node is a docker tmpfs, which docker cp cannot write into.
run docker cp "$DEB" "$NODE:$DEB_IN_NODE"

# `set -o pipefail` inside the container: a bash -c does not inherit it, so
# without this the pipeline reports tail's status and a failed configure would
# read as a clean install. That is exactly how the cargo-deb 3.8 sysusers
# regression could have slipped through every assertion below.
nsh "set -o pipefail
DEBIAN_FRONTEND=noninteractive apt-get install -y $DEB_IN_NODE 2>&1 | tail -8"

# apt's exit status is one witness; dpkg's own record of the package state is a
# second, independent one. A half-configured package is `install ok half-configured`
# here, whatever apt returned.
install_status=$(nshq "dpkg-query -W -f='\${Status}' trawl-server")
printf '\n$ dpkg-query -W -f=%s trawl-server\n%s\n' "'\${Status}'" "$install_status"
[[ "$install_status" == "install ok installed" ]] \
  || die "trawl-server is '$install_status', not 'install ok installed' — the package did not configure"

printf '\n$ dpkg-deb -c %s | grep examples/crashdump.conf\n' "$(basename "$DEB")"
dpkg-deb -c "$DEB" | grep 'examples/crashdump.conf' \
  || die "the .deb does not ship usr/share/doc/trawl-server/examples/crashdump.conf"

# The packaged /etc/default/trawld documents FLEET_DATABASE_URL and
# TRAWL_DATABASE_URL as the place DSNs belong, and the unit reads it as
# EnvironmentFile. Both override the CHANGE_ME placeholders in trawld.toml.
nsh "cat >> /etc/default/trawld <<'EOF'
FLEET_DATABASE_URL=$FLEET_DSN
TRAWL_DATABASE_URL=$TRAWL_DSN
EOF
chown root:trawl /etc/default/trawld && chmod 0640 /etc/default/trawld
grep -c '^[A-Z_]*DATABASE_URL=' /etc/default/trawld"

printf '\n$ docker exec -e DATABASE_URL=... %s /usr/bin/fleet-admin migrate\n' "$NODE"
docker exec -e "DATABASE_URL=$FLEET_DSN" "$NODE" /usr/bin/fleet-admin migrate

run docker exec "$NODE" systemctl restart trawld
wait_active
run docker exec "$NODE" systemctl show trawld -p MainPID -p ActiveState -p SubState

# ------------------------------------------------------- phase 5: A, inert --

phase "5 A inert"

run docker exec "$NODE" systemctl show trawld -p AmbientCapabilities -p Environment

printf '\n$ test -e /etc/systemd/system/trawld.service.d\n'
if nshq "test -e /etc/systemd/system/trawld.service.d"; then
  die "a drop-in directory already exists; the package must ship capture off"
fi
note "absent — the package installs no drop-in"

pid=$(main_pid)
caps_report "$pid" "trawld"
[[ "$(cap_bit "$pid" CapEff)" == 0 ]] || die "the inert daemon already holds CAP_SYS_PTRACE in CapEff"
[[ "$(cap_bit "$pid" CapAmb)" == 0 ]] || die "the inert daemon already holds CAP_SYS_PTRACE in CapAmb"
note "CAP_SYS_PTRACE (bit $PTRACE_CAP_BIT) is clear in both CapEff and CapAmb"

# %F as well as the mode: stat follows symlinks, so a link pointing at some
# other 0700 trawl-owned directory would satisfy owner and mode alone. postinst
# creates this through systemd-tmpfiles precisely so it cannot be a link.
run docker exec "$NODE" stat -c '%F %U %G %a' "$CORES"
[[ "$(nshq "stat -c '%F %U %G %a' $CORES")" == "directory trawl trawl 700" ]] \
  || die "$CORES is not a plain directory owned trawl:trawl mode 0700"

# ------------------------------------------------------ phase 6: B, enable --

phase "6 B enable"

# Verbatim from the docs. If the page changes, this stops rather than silently
# testing something the operator was never told to run.
docs_block=$(awk '
  /^To enable:$/ { found = 1; next }
  found && /^```bash$/ { inblock = 1; next }
  inblock && /^```$/ { exit }
  inblock { print }
' "$DOCS_PAGE")
if [[ "$docs_block" != "$ENABLE_CMD" ]]; then
  printf '\n--- docs %s ---\n%s\n--- harness ---\n%s\n' "$DOCS_PAGE" "$docs_block" "$ENABLE_CMD"
  die "the enable command in the docs no longer matches the one this harness runs"
fi
note "enable command matches $DOCS_PAGE"

printf '\n$ docker exec -i %s bash -s   # the docs enable command, verbatim:\n%s\n' "$NODE" "$ENABLE_CMD"
docker exec -i "$NODE" bash -s <<< "$ENABLE_CMD"

wait_active
run docker exec "$NODE" systemctl show trawld -p AmbientCapabilities -p Environment

# ---------------------------------------------- phases C/D: a fault per scope --

# Sets SCOPE_READY (1 usable, 0 skip) and SCOPE_SKIP_REASON. It reports through
# variables rather than an exit status on purpose: a function called as an `if`
# condition runs with errexit suppressed for its whole body, so a docker exec
# that failed inside would fall through instead of aborting, and the run could
# certify a scope it never actually set.
SCOPE_READY=0
SCOPE_SKIP_REASON=""

ensure_scope() { # ensure_scope <wanted>
  local want="$1" cur readback
  SCOPE_READY=0
  SCOPE_SKIP_REASON=""

  if [[ -z "$ORIG_SCOPE" ]]; then
    SCOPE_SKIP_REASON="no $YAMA on this kernel"
    return 0
  fi

  cur=$(nshq "cat $YAMA")
  printf '\n$ docker exec %s cat %s   # shared kernel: this IS the host value\n%s\n' "$NODE" "$YAMA" "$cur"

  if (( want < ORIG_SCOPE )); then
    SCOPE_SKIP_REASON="the host runs ptrace_scope=$ORIG_SCOPE and this harness never lowers it"
    return 0
  fi
  if (( want > cur )); then
    if (( ALLOW_HOST_SYSCTL == 0 )); then
      SCOPE_SKIP_REASON="requires --allow-host-sysctl"
      return 0
    fi
    printf '\n$ docker exec %s bash -c '\''echo %s > %s'\''\n' "$NODE" "$want" "$YAMA"
    # Flag first. If the write lands and then something goes wrong before the
    # readback, teardown still has to put the host back.
    SCOPE_MODIFIED=1
    set_host_scope "$want"
    readback=$(nshq "cat $YAMA")
    SCOPE_EXPECTED="$readback"
    [[ "$readback" == "$want" ]] \
      || die "asked the kernel for ptrace_scope=$want, it reads $readback"
    note "raised $cur -> $want, confirmed by readback (restored to $ORIG_SCOPE by the exit trap)"
  fi

  # Whether we wrote it or found it already there, the case only runs against a
  # kernel that reports the scope the case claims to be testing. A scope-1 run
  # must never be filed as evidence for scope 2.
  readback=$(nshq "cat $YAMA")
  [[ "$readback" == "$want" ]] \
    || die "scope case $want would run against ptrace_scope=$readback"

  SCOPE_READY=1
  return 0
}

scope_case() { # scope_case <scope>
  local scope="$1" pid mon before after restarts_before restarts_after newest threads

  run docker exec "$NODE" systemctl restart trawld
  wait_active
  printf '\n$ docker exec %s cat %s\n%s\n' "$NODE" "$YAMA" "$(nshq "cat $YAMA")"

  pid=$(main_pid)
  wait_for 20 "monitor process found" "find_monitor $pid" || die "no crash-dump monitor under pid $pid"
  mon=$(find_monitor "$pid")
  printf '\n$ tr "\\0" "\\n" < /proc/%s/environ | grep TRAWL_CRASHDUMP_MONITOR\n' "$mon"
  nshq "tr '\0' '\n' < /proc/$mon/environ | grep '^TRAWL_CRASHDUMP_MONITOR='"
  printf '\ncapabilities (bit %s = CAP_SYS_PTRACE):\n' "$PTRACE_CAP_BIT"
  caps_report "$pid" "daemon"
  caps_report "$mon" "monitor"
  [[ "$(cap_bit "$pid" CapEff)" == 1 ]] || die "scope $scope: the daemon has no CAP_SYS_PTRACE despite the drop-in"
  [[ "$(cap_bit "$mon" CapEff)" == 1 ]] || die "scope $scope: the monitor did not inherit CAP_SYS_PTRACE"

  before=$(dump_count)
  restarts_before=$(docker exec "$NODE" systemctl show trawld -p NRestarts --value | tr -d '\r')
  note "dumps before the fault: $before   NRestarts=$restarts_before"

  force_fault "$pid"

  wait_for "$DUMP_WAIT_SECS" "a new dump appeared" "(( \$(docker exec $NODE bash -c 'ls -1 $CORES/*.dmp 2>/dev/null | wc -l') > $before ))" \
    || { journal_crash_lines; die "scope $scope: no new dump within ${DUMP_WAIT_SECS}s"; }

  after=$(dump_count)
  newest=$(newest_dump)
  printf '\n$ /root/mdmp-summary %s\n' "$newest"
  dump_summary "$newest"

  [[ "$(dump_field "$newest" magic)" == "MDMP" ]] || die "scope $scope: $newest is not a minidump"
  [[ "$(dump_field "$newest" owner)" == "trawl:trawl" ]] || die "scope $scope: $newest is not owned by trawl:trawl"
  [[ "$(dump_field "$newest" mode)" == "600" ]] || die "scope $scope: $newest is not mode 600"
  (( $(dump_field "$newest" bytes) > 0 )) || die "scope $scope: $newest is empty"
  threads=$(dump_field "$newest" threads)
  (( threads > 0 )) || die "scope $scope: $newest captured $threads threads — the monitor could not ptrace"
  note "scope $scope: dumps $before -> $after, $threads threads captured"

  journal_crash_lines
  wait_active
  restarts_after=$(docker exec "$NODE" systemctl show trawld -p NRestarts --value | tr -d '\r')
  note "NRestarts $restarts_before -> $restarts_after (Restart=on-failure brought trawld back)"
  (( restarts_after > restarts_before )) || die "scope $scope: systemd did not restart trawld"
}

# A skipped case is a hole in the evidence, not a pass. Both get counted so the
# result block can say which of the two it is.
POSITIVE_REQUESTED=${#SCOPES[@]}
POSITIVE_RUN=0
NOT_RUN=()

for scope in "${SCOPES[@]}"; do
  phase "C/D fault at yama ptrace_scope=$scope"
  ensure_scope "$scope"
  if (( SCOPE_READY )); then
    scope_case "$scope"
    POSITIVE_RUN=$((POSITIVE_RUN + 1))
  else
    note "skipped: $SCOPE_SKIP_REASON"
    NOT_RUN+=("crash case at ptrace_scope=$scope ($SCOPE_SKIP_REASON)")
  fi
done

# -------------------------------------------- phase E: the capability control --

phase "E negative (capability removed)"

NEGATIVE_READY=0
if [[ "$RUN_NEGATIVE" == 0 ]]; then
  note "skipped: --no-negative"
else
  ensure_scope 2
  if (( SCOPE_READY )); then
    NEGATIVE_READY=1
  else
    note "skipped: $SCOPE_SKIP_REASON"
    # --no-negative is an operator asking for less. This branch is the harness
    # failing to deliver what it was asked for, which is a different thing.
    NOT_RUN+=("negative control at ptrace_scope=2 ($SCOPE_SKIP_REASON)")
  fi
fi

if (( NEGATIVE_READY )); then
  # What this proves, and what it does NOT prove.
  #
  # The docs say a half-applied drop-in gives you "the stderr breadcrumb and no
  # .dmp". That is not what happens with minidump-writer 0.13: a failed
  # PTRACE_ATTACH is a SOFT error, so the thread is dropped from the dump and
  # the writer reports success anyway. You still get a file, still get
  # "wrote minidump" in the journal, and only the contents give it away — zero
  # threads, zero memory regions, about a tenth the size. Silent denial, worse
  # than documented, because the artifact looks fine until you open it.
  cat <<'EOF'

  Control: the same drop-in with AmbientCapabilities removed. The two
  Environment lines stay, so trawld still starts its monitor and still logs
  "trawl-crashdump: enabled". The assertion is that no dump carrying THREAD
  DATA appears — a zero-thread file is a pass, and is what actually happens.
EOF

  nsh "cat > /etc/systemd/system/trawld.service.d/crashdump.conf <<'EOF'
[Service]
Environment=TRAWL_CRASH_DUMP_DIR=$CORES
Environment=TRAWL_CRASH_DUMP_RETAIN=10
EOF
cat /etc/systemd/system/trawld.service.d/crashdump.conf"

  run docker exec "$NODE" systemctl daemon-reload
  run docker exec "$NODE" systemctl restart trawld
  wait_active
  run docker exec "$NODE" systemctl show trawld -p AmbientCapabilities -p Environment

  neg_pid=$(main_pid)
  wait_for 20 "monitor process found" "find_monitor $neg_pid" || die "no monitor under pid $neg_pid"
  neg_mon=$(find_monitor "$neg_pid")
  caps_report "$neg_pid" "daemon"
  caps_report "$neg_mon" "monitor"
  [[ "$(cap_bit "$neg_pid" CapEff)" == 0 ]] || die "the control still grants the daemon CAP_SYS_PTRACE"
  [[ "$(cap_bit "$neg_mon" CapEff)" == 0 ]] || die "the control still grants the monitor CAP_SYS_PTRACE"

  neg_before=$(dump_count)
  note "dumps before the fault: $neg_before"
  force_fault "$neg_pid"

  # Long enough that a working capture would certainly have landed: the positive
  # cases above take a couple of seconds.
  note "waiting ${DUMP_WAIT_SECS}s for a dump that should not be usable"
  sleep "$DUMP_WAIT_SECS"

  neg_after=$(dump_count)
  if (( neg_after == neg_before )); then
    note "outcome: no new dump at all (the docs' stated failure mode)"
  else
    neg_newest=$(newest_dump)
    printf '\n$ /root/mdmp-summary %s\n' "$neg_newest"
    dump_summary "$neg_newest"
    neg_threads=$(dump_field "$neg_newest" threads)
    (( neg_threads == 0 )) \
      || die "negative control captured $neg_threads threads without CAP_SYS_PTRACE — the capability line is not load-bearing, or the drop-in did not take effect"
    note "outcome: a file appeared but captured 0 threads and 0 memory regions — an unusable dump"
  fi
  journal_crash_lines

  note "restoring the shipped drop-in"
  nsh "install -D -m 0644 /usr/share/doc/trawl-server/examples/crashdump.conf /etc/systemd/system/trawld.service.d/crashdump.conf && systemctl daemon-reload && systemctl restart trawld"
  wait_active
  run docker exec "$NODE" systemctl show trawld -p AmbientCapabilities
fi

phase "result"

note "crash cases executed: $POSITIVE_RUN of $POSITIVE_REQUESTED requested (${SCOPES[*]})"
if (( ${#NOT_RUN[@]} == 0 )); then
  note "every assertion passed"
else
  note "result: partial"
  for missing in "${NOT_RUN[@]}"; do
    note "  not run: $missing"
  done
  note "what ran passed, but this run does not cover what it was asked to cover"
  exit "$EXIT_PARTIAL"
fi
