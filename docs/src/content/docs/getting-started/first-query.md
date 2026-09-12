---
title: Your first query
description: Start a private Trawl server, send three events, and check an exact query result.
---

In this tutorial we start a private Trawl server on your Linux machine, send it
three events, and check an exact answer from the CLI, the TUI, and a local
Parquet export. Allow about 15 minutes after installation.

You need:

- Linux with Bash, Docker, Python 3, and `curl`.
- `trawl`, `trawld`, `trawl-admin`, and `fleet-admin` in your `PATH`. See [Installation](/getting-started/).
- Ports 55439 and 15514 free.

Keep the first terminal open, because later steps reuse its variables. Do not
reuse an existing database or data directory.

## Start PostgreSQL

Trawl needs two databases. Create a private working directory and random
passwords first. Nothing prints a password or a token.

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

Now wait for PostgreSQL:

```bash
for attempt in {1..30}; do
  docker exec trawl-docs-postgres pg_isready -U postgres >/dev/null 2>&1 && break
  sleep 1
done
docker exec trawl-docs-postgres pg_isready -U postgres
```

The last line must end with `accepting connections`. If not, read
`docker logs trawl-docs-postgres`.

## Create the databases

Create a login and a database for each, then apply the Fleet migrations.
Hexadecimal passwords are safe inside the SQL quotes.

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

Expect four `CREATE` lines, then `fleet-admin: migrations applied`. `trawld`
runs its own app-state migrations at startup.

## Create an API key

One key reads, another sends. Permissions come from the role's list, not its
name.

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

`keys create` prints metadata to stderr and the token to stdout once, so the
redirect keeps it in a private file.

## Start the server

Generate a self-signed certificate and write the configuration.

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
printf 'env -u FLEET_DATABASE_URL -u TRAWL_DATABASE_URL -u TRAWL_HTTP_ADDR trawld --config %q\n' "$TRAWL_TUTORIAL_DIR/trawld.toml"
```

`tls generate` prints the `cert:` and `key:` paths. The last command prints a
`trawld` command line, with `env -u` clearing environment overrides. Run it in
a second terminal and leave it running. Back in the first terminal:

```bash
curl --fail --silent --show-error \
  --cacert "$TRAWL_TUTORIAL_DIR/tls/cert.pem" \
  https://localhost:15514/api/v1/health
```

Expect a JSON object whose `status` is `ok`. If not, read the error in the
second terminal. A connection error points at PostgreSQL, an authentication
error at a database login or the migrations.

## Configure the client

Write a client config for this tutorial only, so your usual
`~/.config/trawl/config.toml` stays untouched.

```bash
printf '[server]\nurl = "https://localhost:15514"\ntoken = "%s"\ninsecure = true\n' \
  "$(cat "$TRAWL_TUTORIAL_DIR/reader.token")" > "$TRAWL_TUTORIAL_DIR/client.toml"
unset TRAWL_TOKEN TRAWL_URL TRAWL_PROFILE TRAWL_INSECURE
```

`insecure = true` accepts the self-signed certificate. Use a trusted
certificate for a permanent installation.

## Send three events

Build three events with the current UTC time, then post them with the ingest
key.

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

Expect `{"accepted":3}`. Send it once, because a second post adds three more
events. The events are visible at once from the hot buffer.

## Run your first query

```bash
trawl --config "$TRAWL_TUTORIAL_DIR/client.toml" query --format json \
  'service=tutorial last=1h | stats count() by service'
```

Expect:

```json
{"service":"tutorial","count":3}
```

Property order can differ. Now select the error event:

```bash
trawl --config "$TRAWL_TUTORIAL_DIR/client.toml" query --format json \
  'service=tutorial _severity>=error last=1h | table message, duration'
```

Expect one row with `message` equal to `connection refused` and `duration`
equal to `1500`. Trawl derived `_severity` from `level`. [Build a query](/use/query-tutorial/)
continues from here.

## Open the TUI

```bash
trawl --config "$TRAWL_TUTORIAL_DIR/client.toml"
```

Press F1 for the shortcut list. Enter the same query and press Shift+Enter or
F5. Expect the same rows. Press Ctrl+Q to quit before you clean up. See
[CLI and TUI workflows](/use/cli-tui/).

## Query the export without a server

Export the events to Parquet, then query the file with no server:

```bash
trawl --config "$TRAWL_TUTORIAL_DIR/client.toml" query \
  'service=tutorial last=1h' --format parquet \
  --output "$TRAWL_TUTORIAL_DIR/tutorial.parquet"
trawl query --data "$TRAWL_TUTORIAL_DIR/tutorial.parquet" \
  '* | stats count() by service'
```

Expect one row with `tutorial` and `3`, then `1 row(s)`. See
[Query local Parquet](/start/local-parquet/).

## Clean up

Stop `trawld` with Ctrl+C in its terminal. Remove the container and its
anonymous volume:

```bash
docker rm --force --volumes trawl-docs-postgres
```

The working directory still holds tokens, passwords, TLS keys, and the export.
Remove it when you are done:

```bash
rm -r -- "$TRAWL_TUTORIAL_DIR"
unset TRAWL_PG_ADMIN_PASSWORD TRAWL_FLEET_PASSWORD TRAWL_APP_PASSWORD TRAWL_TUTORIAL_DIR
```

## What's next

- [Search in the browser](/reference/web-ui/) once you have a browser URL.
- [Vector integration](/getting-started/vector-integration/) sends logs continuously.
- [Share searches and export results](/use/sharing-export/) keeps or shares an answer.
