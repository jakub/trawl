---
title: Ship logs with Vector
description: Install Vector on a Debian host, load the Trawl configuration, set the key, and prove that the first event arrived.
---

Vector reads journald and `/var/log` files, maps them to the
[event contract](/reference/events/), buffers to disk, and posts gzip batches
to `POST /api/v1/ingest`. This page sets it up on one Debian host with the
configuration that the `trawl-server` package ships, then proves that the
first event from each sender arrived.

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
- For the [sample preview](#preview-a-sample), a CLI profile named `ops`
  whose key has `trawl:server_manage`. It can be on another machine.
- A `pass` verdict from `trawl doctor -p prod` on the host. The doctor checks the
  profile's configuration, the API's transport, TLS, and health, and the
  identity of the profile's key. It never sends an event, so it does not prove
  delivery. It never reads Vector's key either. See
  [Doctor mode](/reference/cli/#doctor-mode).

## Install Vector

1. Add the Vector repository, then install Vector 0.57.0 and hold it at that
   version:

   ```bash
   bash -c "$(curl -L https://setup.vector.dev)"
   sudo apt-get install vector=0.57.0-1 && sudo apt-mark hold vector
   ```

   Trawl's CI runs the shipped configuration and the
   [sample capture](#preview-a-sample) on Vector 0.57.0. Vector 0.58 removed
   the `vector config` command that the capture uses to check its copy.
   `apt-mark hold` keeps `apt upgrade` from moving Vector past the tested
   version. Moving to 0.58 or later is a deliberate change of CI and this
   guide together.

2. Let the `vector` user read the journal and `/var/log`:

   ```bash
   sudo usermod -aG systemd-journal,adm vector
   ```

   Add `docker` for `docker.toml`, and the owning group of any application log
   directory you collect.

## Load the Trawl configuration

The `trawl-server` package installs the configuration in
`/usr/share/doc/trawl-server/examples/vector/`: `base.toml` and one drop-in
per service: `apache.toml`, `docker.toml`, `fail2ban.toml`, `mysql.toml`,
`nginx.toml`, `postgresql.toml`, `redis.toml`, and `unifi-syslog.toml`.

1. Copy `base.toml` and only the drop-ins for services on this host. On the
   trawld host:

   ```bash
   sudo mkdir -p /etc/vector/vector.d
   sudo install -m 0644 -t /etc/vector/vector.d \
     /usr/share/doc/trawl-server/examples/vector/base.toml \
     /usr/share/doc/trawl-server/examples/vector/nginx.toml
   ```

   A collector host without `trawl-server` installed has no copy. Copy the
   files from the trawld host, so they match the server's release:

   ```bash
   scp trawl.example.com:/usr/share/doc/trawl-server/examples/vector/base.toml \
     trawl.example.com:/usr/share/doc/trawl-server/examples/vector/nginx.toml .
   sudo mkdir -p /etc/vector/vector.d
   sudo install -m 0644 -t /etc/vector/vector.d base.toml nginx.toml
   ```

   Some images install no files under `/usr/share/doc`, such as Debian's slim
   container images, so even the trawld host has no copy. Download the files
   for this release from
   [`config/vector/debian/`](https://github.com/jakub/trawl/tree/{{release.tag}}/config/vector/debian)
   instead.

   `base.toml` reads journald and every `*.log` file under `/var/log`, except
   in `/var/log/private`, which systemd keeps root-only, and `/var/log/trawl`,
   which `trawld.service` keeps owner-only. trawld's own events arrive through
   its internal telemetry instead. It derives `service`
   as [Service names](#service-names) describes, maps `PRIORITY` to
   `severity_text`, and defines the `trawld` sink.
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
   set `TRAWL_SUPPRESS_HOMELAB_NOISE=true` in `/etc/vector/trawl.env` and
   restart Vector. This drops events whose `systemd_unit` is
   `serial-getty@ttyS0.service`, `init` messages containing `serial-getty`,
   and container-network churn from `networkd-dispatcher`, `NetworkManager`,
   and `systemd-networkd`. Other serial gettys, such as
   `serial-getty@ttyS1.service`, still arrive. Review the conditions in
   `transforms.trawl_journal` before enabling them. Leave the variable unset
   to keep these events.

   The package updates only the examples directory. After an upgrade of
   `trawl-server`, copy the files again, as in
   [Upgrade the configuration](#upgrade-the-configuration).

2. Put the ingest key in `/etc/vector/trawl.env`. Run these commands in the
   directory that holds `vector.token`:

   <!-- proof:key-write -->
   ```bash
   sudo install -m 0600 -o root -g root /dev/null /etc/vector/trawl.env &&
     printf 'TRAWL_INGEST_TOKEN=%s\n' "$(cat vector.token)" | sudo tee -a /etc/vector/trawl.env > /dev/null &&
     rm vector.token
   ```

   `install` replaces the file with an empty one, owned by root with mode
   0600, before the key is written. The key never appears on a command line:
   `printf` is a shell builtin, and `sudo tee` reads the key from its standard
   input. The commands are chained with `&&`, so the key is written only into
   the restricted file, and the copy in `vector.token` is deleted only after
   the write succeeds.

3. Add the settings to the same file with `sudoedit /etc/vector/trawl.env`:

   ```ini
   VECTOR_CONFIG_DIR=/etc/vector/vector.d
   VECTOR_DANGEROUSLY_ALLOW_ENV_VAR_INTERPOLATION=true
   TRAWL_URL=https://trawl.example.com:5514
   TRAWL_ENV=prod
   ```

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

4. Make the Vector service read the file, with a systemd drop-in:

   <!-- proof:vector-dropin -->
   ```bash
   sudo install -d -m 0755 /etc/systemd/system/vector.service.d &&
     printf '[Service]\nEnvironmentFile=/etc/vector/trawl.env\n' | sudo tee /etc/systemd/system/vector.service.d/trawl.conf > /dev/null &&
     sudo systemctl daemon-reload
   ```

   The drop-in adds `/etc/vector/trawl.env` to the environment of the
   packaged unit, for both the `vector validate` it runs before it starts and
   `vector` itself. Do not put the settings in `/etc/default/vector`, which
   the unit also reads. The Vector package ships that file as a dpkg
   conffile: after you edit it, the next upgrade of Vector stops at a prompt
   that asks which version to keep, and an unattended upgrade cannot answer
   it. The drop-in and `trawl.env` belong to no package, so upgrades leave
   them alone.

5. Configure certificate trust before starting Vector. The shipped sink
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

   On the trawld host itself, Vector can use trawld's generated certificate,
   which is valid for `localhost`. Set `TRAWL_URL=https://localhost:5514` in
   `/etc/vector/trawl.env`, and copy the certificate where the `vector` user
   can read it:

   ```bash
   sudo install -m 0644 /var/lib/trawl/tls/cert.pem /etc/vector/trawl-ca.pem
   ```

   Then add the same `ca_file` line to `[sinks.trawld.tls]`. Vector needs a
   copy because only the `trawl` user and group can open `/var/lib/trawl`.
   trawld generates a new pair when its key file is missing or exposed, and
   when it upgrades an older layout, as
   [Install the Debian package](/operate/deployment/#install-the-debian-package)
   describes. After that, Vector refuses the new certificate: copy it again
   with the same command and run `sudo systemctl restart vector`.

## Preview a sample

Before you start Vector, check what trawld would do to the events this host
sends. Vector parses and reshapes each line before it posts it, so a raw log
file is not a sample. Capture what Vector would post, then run the
[ingest preview](/operate/ingestion/#ingest-preview) on it. The preview stores
nothing.

1. Capture up to 500 events and 128 KiB:

   <!-- proof:capture-sample -->
   ```bash
   CAPTURE="$(mktemp -d)" &&
   mkdir "$CAPTURE/config" "$CAPTURE/data" &&
   (
     for file in /etc/vector/vector.d/*.toml; do
       case "$(basename "$file")" in
         base.toml | unifi-syslog.toml) ;;
         *) cp "$file" "$CAPTURE/config/" || exit ;;
       esac
     done
   ) &&
   awk -v data_dir="$CAPTURE/data" -v since="${CAPTURE_SINCE-}" '
     BEGIN { print "data_dir = \"" data_dir "\"" }
     /^\[/ { skip = /^\[sinks\.trawld[].]/ }
     /^data_dir *=/ { next }
     !skip
     /^\[sources\.journald\]/ && since != "" { print "extra_args = [\"--since=" since "\"]" }
   ' /etc/vector/vector.d/base.toml > "$CAPTURE/config/base.toml" &&
   cat >> "$CAPTURE/config/base.toml" <<'EOF' &&

   [sinks.capture]
   type = "console"
   inputs = ["trawl_*"]
   target = "stdout"
   encoding.codec = "json"
   EOF
   cat > "$CAPTURE/check.vrl" <<'EOF' &&
   sinks = object!(.sinks)
   keys(sinks) == ["capture"] && sinks.capture.type == "console" &&
     .data_dir == get_env_var!("CAPTURE") + "/data"
   EOF
   sudo sh -c '
     if ! vector config --help > /dev/null 2>&1 || ! vector vrl --help > /dev/null 2>&1; then
       echo "capture: this Vector has no vector config or vector vrl command (Vector 0.58 removed vector config); install vector=0.57.0-1 as in Install Vector. Vector did not start" >&2
       exit 1
     fi
     set -a && . /etc/vector/trawl.env && set +a
     checked="$(vector config --config-dir "$1/config" |
       CAPTURE="$1" vector vrl --input /dev/stdin --program "$1/check.vrl")"
     if [ "$checked" != true ]; then
       echo "capture: the copy has a sink other than capture, or another data_dir; Vector did not start" >&2
       exit 1
     fi
     { timeout 60 vector --config-dir "$1/config"; echo "$?" > "$1/vector.status"; } |
       LC_ALL=C awk -v max=131072 "{ size += length + 1; if (size > max) exit; print }" |
       head -n 500
     written=$?
     if [ "$written" -ne 0 ]; then
       echo "capture: writing the capture stopped with status $written; the capture is incomplete, do not preview it" >&2
       exit 1
     fi
     status="$(cat "$1/vector.status")"
     case "$status" in
       0 | 124) ;;
       *)
         echo "capture: Vector stopped with status $status; the capture is incomplete, do not preview it" >&2
         exit 1
         ;;
     esac
   ' sh "$CAPTURE" > "$CAPTURE/capture.ndjson" &&
   if [ ! -s "$CAPTURE/capture.ndjson" ]; then
     echo "capture: the capture is empty: no event arrived, or the first event alone is larger than 131072 bytes" >&2
     false
   fi
   ```

   The capture runs a copy of this host's configuration with two changes:

   - The `trawld` sink is replaced by a `console` sink that writes each event
     as one JSON line, the same JSON the `trawld` sink posts. Nothing is sent
     to trawld.
   - `data_dir` points at a new, empty directory. The capture keeps its
     checkpoints there, so it does not move the service's checkpoints in
     `/var/lib/vector` or touch the sink's disk buffer.

   The first `awk` filter removes only `[sinks.trawld]` tables whose headers
   start at the beginning of a line, as the shipped `base.toml` writes them.
   Before Vector starts, the block checks that this Vector has the
   `vector config` and `vector vrl` commands. Vector 0.58 removed
   `vector config`, so on a newer Vector the block prints
   `capture: this Vector has no vector config or vector vrl command` with the
   version to install, exits `1`, and Vector does not start. Then it checks
   the copy as Vector reads it:
   `vector config` resolves the configuration, and `check.vrl` accepts it
   only if the one sink is the `console` sink `capture` and `data_dir` is the
   new directory. If you have reformatted `base.toml`, or another file in
   `/etc/vector/vector.d` adds a sink, the check prints an error and exits
   `1`, and Vector does not start. Remove that sink from the copy in
   `$CAPTURE/config`, then run the `sudo` command again.

   `unifi-syslog.toml` stays out of the copy: its events come from devices,
   and its listener would compete with a running Vector for port 1514.
   Vector runs as root with the variables from `/etc/vector/trawl.env`, so
   `TRAWL_ENV` and the noise policy apply as they do in the service. The
   capture stops at 500 events, before the event that would take it past
   131072 bytes, or after 60 seconds, whichever comes first. 131072 bytes is
   128 KiB, the default `[server] max_request_body_bytes` that the preview
   reads, and the cut always falls between two events. Vector's log goes to
   the terminal, and only events go to `$CAPTURE/capture.ndjson`. `mktemp -d`
   creates `$CAPTURE` readable only by you, so the captured log lines stay
   private to your account and root.

   Each step runs only if the step before it succeeded. If a file in
   `/etc/vector/vector.d` cannot be copied, for example because you cannot
   read it, the block stops with the `cp` error and Vector does not start.
   A partial copy would capture with a configuration that the service does
   not run. The block does not exit your shell, so you can fix the cause and
   run it again.

   A capture that ends at 500 events, at the byte limit, at the end of its
   input, or at the 60 second limit is complete. When the event or byte limit
   ends it, Vector logs `ERROR` lines as its components stop. Which lines
   appear, and how many, depends on the sources in the copy and on timing.
   They include `Error writing to output. Stopping sink.` with
   `Broken pipe (os error 32)`, `An error occurred that Vector couldn't
   handle:` followed by `the task completed with an error.` or
   `receiver disconnected.`, and `FinalizerSet task ended prematurely.`
   They are expected: the capture closed Vector's output on purpose. The
   block reports a capture that you must not preview in its own messages,
   which start with `capture:`. If no event fits,
   because none arrived or the first event alone is larger than 131072 bytes,
   the block prints `capture: the capture is empty` and returns `1`.

   If Vector stops for another reason, such as a
   crash or a kill, the `sudo` command prints
   `capture: Vector stopped with status N` and exits `1`. If writing
   `$CAPTURE/capture.ndjson` fails, for example on a full disk, it prints
   `capture: writing the capture stopped with status N` and exits `1`.
   In both cases `$CAPTURE/capture.ndjson` can hold some events. Do not
   preview them. Fix the cause shown in the error or in Vector's log, then
   run the `sudo` command again.

   The empty `data_dir` has no journal checkpoint, so the journald source
   starts at the beginning of the current boot, as on Vector's
   [first start](#what-arrives-from-before). On a host that has run for days,
   the sample then holds only the boot's first lines. To sample recent
   journal lines instead, set `CAPTURE_SINCE` in the same shell before you run
   the capture block. If you already ran it, delete that capture first, as in
   step 3:

   <!-- proof:capture-recent -->
   ```bash
   CAPTURE_SINCE=-15min
   ```

   The block then adds `extra_args = ["--since=-15min"]` to
   `[sources.journald]` in the copy, so journalctl starts 15 minutes back in
   the current boot. Any value that `journalctl --since` accepts works. The
   same checks and limits apply. Run `unset CAPTURE_SINCE` to sample the
   whole boot again.

   The file sources read only lines appended while the capture runs. To
   sample a file source, write to its log during the capture, for example
   with the request from the [nginx recipe](#nginx).

2. Preview the capture:

   ```bash
   trawl -p ops preview-ingest "$CAPTURE/capture.ndjson"
   ```

   The preview needs a key with `trawl:server_manage`, such as
   [an operator key](/operate/access/#create-roles-and-keys) saved in the CLI
   profile `ops`. The `prod` profile's query key gets `403`. If the `ops`
   profile is on another machine, copy `$CAPTURE/capture.ndjson` there.

   The command exits `0` when every event is accepted, and `1` when any event
   is rejected. It exits `2` when it gets no report, for example when the key
   lacks the permission. Look at each rejected row's reason, and at the repairs and
   the `_time` and `_severity` sources of the accepted rows. For example, a
   `_time` of `arrival (bad timestamp)`, with the `time.from_ingest` repair,
   means that `timestamp` did not parse, so trawld would use the arrival time.
   Fix the configuration and capture again until the
   preview shows what you expect. The shipped configuration sets `host` on
   every event, so the preview needs no `--peer-ip`.

   The preview reads at most `[server] max_request_body_bytes`, 128 KiB by
   default, and the capture stops below that. If trawld runs with a lower
   limit, the preview answers `413 request_too_large`. A large body can also
   show up as `network error: the server closed the connection before the
   upload finished`: trawld hangs up on an oversized upload, and the reset can
   arrive before the `413`. The remedy is the same. Set `max=` in the capture
   block's second `awk` to trawld's limit in bytes, and capture again.

3. Delete the capture. It holds real log lines, and Vector wrote its
   checkpoints as root:

   ```bash
   sudo rm -rf "$CAPTURE"
   ```

A preview checks canonicalization, not delivery. Prove that the first event
arrived after you start Vector.

## Start Vector

Save trawld's rejection counter first, so that you can
[check for refused events](#check-for-refused-events) after the start. Use
the address in `TRAWL_URL`. `/metrics` needs no key:

```bash
body="$(curl -fsS https://trawl.example.com:5514/metrics)" && printf '%s\n' "$body" | grep '^trawl_ingest_events_rejected_total' > rejected-before.txt || { rm -f rejected-before.txt; echo "no baseline saved: fix the address or add --cacert, then rerun" >&2; false; }
```

For a private CA, or for trawld's generated certificate, add
`--cacert /etc/vector/trawl-ca.pem`. A scrape that fails, or that returns no
counter line, saves no file and prints the error above. Fix the cause and run
the command again before you start Vector.

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
| Acknowledgements | Enabled. The journald and file sources advance only after trawld accepts the batch. Vector 0.57's `docker_logs` source does not support acknowledgements, so a Docker line in a batch that trawld refuses is lost. |

### Check for refused events

trawld checks each event on its own. It refuses an event that breaks the
[event contract](/reference/events/#rejection-reasons), stores the rest of
the batch, and answers `200`. Vector reads only the status code, so it logs
no error, and the refused event is lost. trawld counts each refusal in
`trawl_ingest_events_rejected_total{reason}`.

Wait at least five minutes after the start. On a host with a long journal,
wait until the [first-start backfill](#what-arrives-from-before) is done.
Then compare the counter with the copy you saved:

```bash
if [ -s rejected-before.txt ] && now="$(curl -fsS https://trawl.example.com:5514/metrics)"; then printf '%s\n' "$now" | grep '^trawl_ingest_events_rejected_total' | diff rejected-before.txt - && echo "no new rejections"; else echo "cannot compare: no saved baseline, or the scrape failed; fix the address or add --cacert, then rerun" >&2; false; fi
```

Expect `no new rejections`. A changed line names the `reason` that rose. The
message `cannot compare` means the check observed nothing: the baseline file is
missing or empty, or the scrape failed. For a scrape error, fix the address or
add `--cacert`, then run the check again. Without a baseline, save one now with
the command above and check again later. A fresh baseline does not count the
refusals from before it, so [preview a sample](#preview-a-sample) as well. Only
the `no new rejections` line is a pass. See
[Troubleshoot delivery](#troubleshoot-delivery). The counter covers every
sender of this trawld, so a rise can come from another host.
[Preview a sample](#preview-a-sample) from this host to tell. When the check
passes, delete the file with `rm rejected-before.txt`.

The [operational alert pack](/operate/operational-alerts/#ingest-events-rejected)
warns when this counter rises.

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
`SENDER_ENV` to the value of `TRAWL_ENV` in `/etc/vector/trawl.env`:

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
   sudo sh -c 'grep -F -- "$1" /var/log/nginx/*.log' sh "$MARKER"
   ```

   This searches the same files the nginx drop-in reads,
   `/var/log/nginx/*.log`, so a virtual host with its own log file there
   counts too.

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

## Service names

trawld accepts a `service` of 1 to 128 ASCII letters, digits, `.`, `_`, and
`-` that does not start with a dot. It refuses any other event, and Vector
logs no error (see [Check for refused events](#check-for-refused-events)).
Every shipped transform that takes `service` from the event therefore checks
the value against that rule before it sends it. A value that fails is never
rewritten, unescaped, or cut. The transform uses its next candidate or a
marked fallback, and the original values stay on the event:

| Source | `service` | Original values kept | Fallback |
| --- | --- | --- | --- |
| journald (`base.toml`) | The first valid candidate: the unit, then `SYSLOG_IDENTIFIER`, then `_COMM` | `systemd_unit`, `systemd_user_unit`, `syslog_identifier`, `comm` | `journal-unidentified` |
| `/var/log` files (`base.toml`) | The first directory under `/var/log`, or the file name without `.log` | `file` | `varlog-unidentified` |
| Docker (`docker.toml`) | The first that is set: Compose service, Swarm service with `_` replaced by `-`, container name, image name | `container_name`, `image`, `docker_compose_service`, `docker_swarm_service` | `docker-unidentified` |
| UniFi syslog (`unifi-syslog.toml`) | `unifi-` and the device model in lowercase, or the APP-NAME of another device | `syslog_appname` | `syslog-unidentified` |

A fallback marks events that carry no name trawld accepts. Find the
original in the kept fields, for example
`service=varlog-unidentified | stats count() by file`. Docker and UniFi do
not try a second candidate. A Compose service named `my web` gives
`docker-unidentified` and keeps `docker_compose_service`. The image name is
the last path component of the image reference, without a tag or digest:
`registry.example:5000/org/redis:7` gives `redis`.

For journald, the unit candidate is `_SYSTEMD_USER_UNIT` when it is set and
is not `init.scope`, else `_SYSTEMD_UNIT`. One trailing `.service`, `.scope`,
`.slice`, `.socket`, or `.target` comes off, then everything from the first
`@`. For `SYSLOG_IDENTIFIER` and `_COMM`, one pair of parentheses around the
whole value comes off. Some results:

| Journal fields | `service` |
| --- | --- |
| `_SYSTEMD_UNIT=postgresql@17-main.service` | `postgresql` |
| `_SYSTEMD_UNIT=serial-getty@ttyS0.service` | `serial-getty` |
| `_SYSTEMD_UNIT=user@1000.service`, `_SYSTEMD_USER_UNIT=pipewire.service` | `pipewire` |
| `_SYSTEMD_UNIT=user@1000.service`, with no user unit or `_SYSTEMD_USER_UNIT=init.scope` | `user` |
| `_SYSTEMD_UNIT=init.scope`, PID 1 | `init` |
| No unit, `SYSLOG_IDENTIFIER=(sd-pam)` | `sd-pam` |
| `_SYSTEMD_UNIT=systemd-fsck@dev-disk-by\x2duuid-0f1e.service` | `systemd-fsck` |
| `_SYSTEMD_UNIT=mnt-my\x2ddisk.mount`, `SYSLOG_IDENTIFIER=mount` | `mount` |
| A unit name over 128 bytes, `SYSLOG_IDENTIFIER=long-unit` | `long-unit` |
| No unit, `SYSLOG_IDENTIFIER=/usr/bin/x y`, `_COMM=(a b)` | `journal-unidentified` |

The kept journal fields hold the source values unchanged, and each is set
only when its source field is not empty. To tell the instances of a template
apart, group by the kept unit, for example
`service=postgresql | stats count() by systemd_unit`. No kept field is named
`unit`: journald's own `UNIT` field, the unit that a systemd manager message
is about, arrives as `unit`.

A user unit names its service. The owner of a user unit chooses its name,
so any local user can run a unit named `sshd.service`, and its lines arrive
as `service=sshd`. Those lines keep `systemd_unit=user@1000.service`, where
`1000` is the user's uid. To see only the system's `sshd`, filter on
`service=sshd systemd_unit=sshd.service`.

## Upgrade the configuration

The `trawl-server` package updates only
`/usr/share/doc/trawl-server/examples/vector/`. Vector keeps running the
copies in `/etc/vector/vector.d/` until you replace them. After each upgrade
of `trawl-server`, copy the files again and restart Vector:

1. Set `NEW` to the directory that holds the new files. On the trawld host,
   that is the examples directory. On a collector host, copy the files from
   the trawld host into a directory of your own first, as in
   [Load the Trawl configuration](#load-the-trawl-configuration), and set
   `NEW` to that directory.

   ```bash
   NEW=/usr/share/doc/trawl-server/examples/vector
   ```

2. List your edits to the current copies, such as the `ca_file` line in
   `[sinks.trawld.tls]`. The copy in step 3 replaces them:

   ```bash
   for file in /etc/vector/vector.d/*.toml; do
     diff -u "$file" "$NEW/$(basename "$file")"
   done
   ```

   `diff` reports a missing file for a drop-in of your own. The copy leaves
   that file alone.

3. Copy the same files you installed:

   ```bash
   sudo install -m 0644 -t /etc/vector/vector.d "$NEW/base.toml" "$NEW/nginx.toml"
   ```

   Make your edits from step 2 again with `sudoedit`.

4. Save the rejection counter, as in [Start Vector](#start-vector), then
   restart Vector:

   ```bash
   sudo systemctl restart vector
   ```

5. [Check for refused events](#check-for-refused-events).

A new release can change the service names that the configuration derives.
Update saved searches and filters that use an old name.

## Troubleshoot delivery

Vector logs sink errors to its journal. Read them with `journalctl -u vector`.
Some failures leave no error in that journal, because trawld answers `200` and
counts the rejection on `/metrics` instead. The rejection counter is
`trawl_ingest_events_rejected_total{reason}`. See
[Confirm delivery over time](/operate/ingestion/#confirm-delivery-over-time).
A [preview of a sample](#preview-a-sample) shows each rejected event with its
reason and message, without waiting for the counter.

| What you see | What to do |
| --- | --- |
| Vector logs `404` from the `trawld` sink | Check whether `TRAWL_URL` is the `trawl-web` origin. `trawl-web` answers `404` on `/api/v1/ingest`. Set `TRAWL_URL` to trawld's HTTPS address. Vector drops a batch that gets a `404`. |
| Vector logs `401` | The key in `TRAWL_INGEST_TOKEN` is wrong, missing, revoked, or expired. Replace it with `sudoedit /etc/vector/trawl.env` and restart Vector. Vector does not retry a `401`. |
| Vector logs `403` | The key lacks `trawl:ingest`. Give its role that permission, or create a key with the `trawl-ingest` role. Vector does not retry a `403`. |
| Vector logs no error, the check finds nothing, and the rejection counter with `reason="invalid_env"` rises | `TRAWL_ENV` fails the env name rule: 1 to 32 characters from `a-z`, `0-9`, `_`, and `-`. `Prod` fails it. Fix `TRAWL_ENV` and restart Vector. |
| Vector logs no error, the check finds nothing, and the rejection counter with `reason="env_not_allowed"` rises | `TRAWL_ENV` is not in trawld's `[ingest] envs`. Keys are not scoped to an env, so the list is the only check. Add the env to the list and restart trawld, or fix `TRAWL_ENV`. |
| Vector logs no error, and the rejection counter rises with `reason="invalid_chars"` or `reason="service_too_long"` | Vector runs an old or edited copy of the shipped configuration, which sends a `service` that trawld refuses. The current files check every `service` they derive, as [Service names](#service-names) describes. Copy them again and restart Vector, as in [Upgrade the configuration](#upgrade-the-configuration). Then [preview a sample](#preview-a-sample) and expect `0` rejected. A drop-in of your own that derives `service` must end in the check from [Add your own source](#add-your-own-source). |
| The check finds nothing, and the rejection counter rises with another `reason` | For a rule an event broke, trawld answered `200` with `rejected` in the body, and Vector counted the batch as a success. The `reason` label names the rule. See the [event contract](/reference/events/). To see which events trawld rejects and why, [preview a sample](#preview-a-sample). Three reasons refuse the whole request instead, and Vector logs the status. |
| Vector logs `503`, and `trawl_hot_buffer_admission_refusals_total{producer="http",kind="full"}` or the rejection counter with `reason="hot_buffer_full"` rises | trawld's hot buffer is full. A request refused before trawld parses it raises only the admission counter. Vector retries. If it persists, check compaction and disk headroom on the [Health page](/operate/health/). |
| Vector logs `503` with the error code `request_limit_reached`, and `trawl_http_requests_refused_total` rises | trawld is at its `[server] max_concurrent_requests` limit and refused the request before it ran. Nothing was ingested. Vector retries with backoff, as it does for any `503`, including `hot_buffer_full`. If it persists, see [the request limit](/reference/configuration/#the-request-limit) before you raise the setting. |
| Vector logs `413` | One request is too large, and Vector drops that batch. Lower the sink's `batch.max_bytes`. If `trawl_hot_buffer_admission_refusals_total{producer="http",kind="oversized"}` or the rejection counter with `reason="ingest_batch_too_large"` rises, the request holds more than the hot buffer admits at once. If neither rises, the body exceeds trawld's `[ingest] max_body_bytes`, as sent (code `request_too_large`) or once gzip is decoded. A decoded body over the limit logs `ingest_body_too_large` on trawld. |
| Vector logs `500`, and `reason="wal_failure"` rises | trawld could not write its WAL. Vector retries. Check trawld's journal and the data directory's disk. |
| The event arrives with `env.defaulted` in `_repairs` | The event arrived with no `env`, so trawld used `[ingest] default_env`. The shipped configuration always sends `env`, and an unset `TRAWL_ENV` sends `prod`. A custom transform dropped `.env`. Set `.env = "${TRAWL_ENV:-prod}"` in it. |
| Vector logs a certificate verification or hostname error | trawld's generated certificate is valid only for `localhost`, `127.0.0.1`, and `::1`, so no other host can verify it. Give trawld a certificate for the `TRAWL_URL` hostname, as in [Configure TLS](/operate/access/#configure-tls). Get a private CA certificate over a channel you trust. A CA certificate fetched from the endpoint you are configuring proves nothing, because an attacker in the path serves their own. |
| Vector logs a connection refused or timeout error | The host or port in `TRAWL_URL` is wrong, trawld is not running, or a firewall blocks the port. |

## Receive UniFi syslog

Deploy `unifi-syslog.toml` and point the devices at the Vector host on UDP port
1514. To also receive TCP on that port, uncomment the complete
`sources.unifi_syslog_tcp` block. The `unifi_syslog*` input sends both sources
through the same normalizer before HTTP forwarding. An APP-NAME that trawld
would refuse as a `service`, such as `foo/bar`, arrives as
`syslog-unidentified`, with the original in `syslog_appname`.

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
with `_` belong to Trawl.

A transform that takes `service` from the event, not from a constant, must
end in the check that the shipped transforms use (see
[Service names](#service-names)). Otherwise trawld refuses each event whose
value breaks the rule, and Vector logs no error. Bind the rule once, test the
value, and fall back to a marked name of your own. This example takes
`service` from a field `app` that an earlier parser set, and keeps `app` on
the event:

```toml
[transforms.trawl_myapp]
type = "remap"
inputs = ["myapp_parsed"]
source = '''
gate = r'^[A-Za-z0-9_-][A-Za-z0-9._-]{0,127}$'
service = to_string(.app) ?? ""
if !match(service, gate) {
  service = "myapp-unidentified"
}
.service = service
.env = "${TRAWL_ENV:-prod}"
del(.source_type)
'''
```

Do not repair a failing value by replacing or cutting characters. Two
different names can then become the same service.

[Preview a sample](#preview-a-sample) of the new
mapping, then test one host with the
[proof pattern](#prove-the-first-event-arrived) before you roll it out to
every sender.
