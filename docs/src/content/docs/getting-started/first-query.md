---
title: Your first query
description: Start an isolated local server, send three events, and check an exact query result.
---

This tutorial runs Trawl on your Linux machine with a separate PostgreSQL
container and a private working directory. It uses the CLI, not the browser.
Allow about 15 minutes after installing the binaries. You need Docker, Python 3,
`curl`, and `trawl`, `trawld`, `trawl-admin`, and `fleet-admin` in your PATH.
See [Installation](/getting-started/) first.

Use Bash for the commands below. Keep the first terminal open: later commands
reuse its variables. Ports 55439 and 15514 must be free. This creates a fresh
installation; do not substitute a production database or existing data directory.

## Prepare the databases

Trawl uses two databases: Fleet stores keys and roles; Trawl stores history,
saved queries, schedules, and the field catalog. They can share a PostgreSQL
server, but each has its own database and login.

Create a private directory and random tutorial passwords. Nothing prints the
passwords or API tokens to the terminal.

```bash
umask 077
TRAWL_TUTORIAL_DIR=$(mktemp -d /tmp/trawl-tutorial.XXXXXX)
TRAWL_PG_ADMIN_PASSWORD=$(python3 -c 'import secrets; print(secrets.token_hex(24))')
TRAWL_FLEET_PASSWORD=$(python3 -c 'import secrets; print(secrets.token_hex(24))')
TRAWL_APP_PASSWORD=$(python3 -c 'import secrets; print(secrets.token_hex(24))')
printf 'POSTGRES_PASSWORD=%s\n' "$TRAWL_PG_ADMIN_PASSWORD" > "$TRAWL_TUTORIAL_DIR/postgres.env"
docker run --detach --name trawl-docs-postgres \
  --publish 127.0.0.1:55439:5432 \
  --env-file "$TRAWL_TUTORIAL_DIR/postgres.env" postgres:18
```

Wait until PostgreSQL accepts connections. If the container fails to start,
inspect `docker logs trawl-docs-postgres` before continuing.

```bash
for attempt in {1..30}; do
  docker exec trawl-docs-postgres pg_isready -U postgres >/dev/null 2>&1 && break
  sleep 1
done
docker exec trawl-docs-postgres pg_isready -U postgres
```

The last command must say `accepting connections`. Create the database owners
and databases. The generated passwords contain only hexadecimal characters.

```bash
docker exec -i trawl-docs-postgres psql -U postgres -v ON_ERROR_STOP=1 <<SQL
CREATE ROLE fleet LOGIN PASSWORD '$TRAWL_FLEET_PASSWORD';
CREATE ROLE trawl LOGIN PASSWORD '$TRAWL_APP_PASSWORD';
CREATE DATABASE fleet OWNER fleet;
CREATE DATABASE trawl OWNER trawl;
SQL
export DATABASE_URL="postgres://fleet:$TRAWL_FLEET_PASSWORD@127.0.0.1:55439/fleet"
fleet-admin migrate
```

Fleet migrations are an explicit administrative step. `trawld` applies its
own app-state migrations when it starts.

## Create an API key

Create separate identities for reading and sending events. Role names do not
grant permissions by themselves; the permission list defines what each key can do.

```bash
fleet-admin roles create --name tutorial-reader \
  --perm trawl:query --perm trawl:schema_read --perm trawl:validate \
  --perm trawl:export --perm trawl:stream --perm trawl:saved_query
fleet-admin roles create --name tutorial-ingest --perm trawl:ingest
fleet-admin keys create --name tutorial-reader --kind human \
  --role tutorial-reader > "$TRAWL_TUTORIAL_DIR/reader.token"
fleet-admin keys create --name tutorial-ingest --kind service \
  --role tutorial-ingest > "$TRAWL_TUTORIAL_DIR/ingest.token"
unset DATABASE_URL
```

`fleet-admin` writes the token once to standard output and metadata to standard
error. The redirects keep each token in a private file. A reader key cannot
send the sample events; the next steps use the ingest key for that operation.

## Start the server

Generate TLS material and write the complete configuration before starting
`trawld`. The server listens only on this machine. Its data directory belongs
to this tutorial.

```bash
trawl-admin tls generate --output-dir "$TRAWL_TUTORIAL_DIR/tls"
cat > "$TRAWL_TUTORIAL_DIR/trawld.toml" <<TOML
[server]
http_addr = "127.0.0.1:15514"
tls_cert_path = "$TRAWL_TUTORIAL_DIR/tls/cert.pem"
tls_key_path = "$TRAWL_TUTORIAL_DIR/tls/key.pem"
max_concurrent_queries = 2

[data]
path = "$TRAWL_TUTORIAL_DIR/data"

[auth]
database_url = "postgres://fleet:$TRAWL_FLEET_PASSWORD@127.0.0.1:55439/fleet"

[storage]
database_url = "postgres://trawl:$TRAWL_APP_PASSWORD@127.0.0.1:55439/trawl"

[ingest]
enabled = true
internal_telemetry = false
TOML
printf 'env -u FLEET_DATABASE_URL -u TRAWL_DATABASE_URL trawld --config %q\n' "$TRAWL_TUTORIAL_DIR/trawld.toml"
```

