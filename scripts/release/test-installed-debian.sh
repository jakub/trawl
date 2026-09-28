#!/usr/bin/env bash
# Run only as root inside a disposable Debian container, with SYS_PTRACE in
# its capability bounding set. No database or systemd service is started.
set -euo pipefail
packages="$(cd "$1" && pwd)"
tooling="$(cd "$(dirname "$0")" && pwd)"
[[ "$(id -u)" == 0 ]]
[[ -f /.dockerenv ]]
printf '#!/bin/sh\nexit 101\n' > /usr/sbin/policy-rc.d
chmod 755 /usr/sbin/policy-rc.d
dpkg -i "$packages"/*.deb
# No service manager runs here, but systemctl reads enablement offline. Both
# units must install disabled; test-host-debian.sh checks the running state.
for unit in trawld trawl-web; do
  state="$(systemctl is-enabled "$unit" 2>&1 || true)"
  [[ "$state" == disabled ]] || { echo "$unit.service is '$state' after install, expected 'disabled'" >&2; exit 1; }
done
python3 - "$packages" <<'PY'
from pathlib import Path
import subprocess, sys
packages = list(Path(sys.argv[1]).glob('*.deb'))
assert len(packages) == 3
runtime = next(p for p in packages if p.name.startswith('trawl-runtime_'))
version = subprocess.check_output(['dpkg-deb', '-f', str(runtime), 'Version'], text=True).strip()
for package in packages:
    contents = subprocess.check_output(['dpkg-deb', '-c', str(package)], text=True)
    assert ('./usr/lib/trawl/libduckdb.so' in contents) == (package == runtime)
    if package != runtime:
        depends = subprocess.check_output(['dpkg-deb', '-f', str(package), 'Depends'], text=True)
        assert f'trawl-runtime (= {version})' in depends
assert subprocess.check_output(['dpkg-query', '-S', '/usr/lib/trawl/libduckdb.so'], text=True).strip() == 'trawl-runtime: /usr/lib/trawl/libduckdb.so'
PY
python3 "$tooling/smoke-cli.py" /usr/bin/trawl "$tooling/fixtures/cli.parquet"
python3 "$tooling/check-runtime.py" /usr/lib/trawl/libduckdb.so "$tooling/fixtures/cli.parquet"
for binary in trawld trawl-admin fleet-admin trawl-web; do
  "/usr/bin/$binary" --help >/dev/null
done
# Test the actual daemon with its deployed capability, not only an unprivileged
# ldd invocation. AT_SECURE is independently asserted by the small loader probe.
setcap cap_sys_ptrace+p /usr/bin/trawld
cat > /tmp/trawl-check.toml <<'TOML'
[server]
http_addr = "127.0.0.1:1"
[auth]
database_url = "postgres://unused:unused@127.0.0.1:1/fleet"
[storage]
database_url = "postgres://unused:unused@127.0.0.1:1/trawl"
[data]
path = "/tmp/trawl-check-data"
TOML
runuser -u trawl -- /usr/bin/trawld --config /tmp/trawl-check.toml --check-config
test ! -e /tmp/trawl-check-data
cat > /tmp/trawl-loader.c <<'C'
#include <stdio.h>
#include <sys/auxv.h>
extern const char *duckdb_library_version(void);
int main(void) {
    unsigned long secure = getauxval(AT_SECURE);
    printf("secure=%lu duckdb=%s\n", secure, duckdb_library_version());
    return secure == 1 ? 0 : 1;
}
C
cc /tmp/trawl-loader.c -L/usr/lib/trawl -lduckdb '-Wl,-rpath,$ORIGIN/../lib/trawl' -o /usr/bin/trawl-loader-probe
setcap cap_sys_ptrace+p /usr/bin/trawl-loader-probe
runuser -u trawl -- /usr/bin/trawl-loader-probe
dpkg -r trawl-cli
test -f /usr/lib/trawl/libduckdb.so
/usr/bin/trawld --help >/dev/null
dpkg -r trawl-server
test -f /usr/lib/trawl/libduckdb.so
dpkg -r trawl-runtime
test ! -e /usr/lib/trawl/libduckdb.so
