---
title: Ship logs with Vector
description: Install Vector on a Debian host, load the Trawl configuration, set the key, and prove that the first event arrived.
---

Vector reads journald and `/var/log` files, maps them to the
[event contract](/reference/events/), buffers to disk, and posts gzip batches
to `POST /api/v1/ingest`. This page sets it up on one Debian host with the
configuration shipped in the Trawl repository, then proves that the first
event from each sender arrived.

You need:

- trawld reachable from the host at its HTTPS address, for example
  `https://trawl.example.com:5514`. Vector talks to trawld, not to `trawl-web`.
- A service key with `trawl:ingest` in a file named `vector.token` on the
  host, from [Create roles and keys](/operate/access/#create-roles-and-keys).
  Copy the file to the host over a channel you trust, such as `scp`.
- A trawld certificate the host can verify. See [Configure TLS](/operate/access/#configure-tls).
- The `trawl` CLI on the host, with a profile named `prod` whose key has
  `trawl:query`. The proof queries run in the same shell as the commands that
  send the test events.
- A `pass` verdict from `trawl doctor -p prod` on the host. The doctor checks the
  profile's configuration, the API's transport, TLS, and health, and the
  identity of the profile's key. It never sends an event, so it does not prove
  delivery. It never reads Vector's key either. See
  [Doctor mode](/reference/cli/#doctor-mode).

## Install Vector

1. Add the Vector repository and install the package:

   ```bash
   bash -c "$(curl -L https://setup.vector.dev)"
   sudo apt-get install vector
   ```

2. Let the `vector` user read the journal and `/var/log`:

   ```bash
   sudo usermod -aG systemd-journal,adm vector
   ```

   Add `docker` for `docker.toml`, and the owning group of any application log
   directory you collect.

## Load the Trawl configuration

The repository directory [`config/vector/debian/`](https://github.com/jakub/trawl/tree/main/config/vector/debian)
holds `base.toml` and one drop-in per service: `apache.toml`, `docker.toml`,
`fail2ban.toml`, `mysql.toml`, `nginx.toml`, `postgresql.toml`, `redis.toml`,
and `unifi-syslog.toml`.

1. Copy `base.toml` and only the drop-ins for services on this host:

   ```bash
   sudo mkdir -p /etc/vector/vector.d
   sudo cp base.toml nginx.toml /etc/vector/vector.d/
   ```

   `base.toml` reads journald and `/var/log/**/*.log`, maps `_SYSTEMD_UNIT` to
   `service` and `PRIORITY` to `severity_text`, and defines the `trawld` sink.
   The sink takes input from every final transform named `trawl_*`, so a
   drop-in needs no change to `base.toml`. Use another prefix for intermediate
   transforms, such as `journal_enriched`, to prevent duplicate delivery and
   filter bypass.

   The catch-all always excludes nginx, Apache, PostgreSQL, MySQL, Redis,
   and fail2ban files. Install the corresponding drop-in to collect those
   files, even if the application also writes some events to the journal.
   All shipped file sources use `read_from = "end"`. When Vector first
   discovers a file without a saved checkpoint, it collects newly appended
   lines and skips existing history. On restart, it resumes saved checkpoints.

   Journal collection covers the current boot. By default, every normalized
   journal event passes through. To enable the optional homelab noise policy,
   set `TRAWL_SUPPRESS_HOMELAB_NOISE=true` in `/etc/default/vector` and restart
   Vector. This drops `serial-getty@ttyS0` events, `init` messages containing
   `serial-getty`, and container-network churn from `networkd-dispatcher`,
   `NetworkManager`, and `systemd-networkd`. Review the conditions in
   `transforms.trawl_journal` before enabling them. Leave the variable unset
   to keep these events.

2. Put the ingest key in `/etc/default/vector`. Run these commands in the
   directory that holds `vector.token`:

   <!-- proof:key-write -->
   ```bash
   sudo install -m 0600 -o root -g root /dev/null /etc/default/vector
   printf 'TRAWL_INGEST_TOKEN=%s\n' "$(cat vector.token)" | sudo tee -a /etc/default/vector > /dev/null
   rm vector.token
   ```

   `install` replaces the file with an empty one, owned by root with mode
   0600, before the key is written. The key never appears on a command line:
   `printf` is a shell builtin, and `sudo tee` reads the key from its standard
   input. The last command deletes the copy of the key file.

3. Add the settings to the same file with `sudoedit /etc/default/vector`:

   ```ini
   VECTOR_CONFIG_DIR=/etc/vector/vector.d
   VECTOR_DANGEROUSLY_ALLOW_ENV_VAR_INTERPOLATION=true
   TRAWL_URL=https://trawl.example.com:5514
   TRAWL_ENV=prod
   ```

   The unit reads this file for both `vector validate` and `vector`.
   `VECTOR_CONFIG_DIR` replaces the default `/etc/vector/vector.yaml`. The
   interpolation variable is required: Vector 0.57 and later do not expand
   `${TRAWL_URL}` in a configuration file without it. `TRAWL_ENV` must be in
   the server's `[ingest] envs`. When `TRAWL_ENV` is unset, the shipped
   configuration sends `prod`. To replace the key later, edit the same file
   with `sudoedit` and restart Vector.

   The key stays in this file while Vector runs. systemd reads the file as
   root and passes its variables to Vector as the process environment. Root
   and the `vector` user can read the key there, through
   `/proc/<pid>/environ`. Other users cannot read the file or the environment.
   This boundary is accepted because the `vector` user is the process that
   sends with the key, and the key holds only `trawl:ingest`. Whoever reads it
   can post events and do nothing else. The file-only key rule of the
   [trial](/getting-started/first-query/) does not apply to collector hosts.
   To limit what one leaked key reaches, create one key per collector fleet
   and [revoke it](/operate/access/#rotate-or-revoke-an-api-key) when a host
   leaves.

4. Configure certificate trust before starting Vector. The shipped sink
   verifies the server certificate and the hostname in `TRAWL_URL` against
   the host's system CA store. For a publicly trusted certificate, keep the
   shipped TLS settings.

   For a private CA, obtain its PEM certificate from your CA administrator
   through a trusted channel. Copy the CA certificate to the collector:

   ```bash
   sudo install -m 0644 trawl-ca.pem /etc/vector/trawl-ca.pem
   ```

   Add `ca_file` to the existing `[sinks.trawld.tls]` table in `base.toml`:

   ```toml
   [sinks.trawld.tls]
   verify_certificate = true
   verify_hostname = true
   ca_file = "/etc/vector/trawl-ca.pem"
   ```

   Configure trawld with a certificate whose Subject Alternative Name
   includes the real DNS hostname in `TRAWL_URL`, such as
   `trawl.example.com`. Configure the server certificate chain and key as
   described in [Configure TLS](/operate/access/#configure-tls). The generated
   localhost certificate cannot verify a different hostname, even if you
   trust its issuer. Keep both verification settings enabled on deployed
   collectors.

## Start Vector

Record the start time in UTC, then start and enable the service:

<!-- proof:vector-start -->
```bash
VECTOR_START="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
sudo systemctl enable --now vector
sudo systemctl status vector --no-pager
```

The unit runs `vector validate` before it starts, so a configuration error
shows here. Keep this shell open: the
[first-start history query](#what-arrives-from-before) reads `VECTOR_START`.
Errors from the sink appear in `journalctl -u vector`. See
[Troubleshoot delivery](#troubleshoot-delivery).

The shipped sink behaves as follows:

| Setting | Value |
| --- | --- |
| Batch | 1 MB or 5 seconds, gzip |
| Buffer | Disk, 1 GB under `/var/lib/vector`. Sources block when it is full. |
| Retries | `5xx`, `408`, and `429`, with 1 to 30 second backoff. Other `4xx` responses drop the batch. |
| Concurrency | Adaptive |
| Acknowledgements | Enabled. A source advances only after trawld accepts the batch. |

## Prove the first event arrived

A running Vector and a healthy trawld do not prove that events arrive. To
prove it, make the sender emit one event that carries a unique marker, then
query for that marker. Every recipe below uses a query of this shape:

```text
env=<env> service=<service> host=<host> "<marker>" last=1d _ingested>="<T0>" | head 20 | table _time, _ingested, _producer, env, service, host, message
```

Each part of the query has a job:

- `_ingested>="<T0>"` matches only an event that arrived after you captured
  `T0`. `_ingested` is the time trawld received the event. `_time` is the
  time the sender asserts, and it can be old: nginx takes it from the access
  line, and a backfilled journal entry keeps its original time.
- `last=1d` bounds the scan. Only `last=` prunes date directories. `earliest=`,
  `_time>=`, `_ingested>=`, and `host=` do not, so a query without `last=`
  reads every retained date. The proof covers events whose `_time` falls in
  the last day. An event from a host whose clock is far off, or a backfilled
  line older than a day, falls outside that window. To find it, widen `last=`.
- The marker is a phrase. A phrase matches a case-insensitive substring of
  `message` and `_raw`, and `%` and `_` in it act as wildcards. Keep markers
  uuid-shaped, with letters, digits, and hyphens only.
- `env`, `service`, and `host` prove that the event arrived with the identity
  you expect.

A check on a short `last=` window alone, such as 15 minutes, is wrong in
both directions. It misses an event whose `_time` is old or skewed, and it
passes on an event from an earlier run.

The check passes on at least one row. Vector retries can deliver an event
twice, so do not expect exactly one. Marker events stay stored until
retention removes them. They are harmless, but keep anything secret out of a
marker.

The CLI is the canonical check. The browser's Search page adds the range
picker's own filter on `_time`, so a range snapshot can hide an event that the
CLI query finds.

### Set the variables

Run this block before each recipe, in the shell on the collector. Set
`SENDER_ENV` to the value of `TRAWL_ENV` in `/etc/default/vector`:

<!-- proof:vars -->
```bash
SENDER_ENV=prod
HOST="$(hostname)"
MARKER="trawl-check-$(cat /proc/sys/kernel/random/uuid)"
T0="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
```

`HOST` is the name Vector sets as `host` on journal and file events. `T0` is
UTC with a `Z` suffix. trawld drops a UTC offset such as `+02:00` when it
compares a time literal, so a local time compares as the wrong instant. `T0`
comes from the collector's clock and `_ingested` from trawld's clock. If the
collector's clock runs ahead of trawld's by more than the delivery delay, the
check misses the event. Keep both clocks in sync with NTP.

Vector sends a batch every 5 seconds. If a check returns no row, run it again
after a few seconds. If it still returns no row after a minute, see
[Troubleshoot delivery](#troubleshoot-delivery).

### journald

1. Run a transient unit that prints the marker:

   <!-- proof:journald-send -->
   ```bash
   sudo systemd-run --collect --quiet --unit "$MARKER" /bin/echo "$MARKER"
   ```

   The unit name becomes `service`. Do not use `logger` from an SSH session
   for this check. The journal records the session scope as the unit, so
   `service` becomes `session-N`.

2. Query for the marker:

   <!-- proof:journald-check -->
   ```bash
   trawl -p prod query "env=$SENDER_ENV service=$MARKER host=$HOST \"$MARKER\" last=1d _ingested>=\"$T0\" | head 20 | table _time, _ingested, _producer, env, service, host, message"
   ```

   Expect a row with `service` and `message` equal to the marker, `host`
   equal to `HOST`, and `_producer` = `http`.

### nginx

This recipe needs `nginx.toml` in `/etc/vector/vector.d/`. The drop-in reads
`/var/log/nginx/*.log` and parses access lines in the `combined` format.

1. Request a path that carries the marker:

   <!-- proof:nginx-send -->
   ```bash
   curl -sS -o /dev/null -w '%{http_code}\n' "http://localhost/$MARKER"
   ```

   The path does not exist, so nginx answers `404` and writes the access
   line. To prove a specific virtual host, request the same path on that
   host's name instead.

2. Confirm that nginx wrote the line:

   <!-- proof:nginx-confirm -->
   ```bash
   sudo grep -F -- "$MARKER" /var/log/nginx/access.log
   ```

   If no line appears, the event can never reach trawld. Look for these
   causes:

   - The virtual host sets `access_log off`.
   - `access_log` has a `buffer=` or `flush=` parameter, so the line waits
     in memory.
   - The virtual host logs to a file outside `/var/log/nginx/`.
   - The request reached a different virtual host than the one you meant.

   A custom `log_format` does not stop delivery, but the `combined` parser
   fails on it. The event then arrives with `message` and without `uri` or
   `status`.

3. Query for the marker:

   <!-- proof:nginx-check -->
   ```bash
   trawl -p prod query "env=$SENDER_ENV service=nginx host=$HOST \"$MARKER\" last=1d _ingested>=\"$T0\" | head 20 | table _time, _ingested, _producer, env, service, host, uri, status, message"
   ```

   Expect a row with `service` = `nginx`, `uri` equal to `/` followed by the
   marker, and `status` = `404`. `_time` comes from the access line.

### Docker

This recipe needs `docker.toml` in `/etc/vector/vector.d/` and the `vector`
user in the `docker` group.

1. Start a detached, named container that waits, then prints the marker:

   <!-- proof:docker-send -->
   ```bash
   sudo docker run -d --name "$MARKER" alpine sh -c "sleep 15; echo $MARKER"
   ```

   The wait gives Vector time to attach to the new container before the line
   is written. Do not add `--rm`: Docker deletes the log with the container,
   and that can happen before Vector reads it. `docker exec … echo` does not
   work either, because its output goes to the caller, not to the container
   log.

2. Wait about 30 seconds, then query for the marker:

   <!-- proof:docker-check -->
   ```bash
   trawl -p prod query "env=$SENDER_ENV service=$MARKER host=$MARKER \"$MARKER\" last=1d _ingested>=\"$T0\" | head 20 | table _time, _ingested, _producer, env, service, host, container_name, image, message"
   ```

   Docker events carry no machine name. `docker.toml` sets `service` and
   `host` from the container's identity:

   | Container | `service` | `host` |
   | --- | --- | --- |
   | Started by Compose | The Compose service | The Compose project |
   | A Swarm task | The Swarm service, with `_` replaced by `-` | The stack name |
   | Any other | The container name | The container name |

   The test container has no labels, so expect `service` and `host` equal to
   the marker.

3. When the check passes, remove the container:

   <!-- proof:docker-cleanup -->
   ```bash
   sudo docker rm "$MARKER"
   ```

### Host firewall (UFW)

`base.toml` reads UFW's kernel log lines from the journal and sets
`service` = `ufw`, `src_ip`, `dst_ip`, `src_port`, `dst_port`, and
`protocol`. A UFW line has no free text, so the recipe uses the source
address and the destination port as its marker.

Only a real packet proves that the firewall writes its log line. The packet
must come from another machine: UFW does not log loopback traffic. You need:

- UFW active, with logging on. `sudo ufw status verbose` shows both.
- A second machine that can reach this host.
- A port that no service listens on and no Docker container publishes. Docker
  sends a published port's traffic through its own forwarding rules, which
  UFW's rules do not filter.

1. Run the variables block from [Set the variables](#set-the-variables). Then
   set the addresses and pick a port:

   <!-- proof:ufw-vars -->
   ```bash
   PEER=192.0.2.20
   COLLECTOR=192.0.2.10
   PORT="$(shuf -i 40000-59999 -n 1)"
   echo "COLLECTOR=$COLLECTOR PORT=$PORT"
   ```

   `PEER` is the other machine's address as this host sees it. `COLLECTOR` is
   this host's address as the other machine reaches it.

2. Add a rule that blocks and logs that port from that machine:

   <!-- proof:ufw-rule -->
   ```bash
   sudo ufw prepend deny log from "$PEER" to any port "$PORT" proto tcp
   ```

   `prepend` puts the rule before any rule that allows the traffic. `log`
   writes a kernel log line for each new connection that the rule matches.

3. On the other machine, paste the line that step 1 printed, then try to
   connect:

   <!-- proof:ufw-send -->
   ```bash
   timeout 5 bash -c "echo > /dev/tcp/$COLLECTOR/$PORT" || echo "blocked, as expected"
   ```

4. On this host, query for the packet:

   <!-- proof:ufw-check -->
   ```bash
   trawl -p prod query "env=$SENDER_ENV service=ufw host=$HOST src_ip=$PEER dst_port=$PORT last=1d _ingested>=\"$T0\" | head 20 | table _time, _ingested, _producer, env, service, host, src_ip, dst_port, protocol, message"
   ```

   Expect a row with `src_ip` equal to `PEER` and `dst_port` equal to `PORT`.
   TCP resends a blocked connection attempt, so several rows can match.

5. When the check passes, delete the rule with
   `sudo ufw delete deny log from "$PEER" to any port "$PORT" proto tcp`.

### Network firewall appliance

An appliance sends syslog directly to trawld, not through Vector. See
[Receive syslog](/operate/ingestion/#receive-syslog).

## What arrives from before

On first start, some senders also deliver events that were written before
Vector started:

| Sender | First start | Later restarts |
| --- | --- | --- |
| journald (`base.toml`) | Every entry of the current boot, because `current_boot_only = true` | Resumes from its checkpoint |
| File drop-ins, such as nginx and the `/var/log` catch-all | For files present when Vector starts, only lines appended after that, because `read_from = "end"`. A file that appears later, such as one moved into `/var/log`, is read from its beginning | Resumes from its checkpoint |
| Docker (`docker.toml`) | Lines written after Vector starts. Vector 0.57 `docker_logs` keeps no cursor and has no `since_now` setting | The same. Lines written while Vector was down are not collected |
| A syslog device sending to trawld | Only what the device sends after you point it at trawld | Only what the device sends while trawld listens |

A host that has run for months can hold millions of journal entries in the
current boot. Vector sends them all on first start, in 1 MB batches, as fast
as trawld accepts them. The backfill can take minutes. A proof event waits
behind it, and backfilled entries keep their original `_time`.

To see what arrived from before, run this query in the shell that started
Vector, after [Set the variables](#set-the-variables):

<!-- proof:history-finder -->
```bash
BOOT_DAYS=$(( ( $(date +%s) - $(date -d "$(uptime -s)" +%s) ) / 86400 + 1 ))
trawl -p prod query "env=$SENDER_ENV host=$HOST last=${BOOT_DAYS}d _ingested>=\"$VECTOR_START\" | stats count(), min(_time), max(_time) by service"
```

`BOOT_DAYS` makes the `last=` window reach back past the boot time that
`uptime -s` prints, so the window covers the whole journal backfill. As in the
proof queries, `last=` also bounds the scan. `_ingested>="$VECTOR_START"`
keeps only events that arrived after Vector started. A service whose
`min(_time)` is earlier than `VECTOR_START` delivered events from before.
Docker events do not appear here, because their `host` is the container's
identity.

## Troubleshoot delivery

Vector logs sink errors to its journal. Read them with `journalctl -u vector`.
Some failures leave no error in that journal, because trawld answers `200` and
counts the rejection on `/metrics` instead. The rejection counter is
`trawl_ingest_events_rejected_total{reason}`. See
[Confirm delivery over time](/operate/ingestion/#confirm-delivery-over-time).

| What you see | What to do |
| --- | --- |
| Vector logs `404` from the `trawld` sink | Check whether `TRAWL_URL` is the `trawl-web` origin. `trawl-web` answers `404` on `/api/v1/ingest`. Set `TRAWL_URL` to trawld's HTTPS address. Vector drops a batch that gets a `404`. |
| Vector logs `401` | The key in `TRAWL_INGEST_TOKEN` is wrong, missing, revoked, or expired. Replace it with `sudoedit /etc/default/vector` and restart Vector. Vector does not retry a `401`. |
| Vector logs `403` | The key lacks `trawl:ingest`. Give its role that permission, or create a key with the `trawl-ingest` role. Vector does not retry a `403`. |
| Vector logs no error, the check finds nothing, and the rejection counter with `reason="invalid_env"` rises | `TRAWL_ENV` fails the env name rule: 1 to 32 characters from `a-z`, `0-9`, `_`, and `-`. `Prod` fails it. Fix `TRAWL_ENV` and restart Vector. |
| Vector logs no error, the check finds nothing, and the rejection counter with `reason="env_not_allowed"` rises | `TRAWL_ENV` is not in trawld's `[ingest] envs`. Keys are not scoped to an env, so the list is the only check. Add the env to the list and restart trawld, or fix `TRAWL_ENV`. |
| The check finds nothing, and the rejection counter rises with another `reason` | For a rule an event broke, trawld answered `200` with `rejected` in the body, and Vector counted the batch as a success. The `reason` label names the rule. See the [event contract](/reference/events/). Three reasons refuse the whole request instead, and Vector logs the status. |
| Vector logs `503`, and `reason="hot_buffer_full"` rises | trawld's hot buffer is full. Vector retries. If it persists, check compaction and disk headroom on the [Health page](/operate/health/). |
| Vector logs `413`, and `reason="ingest_batch_too_large"` rises | One request exceeds trawld's per-request limit. Vector drops that batch. Lower the sink's `batch.max_bytes`. |
| Vector logs `500`, and `reason="wal_failure"` rises | trawld could not write its WAL. Vector retries. Check trawld's journal and the data directory's disk. |
| The event arrives with `env.defaulted` in `_repairs` | The event arrived with no `env`, so trawld used `[ingest] default_env`. The shipped configuration always sends `env`, and an unset `TRAWL_ENV` sends `prod`. A custom transform dropped `.env`. Set `.env = "${TRAWL_ENV:-prod}"` in it. |
| Vector logs a certificate verification or hostname error | trawld's generated certificate is valid only for `localhost`, `127.0.0.1`, and `::1`, so no other host can verify it. Give trawld a certificate for the `TRAWL_URL` hostname, as in [Configure TLS](/operate/access/#configure-tls). Get a private CA certificate over a channel you trust. A CA certificate fetched from the endpoint you are configuring proves nothing, because an attacker in the path serves their own. |
| Vector logs a connection refused or timeout error | The host or port in `TRAWL_URL` is wrong, trawld is not running, or a firewall blocks the port. |

## Receive UniFi syslog

Deploy `unifi-syslog.toml` and point the devices at the Vector host on UDP port
1514. To also receive TCP on that port, uncomment the complete
`sources.unifi_syslog_tcp` block. The `unifi_syslog*` input sends both sources
through the same normalizer before HTTP forwarding.

For a gateway with a fixed service name, enable the gateway override in that
normalizer and set its source IP. The daemon's `source_service_map` applies
when devices send directly to trawld's native syslog listener. It does not
map the events that Vector forwards over HTTP.

## Add your own source

Name only the final transform `trawl_*` so the sink picks it up. Intermediate
parsers, routes, and filters must use another prefix:

```toml
[sources.myapp]
type = "file"
include = ["/var/log/myapp/*.log"]
read_from = "end"

[transforms.trawl_myapp]
type = "remap"
inputs = ["myapp"]
source = '''
.service = "myapp"
.env = "${TRAWL_ENV:-prod}"
del(.source_type)
del(.file)
'''
```

Vector's sources set `timestamp` and `host`, and trawld derives `_time` from
`timestamp`. Set `severity_text`, `severity`, or `level` from the line when
the source has one. trawld derives `_severity` from the first that maps.
`service`, `env`, `host`, and `message` are the envelope, and names that start
with `_` belong to Trawl. Test one host with the
[proof pattern](#prove-the-first-event-arrived) before you roll a new mapping
out to every sender.
