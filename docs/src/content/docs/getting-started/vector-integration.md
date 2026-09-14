---
title: Ship logs with Vector
description: Install Vector on a Debian host, load the Trawl configuration, set the token, and confirm that events arrive.
---

Vector reads journald and `/var/log` files, maps them to the
[event contract](/reference/events/), buffers to disk, and posts gzip batches
to `POST /api/v1/ingest`. This page sets it up on one Debian host with the
configuration shipped in the Trawl repository.

You need:

- trawld reachable from the host at its HTTPS address, for example
  `https://trawl.example.com:5514`. Vector talks to trawld, not to `trawl-web`.
- A service key with `trawl:ingest`, from
  [Create roles and keys](/operate/access/#create-roles-and-keys).
- A trawld certificate the host can verify. See [Configure TLS](/operate/access/#configure-tls).

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

2. Set the environment in `/etc/default/vector`, then restrict the file
   because it holds the token:

   ```bash
   VECTOR_CONFIG_DIR=/etc/vector/vector.d
   VECTOR_DANGEROUSLY_ALLOW_ENV_VAR_INTERPOLATION=true
   TRAWL_URL=https://trawl.example.com:5514
   TRAWL_INGEST_TOKEN=flt_...
   TRAWL_ENV=prod
   ```

   ```bash
   sudo chmod 0600 /etc/default/vector
   ```

   The unit reads this file for both `vector validate` and `vector`.
   `VECTOR_CONFIG_DIR` replaces the default `/etc/vector/vector.yaml`. The
   interpolation variable is required: Vector 0.57 and later do not expand
   `${TRAWL_URL}` in a configuration file without it. `TRAWL_ENV` must be in
   the server's `[ingest] envs`.

3. Verify the server certificate. As shipped, `[sinks.trawld.tls]` sets
   `verify_certificate = false`. When trawld uses a certificate from a CA the
   host trusts, set it to `true`. For a private CA, also name its certificate:

   ```toml
   [sinks.trawld.tls]
   verify_certificate = true
   ca_file = "/etc/vector/trawl-ca.pem"
   ```

   The self-signed certificate that trawld generates is valid only for
   `localhost`, so it cannot pass verification from another host.

## Start Vector and confirm delivery

1. Start and enable the service:

   ```bash
   sudo systemctl enable --now vector
   sudo systemctl status vector
   ```

   The unit runs `vector validate` before it starts. A configuration error
   shows here.

2. Watch the journal for the first batches:

   ```bash
   journalctl -u vector -f
   ```

   A `401` means the token is wrong. A `403` means the key lacks
   `trawl:ingest`. Vector does not retry either one. A connection or TLS
   error means `TRAWL_URL` or the certificate.

3. Write a known line to the journal and query it:

   ```bash
   sudo systemd-run --unit trawl-check --quiet /bin/echo 'vector delivery check'
   trawl -p prod query 'service=trawl-check last=15m'
   ```

   Expect one event with `message` = `vector delivery check`, `_producer` =
   `http`, `host` = this host, and `_severity` derived from `severity_text`.

4. Check for rejected events on the server. Vector counts a `200` as success
   even when trawld rejected some events in the batch, so read
   `trawl_ingest_events_rejected_total` and `trawl_ingest_repairs_total` on
   `/metrics`. See [Confirm delivery over time](/operate/ingestion/#confirm-delivery-over-time).

The shipped sink behaves as follows:

| Setting | Value |
| --- | --- |
| Batch | 1 MB or 5 seconds, gzip |
| Buffer | Disk, 1 GB under `/var/lib/vector`. Sources block when it is full. |
| Retries | `5xx`, `408`, and `429`, with 1 to 30 second backoff. Other `4xx` responses drop the batch. |
| Concurrency | Adaptive |
| Acknowledgements | Enabled. A source advances only after trawld accepts the batch. |

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
with `_` belong to Trawl. Test one host before you roll a new mapping out to
every sender.