Run the printed command in a second terminal and leave it running in the
foreground. The printed command clears database URL overrides for this process
so its config selects the tutorial databases. Back in the first terminal,
check the HTTPS endpoint:

```bash
curl --fail --silent --show-error \
  --cacert "$TRAWL_TUTORIAL_DIR/tls/cert.pem" \
  https://localhost:15514/api/v1/health
```

Expect an HTTP success with JSON health information. If startup fails, read
the second terminal's error. Connection failures usually mean PostgreSQL or
`trawld` is not running; authentication failures during startup mean the
configured database login or migrations need attention.

## Configure your client

Write a separate client config so this tutorial does not replace your usual
server or profiles.

```bash
printf '[server]\nurl = "https://localhost:15514"\ntoken = "%s"\ninsecure = true\n' \
  "$(cat "$TRAWL_TUTORIAL_DIR/reader.token")" > "$TRAWL_TUTORIAL_DIR/client.toml"
unset TRAWL_TOKEN TRAWL_URL TRAWL_PROFILE TRAWL_INSECURE
```

The CLI's `insecure = true` accepts this tutorial's self-signed certificate.
Use a certificate trusted by your client for a permanent installation. The
`curl` commands instead trust the generated certificate explicitly.

## Send some test data

Prepare three events with the current UTC timestamp. The shared service name
keeps the expected result separate from any other source.

```bash
python3 - <<'PYDATA' > "$TRAWL_TUTORIAL_DIR/events.json"
import datetime, json
now = datetime.datetime.now(datetime.timezone.utc).isoformat()
events = [
    {"level": "info", "message": "server started", "duration": 12},
    {"level": "error", "message": "connection refused", "duration": 1500},
    {"level": "warn", "message": "upstream timeout", "duration": 700},
]
for event in events:
    event.update(service="tutorial", host="tutorial-host", timestamp=now)
print(json.dumps(events))
PYDATA
printf 'Authorization: Bearer %s\n' "$(cat "$TRAWL_TUTORIAL_DIR/ingest.token")" \
  > "$TRAWL_TUTORIAL_DIR/ingest.header"
curl --fail --silent --show-error \
  --cacert "$TRAWL_TUTORIAL_DIR/tls/cert.pem" \
  --header @"$TRAWL_TUTORIAL_DIR/ingest.header" \
  --header 'Content-Type: application/json' \
  --data-binary @"$TRAWL_TUTORIAL_DIR/events.json" \
  https://localhost:15514/api/v1/ingest
```

Expect `{"accepted":3}`. Do not resend to fix a display problem: sending the
same payload again stores another three events. The accepted events are
queryable through the hot buffer before Parquet compaction finishes.

## Run your first query

```bash
trawl --config "$TRAWL_TUTORIAL_DIR/client.toml" query --format json \
  'service=tutorial last=1h | stats count() by service'
```

Expected result:

```json
{"service":"tutorial","count":3}
```

JSON property order does not matter. Now select the error event:

```bash
trawl --config "$TRAWL_TUTORIAL_DIR/client.toml" query --format json \
  'service=tutorial _severity>=error last=1h | table message, duration'
```

Expect one row with `message` equal to `connection refused` and `duration`
equal to `1500`. Trawl derives `_severity` from the sender's `level` while
keeping `level` as an ordinary field. See the
[query tutorial](/use/query-tutorial/) to build on this example.

## Launch the TUI

```bash
trawl --config "$TRAWL_TUTORIAL_DIR/client.toml"
```

Enter the same query and use the execute shortcut shown in Help. The TUI uses
the same reader identity. See [CLI and TUI workflows](/use/cli-tui/).
Exit the TUI before cleaning up.

## Try embedded mode (no server)

Export these events and query the file without a server:

```bash
trawl --config "$TRAWL_TUTORIAL_DIR/client.toml" query \
  'service=tutorial last=1h' --format parquet \
  --output "$TRAWL_TUTORIAL_DIR/tutorial.parquet"
trawl query --data "$TRAWL_TUTORIAL_DIR/tutorial.parquet" \
  '* | stats count() by service'
```

See [Local Parquet](/start/local-parquet/) for the differences from server mode.

## Clean up

Stop `trawld` with Ctrl+C in its terminal. Then remove only this tutorial's
container and its anonymous database volume:

```bash
docker rm --force --volumes trawl-docs-postgres
```

The private directory still contains tokens, database passwords, TLS keys,
and the exported data. Once you have finished inspecting it, remove it:

```bash
rm -r -- "$TRAWL_TUTORIAL_DIR"
unset TRAWL_PG_ADMIN_PASSWORD TRAWL_FLEET_PASSWORD TRAWL_APP_PASSWORD TRAWL_TUTORIAL_DIR
```

## What's next

- [Search in the browser](/reference/web-ui/) when your installation has a browser URL.
- [Vector integration](/getting-started/vector-integration/) to send logs continuously.
- [Sharing and export](/use/sharing-export/) to retain or share an answer.
- [Configuration](/reference/configuration/) for a permanent installation.
