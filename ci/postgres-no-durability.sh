#!/usr/bin/env bash
#
# postgres-no-durability.sh turns off durability on CI's throwaway postgres
# service container: the settings docker-compose.dev.yml passes on the
# command line, so per-test CREATE and DROP DATABASE stay cheap. Every DROP
# DATABASE forces a checkpoint, and with fsync on, each one synced every file
# of every database created since the last: 17-34 s stalls that held whole
# batches of pg tests at teardown. GitHub service containers take no command
# line, but all three settings apply on a configuration reload.
#
# Usage: ci/postgres-no-durability.sh <service-container-id>
set -euo pipefail

container=$1
pg() { docker exec "$container" psql -U postgres -v ON_ERROR_STOP=1 -tA "$@"; }

# Each -c runs on its own, so ALTER SYSTEM is outside a transaction block.
pg -c 'ALTER SYSTEM SET fsync = off' \
  -c 'ALTER SYSTEM SET synchronous_commit = off' \
  -c 'ALTER SYSTEM SET full_page_writes = off' \
  -c 'SELECT pg_reload_conf()' >/dev/null

# The reload is asynchronous: wait until a new session sees all three.
for _ in $(seq 50); do
  if [ "$(pg -c "SELECT current_setting('fsync') || current_setting('synchronous_commit') || current_setting('full_page_writes')")" = offoffoff ]; then
    echo "postgres durability off: fsync, synchronous_commit, full_page_writes"
    exit 0
  fi
  sleep 0.1
done
echo "postgres did not apply the durability settings within 5 s" >&2
exit 1
