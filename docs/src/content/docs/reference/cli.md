---
title: CLI & TUI
description: Command syntax, options, defaults, and output formats for the trawl binary.
---

The `trawl` binary runs the terminal UI, executes queries, validates syntax,
inspects the field catalog, checks a connection, and drives a running TUI over
a Unix socket.
For catalog procedures, see [catalog administration](/operate/catalog/).

## Global options

Every subcommand accepts these options.

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `-p, --profile` | `<NAME>` | *(none)* | Named profile from the config file. Overrides `[server]`. `trial` is reserved for the [trial](#trial-mode) |
| `--url` | `<URL>` | `https://localhost:5514` | Server URL |
| `--token` | `<TOKEN>` | *(none)* | API token |
| `--insecure` | *(flag)* | `false` | Accept self-signed TLS certificates |
| `-c, --config` | `<PATH>` | `~/.config/trawl/config.toml` | Config file path |
| `-V, --version` | *(flag)* | | Print the version and exit |
| `-h, --help` | *(flag)* | | Print help and exit |

A flag beats an environment variable, which beats the config file. Profiles and
the `[ui]` and `[tail]` settings live in
[client configuration](/reference/configuration/#client-configuration).

[`trawl doctor`](#doctor-mode) reads these options by its own rules. It has
no default URL, and it refuses some combinations.

When `insecure` is on from the flag, `TRAWL_INSECURE`, `[server]`, or a profile,
`trawl` writes one warning line to stderr before any other output. Stdout does
not change.

### Pin a CA with `ca_cert`

`ca_cert` names a PEM file of CA certificates. It is a config file key on
`[server]` and on `[profiles.<name>]`, and it has no flag or environment
variable. With it set, `trawl` trusts only the CAs in that file for the
connection, not the system trust store, and still checks the hostname in the
URL. The path must be absolute or start with `~`. A profile inherits the
`[server]` value, and a profile value of `""` clears it. `trawl` reads the
file before the first request and fails when it is missing, unreadable, or
empty. `ca_cert` together with an effective `insecure`, from any source, is an
error. See [client configuration](/reference/configuration/#client-configuration).

## Environment variables

| Variable | Equivalent flag | Description |
|----------|-----------------|-------------|
| `TRAWL_PROFILE` | `-p, --profile` | Named profile to select |
| `TRAWL_URL` | `--url` | Server URL |
| `TRAWL_TOKEN` | `--token` | API token |
| `TRAWL_INSECURE` | `--insecure` | Accept self-signed certificates |
| `RUST_LOG` | `warn` | Tracing filter for the TUI log file |

`trawl doctor` refuses to run when `TRAWL_PROFILE`, `TRAWL_URL`,
`TRAWL_TOKEN`, or `TRAWL_INSECURE` is set. See
[refused command lines](#refused-command-lines).

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | The command succeeded |
| `1` | The command failed. `trawl` prints the reason to stderr, except for a broken pipe, which exits quietly. For `trawl doctor`, the verdict is `fail` |
| `2` | Usage error. The command line is not valid, or `trawl doctor` refused it. `trawl` prints the reason to stderr |
| `3` | `trawl doctor` only. The verdict is `incomplete` |

## TUI mode

```text
trawl [--driver [PATH]]
```

Run `trawl` with no subcommand to open the terminal UI: a multi-tab query
editor, a schema browser, query history, saved queries, live tail over SSE, and
result search. Tracing goes to `~/.config/trawl/tui.log`, not stderr.

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--driver` | `[PATH]` | `~/.config/trawl/driver.sock` | Listen on a Unix socket for programmatic control. The path is optional |

```bash
trawl -p dev
```

## Query mode

```text
trawl query <QUERY> [--data GLOB] [-f FORMAT] [-o PATH]
```

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--data` | `<GLOB>` | *(none)* | Parquet glob for embedded mode. No server is contacted |
| `-f, --format` | `table\|json\|csv\|parquet` | `table` on a TTY, `json` on a pipe | Output format |
| `-o, --output` | `<PATH>` | *(stdout)* | Write output to a file. Required for `parquet` |

```bash
trawl query "_severity>=error last=1h | stats count() by service"
```

### Output formats

| Format | Shape | Notes |
|--------|-------|-------|
| `table` | Box-drawing table with a row-count footer | Adds the incomplete-results footer when one applies |
| `json` | One JSON object per row, newline-delimited | Pipe it to `jq` |
| `csv` | RFC 4180 | A string value starting with `=`, `+`, `-`, `@`, tab, or `\|` is prefixed with `'` |
| `parquet` | Snappy-compressed Parquet file | Requires `-o, --output` |

```bash
trawl query "last=1h | stats count() by service" | jq '.service'
```

### The incomplete-results footer

When a query binds a field whose catalog pin is degraded, `table` output adds
one line after the row count:

```text
note: results may be incomplete — degraded field(s): duration (see: trawl schema field duration)
```

With `-o, --output` the footer goes to stderr, so the file holds only rows.
`json`, `csv`, and `parquet` never print it: the HTTP response carries
`degraded_fields` instead. Embedded mode reads no catalog and never prints it.

### Embedded mode

`--data` queries local Parquet files through one ephemeral DuckDB connection,
with no server, no hot buffer, no authentication, and no row limit.

```bash
trawl query --data 'data/**/*.parquet' "* | stats count() by service"
```

## Validate mode

```text
trawl validate <QUERY>
```

Validate checks syntax without executing. With a resolvable token the server
also checks semantics, function arity, and regex patterns. Without one, `trawl`
parses locally.

```bash
trawl validate "_severity>=error | stats count() by host"
```

## Schema mode

```text
trawl schema <SUBCOMMAND>
```

Schema subcommands read and change the field catalog: pinned types, per-service
observations, and type-conflict evidence. Reads need `schema_read`. Repin,
cancellation, pin reclamation, and acknowledgement need `schema_write`.

Every subcommand takes `-f, --format` with the `table`, `json`, and `csv` values
and the TTY auto-detection `query` uses. A `--last` window accepts `s`, `m`,
`h`, `d`, and `w`. Field names fold to ASCII lowercase.

### Fields

```text
trawl schema fields [--service NAME] [--last WINDOW] [--limit N] [--data GLOB] [-f FORMAT]
```

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--service` | `<NAME>` | *(all)* | Only fields observed for this service |
| `--last` | `<WINDOW>` | *(all)* | Only fields observed inside this window |
| `--limit` | `<N>` | `500` | Maximum fields to list. The server clamps to the pin cap of 10000 |
| `--data` | `<GLOB>` | *(none)* | Embedded listing over local Parquet: names and physical types only |

The listing carries a `degraded` column. Catalog fill (`N/M pins used`) goes to
stderr. Embedded mode runs a plain `DESCRIBE` and reads no catalog metadata.

```bash
trawl schema fields --service nginx --last 7d
```

### Field

```text
trawl schema field <NAME> [--limit N] [--after CURSOR] [-f FORMAT]
```

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--limit` | `<N>` | `100` | Service observations per page. The maximum is 1000 |
| `--after` | `<CURSOR>` | *(first page)* | Resume after a previous run's printed cursor. The cursor is opaque, so pass it unchanged |

The detail view prints the pin, the degraded verdict, bounded conflict samples,
and per-service observations. Conflict rows and lifetime totals use different
windows. See [degraded pins](/operate/catalog/#read-a-degraded-pin).

```bash
trawl schema field duration
```

### Conflicts

```text
trawl schema conflicts [--field NAME] [--service NAME] [--last WINDOW] [--limit N] [-f FORMAT]
```

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--field` | `<NAME>` | *(all)* | Only conflicts for this field |
| `--service` | `<NAME>` | *(all)* | Only conflicts from this service |
| `--last` | `<WINDOW>` | *(all)* | Only conflicts recorded inside this window |
| `--limit` | `<N>` | `100` | Maximum rows. The maximum is 1000 |

```bash
trawl schema conflicts --field duration --service envoy --last 7d
```

### Repin

```text
trawl schema repin <FIELD> --to TYPE [--dialect otel|syslog] [--dry-run] [--force]
    [--yes] [--wait] [--max-nulled-rows N] [--max-ambiguous-rows N] [-f FORMAT]
```

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--to` | `<TYPE>` | *(required)* | `BIGINT`, `DOUBLE`, `TIMESTAMP`, `BOOLEAN`, `VARCHAR`, or `SEVERITY`, case-insensitive |
| `--dialect` | `otel\|syslog` | `otel` | Which ladder the corpus numerals read in. Legal only with `--to severity` |
| `--dry-run` | *(flag)* | `false` | Scan and persist a report job. The corpus is not rewritten |
| `--force` | *(flag)* | `false` | Accept a projected loss, or run a same-type resurrection pass, inside the ceilings |
| `--yes` | *(flag)* | `false` | Skip the interactive confirmation. Required off a TTY |
| `--wait` | *(flag)* | `false` | Poll this job to a terminal result |
| `--max-nulled-rows` | `<N>` | derived | With `--force`, the most rows the rewrite may null |
| `--max-ambiguous-rows` | `<N>` | derived | With `--force` and `--to severity`, the most dialect-ambiguous numerals the rewrite may carry |

Repin runs on an ingest-enabled node. An unspecified force ceiling derives from
the preview scan, with ten percent headroom and a minimum addition of ten rows,
and the CLI prints the ceiling it binds. `requires_force` in the report says
whether the same executing request would be refused.

With `--wait`, the command exits `0` for a completed rewrite or a dry-run
report. Any other terminal state exits nonzero, as does a job identity the
status endpoint stops naming. See [the repin procedure](/operate/catalog/#repin-a-field)
and [severity repinning](/operate/catalog/#repin-to-severity).

```bash
trawl schema repin duration --to BIGINT --dry-run
```

### Repin status

```text
trawl schema repin-status [-f FORMAT]
```

Status needs `schema_read` and returns the running job, or the newest one when
none is running. There is no lookup by job ID.

```bash
trawl schema repin-status
```

### Repin cancel

```text
trawl schema repin-cancel [-f FORMAT]
```

Cancellation exits `0` when the server accepts the request, and nonzero when no
job is running or the cutover has started. Acceptance is not proof of terminal
cancellation. In `json` and `csv` the receipt is one record with verdict,
detail, and job columns. In `table` the sentence and the job table are separate.
See [cancellation and verification](/operate/catalog/#cancel-the-repin).

```bash
trawl schema repin-cancel
```

### Reclaim dead pin slots

```text
trawl schema gc-pins [--dry-run] [--older-than WINDOW] [-f FORMAT]
```

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--dry-run` | *(flag)* | `false` | Scan and report only. There is no `--yes`, so omitting this flag deletes metadata |
| `--older-than` | `<WINDOW>` | `30d` | How long a field must have gone unobserved. The server raises it to the retention floor when that floor is longer |

The output states the requested window, the floor, and the effective window.
Summary text goes to stderr for `json` and `csv`, and to stdout for `table`.
See [pin reclamation](/operate/catalog/#reclaim-unused-pins).

```bash
trawl schema gc-pins --dry-run --older-than 90d
```

### Acknowledge a degraded pin

```text
trawl schema ack <FIELD> [--note TEXT | --clear] [-f FORMAT]
```

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--note` | `<TEXT>` | *(none)* | Why the pin is accepted as it stands. The limit is 1024 bytes. Conflicts with `--clear` |
| `--clear` | *(flag)* | `false` | Withdraw the acknowledgement |

An acknowledgement covers the evidence that exists when you write it. The next
conflict episode raises the badge again.

```bash
trawl schema ack duration --note "fix due Friday"
```

## Doctor mode

```text
trawl doctor --url URL [--token-env NAME | --token-file PATH] [--insecure] [--web-url ORIGIN] [-f FORMAT]
trawl doctor -p NAME [-c PATH] [--web-url ORIGIN] [-f FORMAT]
```

`trawl doctor` checks one client connection from the machine where it runs.
It checks the configuration, the API's transport, TLS, and health, the key,
and, with `--web-url`, the browser origin. It prints one line per check and a
verdict, and exits with the verdict's status. It does not run a query, ingest
an event, or sign in with a key.

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--url` | `<URL>` | *(none)* | The API to check. The doctor does not read the config file |
| `-p, --profile` | `<NAME>` | *(none)* | The CLI profile to check. `trial` checks the [trial](#the-reserved--p-trial-profile) |
| `--token-env` | `<NAME>` | *(none)* | With `--url`, read the key from the environment variable `NAME` |
| `--token-file` | `<PATH>` | *(none)* | With `--url`, read the key from this file |
| `--insecure` | *(flag)* | `false` | With `--url`, turn certificate verification off. `api.tls` then fails and the doctor sends no key |
| `-c, --config` | `<PATH>` | `~/.config/trawl/config.toml` | With `--profile`, the config file that holds the profile |
| `--web-url` | `<ORIGIN>` | *(none)* | Also check this browser origin of `trawl-web` |
| `-f, --format` | `table\|json` | `table` on a TTY, `json` on a pipe | Output format |

```bash
trawl doctor -p prod --web-url https://trawl.example.com
```

### Target and key

Give exactly one of `--url` and `--profile`. `--url` has no default here.

With `--url`, the key comes only from a source that the command line names:
`--token-env NAME` reads the variable `NAME`, and `--token-file PATH` reads the
file. The doctor does not read the config file, so a key saved in
`[server].token` never goes to the URL. With no key source, `api.identity` is
`not_configured`. `connection.config` fails, and names the source without its
value, when the variable is unset, empty, or not UTF-8, or when the file is
missing, unreadable, empty, not a regular file, or larger than an API key.

With `--profile NAME`, the config file must exist and hold
`[profiles.NAME]` with a `url`. When the file, the profile, or its `url` is
missing, `connection.config` fails, names the missing source, and the doctor
does not contact the API. With `--web-url`, the `web.*` checks still run,
because they do not depend on the API. The key is the profile's own `token`.
The doctor never uses `[server].token`, so a profile without `token` gives
`api.identity` `not_configured`. `ca_cert` and `insecure` inherit from
`[server]` as they do for every other command.

`-p trial` resolves through the rules of the
[reserved trial profile](#the-reserved--p-trial-profile): the trial's API
address, its operator key, and its certificate as the pinned CA.

### Trust

With `--url`, the doctor verifies the API's certificate against the system
roots only. No flag names a CA. To check an installation with a self-signed or
private-CA certificate, save it in a CLI profile that sets
[`ca_cert`](#pin-a-ca-with-ca_cert), then run `trawl doctor -p NAME`. With
`ca_cert`, the doctor trusts only the CAs in that file.

`--web-url` must be an `https` origin, or an `http` origin whose host is
`localhost` or a loopback address such as `127.0.0.1` or `[::1]`. The doctor
verifies the web origin's certificate against the system roots only, because
a browser opens that origin. A private CA on the web origin is not supported:
`web.transport` fails.

### Refused command lines

`trawl doctor` exits `2` and contacts nothing when:

- `TRAWL_URL`, `TRAWL_PROFILE`, `TRAWL_TOKEN`, or `TRAWL_INSECURE` is set,
  even to an empty value. The message names the variable and not its value.
  Unset the variable, then name the target with `--url` or `--profile`.
- `--token` is given. The doctor never takes a key's value on the command
  line.
- Neither `--url` nor `--profile` is given, or both are.
- `--profile` is given with `--insecure`, `--token-env`, or `--token-file`. A
  profile sets the URL, the key, and the trust.
- `-c` is given with `--url`.
- `--token-env` and `--token-file` are both given.
- `--url` or `--web-url` carries credentials (`user@` or `user:password@`
  before the host), a query, or a fragment. A `%3F` or `%23` in the path
  counts as a query or a fragment. The message does not repeat the URL.
- `--url` or `--web-url` is not an `http` or `https` URL with a host.
- `--web-url` has a path, or uses `http` with a host that is not loopback.

A profile `url` with credentials, a query, or a fragment fails
`connection.config` instead.

### Checks

The doctor runs the checks in this order. When a check's prerequisite is not
`complete`, the check is `not_sampled` with the reason `blocked`, and
`blocked_by` names the prerequisite.

| ID | Proves | Prerequisite |
|----|--------|--------------|
| `connection.config` | The flags or the profile resolve to a URL, a trust mode, and a key source. `source` names where they came from, such as ``CLI profile `prod` in ~/.config/trawl/config.toml`` | None |
| `api.transport` | A connection to the API origin opens. The doctor sends one `GET /api/v1/health` with no key | `connection.config` |
| `api.tls` | The API's certificate verifies under the trust mode that the report names: system roots or the pinned CA. An `http` URL fails with `connection is not TLS`. `insecure` fails with `certificate not verified` | `api.transport` |
| `api.health` | The health answer parses as a Trawl health response with the status `ok`, `degraded`, or `unavailable`, a `checks` map, and a `version`. `trawld` always sends all three. An answer without the map or the version fails with `not a trawl health answer`. A 503 answer counts | `api.tls` |
| `api.health.<key>` | One row for each check that the server reports, sorted by name. `ok` is `complete`. `error`, `refusing`, and every other value are `failed` | `api.health` |
| `api.health._invalid` | The server reported a check name that is not `[a-z][a-z0-9_]{0,63}`. This row is always `failed`, and the report does not show the names. No check name starts with `_`, so this ID cannot match a server's check | `api.health` |
| `api.identity` | `GET /api/v1/whoami` answers HTTP 200 and accepts the key. `detail` shows the key's name, its kind, and its permissions, never the key or its prefix. The name shows at most 32 characters. The kind is `human` or `service`: any other kind fails with `the answer is not a trawl whoami response`. Only Trawl's own permission names show, such as `query` or `ingest`. Any other permission string is counted as `N unrecognized` and not shown. A rejected key (HTTP 401) and a key with no permissions (HTTP 403) fail. Any other 2xx status fails with `unexpected status` | `api.health`, and a key selected |
| `web.transport` | `GET /healthz` on `--web-url` answers HTTP 200 with the body `ok` | `--web-url` given |
| `web.origin` | `trawl-web` accepts `--web-url` as a browser origin. The doctor sends `POST /api/auth/login` with `Origin: <web-url>` and an empty `api_key`. Only `400 {"error":"bad request"}` is `complete`. `403 {"error":"cross-origin request rejected"}` fails with `origin not in public_origins`. HTTP 429 gives `not_sampled` with the reason `rate_limited`. Any other answer fails with `not a trawl-web login endpoint` | `web.transport` |

The doctor sends the key only in `GET /api/v1/whoami`, and only after the
unkeyed health request got a Trawl health answer under verified TLS. A wrong
host, an untrusted certificate, `insecure`, or an `http` URL never receives
the key.

The report never shows the key or its prefix, even when a server echoes
them. In every string that a server sends, such as a key's name, a health
check's value, or the server version, each run of 8 or more of the key's
characters shows as `[redacted]`. A health check name that holds such a run
counts as an invalid name. Redaction finds only the key's own characters. A
server that already holds the key can still send it in another form, such as
base64, in the key's name. The 32-character limit on the name bounds how much
of it the report shows.

The `web.*` checks do not depend on the `api.*` checks, and they send no key.
Without `--web-url`, the report has no `web.*` rows.

Each request waits up to 10 seconds. A request with no answer in that time
gives `not_sampled` with the reason `timed_out`. When the answer starts but its
body does not finish in that time, the connection and the certificate are
proved: `api.transport` and `api.tls` are `complete`, and only the check that
reads the body is `not_sampled` with `timed_out`. An HTTP 429 answer gives
`not_sampled` with the reason `rate_limited`. The doctor does not follow a
redirect: the check fails with `redirect refused`. The doctor reads at most
64 KiB of a health or `whoami` answer. A larger answer fails the check with
`response too large`.

### Outcomes and verdict

Every check has one of four outcomes.

| Outcome | Meaning |
|---------|---------|
| `complete` | The doctor saw the check's assertion hold |
| `failed` | The doctor saw evidence against it |
| `not_configured` | You did not select what the check needs, such as a key |
| `not_sampled` | The doctor could not look. `reason` says why: `blocked`, `rate_limited`, or `timed_out` |

The verdict follows from the outcomes and sets the exit status.

| Verdict | When | Exit status |
|---------|------|-------------|
| `pass` | Every check is `complete` or `not_configured` | `0` |
| `fail` | At least one check is `failed` | `1` |
| `incomplete` | No check is `failed`, and at least one is `not_sampled` | `3` |

A refused command line exits `2`.

### Output

`table` prints the target, one line per check, the notes, and the verdict. A
check's line holds its ID, its outcome, and then its reason, detail, source,
blocking check, and next action when they are present.

```text
target: https://trawl.example.com:5514 (CLI profile `prod` in ~/.config/trawl/config.toml)
connection.config           complete        trust: pinned CA from ca_cert in [profiles.prod]; key: token in [profiles.prod]; source: CLI profile `prod` in ~/.config/trawl/config.toml
api.transport               complete
api.tls                     complete        verified under the pinned CA
api.health                  complete        status: unavailable; server version 1.0.0
api.health.duckdb           failed          reported error; next: read the server's log for why duckdb reports error
api.health.ingest_capacity  complete        reported ok
api.identity                failed          key rejected; source: token in [profiles.prod]; next: check the key: it may be revoked, expired, or mistyped
verdict: fail (exit 1)
```

`json` prints one document. Every key is present on every check, and a value
that the doctor did not observe is `null`.

| Key | Value |
|-----|-------|
| `version` | Version of the document shape. It is `1` |
| `vantage` | Where the checks ran. It is `client` |
| `target` | `origin`, the API origin as `scheme://host:port`, or `null` when `connection.config` failed. `source`, where the target came from |
| `verdict` | `pass`, `fail`, or `incomplete` |
| `checks` | The checks in the order they ran. Each has `id`, `outcome`, `reason`, `detail`, `source`, `blocked_by`, and `next_action` |
| `notes` | Report-level notes. A note never changes an outcome |

A note says, for example, that the CLI and the server versions differ, or
that `ingest_capacity` is `ok` but the key lacks the `ingest` permission.

No output holds a key, a key prefix, a response body, TLS library error text,
or URL credentials.

### Side effects

The doctor sends only these requests: `GET /api/v1/health` and
`GET /api/v1/whoami` to the API, and `GET /healthz` and
`POST /api/auth/login` to `--web-url`. They change nothing that you
configured, but three effects remain:

- `GET /api/v1/whoami` updates the key's last-used time.
- Every request spends rate-limit budget on the server that answers it.
- An HTTP 5xx answer that a request provokes emits its `http_failure` event.

### Limits

`web.origin` shows that `trawl-web` accepts its own origin. It does not show
that `trawl-web` reaches `trawld`: the doctor runs where the client runs and
cannot see that connection. A doctor on the server host checks it in a later
release. Until then, sign in from a browser.

The doctor does not send a test event, so it does not prove that ingest works.

## Trial mode

```text
trawl trial <VERB>
```

`trawl trial` runs a disposable Trawl installation in Docker on this machine,
on loopback only, with sample data. It is supported on Linux with Docker
Engine and the Compose v2 plugin, version 2.20 or later, over the local
`unix://` socket. [Your first query](/getting-started/first-query/) walks
through it and states the trust boundary. The trial verbs run before the
config file is read, so a broken client config does not block them.

| Verb | Description |
|------|-------------|
| `up` | Create the trial, or resume it. Pulls the images when absent, starts PostgreSQL, creates the databases and roles, applies the Fleet schema, generates a certificate, mints two keys, starts `trawld` and `trawl-web`, checks one authenticated query, and loads the samples. Prints the addresses and the token file paths, never a token |
| `status` | Print the trial id, addresses, image ids and digests, certificate fingerprint, sample range as absolute timestamps, setup phases, and containers. Warns on stderr when `TRAWL_URL` or `TRAWL_TOKEN` is set, when `TRAWL_INSECURE` is `true`, and when `TRAWL_PROFILE` names a profile other than `trial` |
| `key` | Print the operator token and one newline to stdout, nothing else |
| `stop` | Stop the containers. The databases, keys, samples, and state stay, and a later `up` resumes |
| `down` | Print the containers, volumes, and network that carry this trial's id, ask `Delete all of it? [y/N]` on a terminal, delete them and the trial directory |

### Up

```text
trawl trial up [--api-port PORT] [--web-port PORT] [--image REFERENCE] [--no-sample-data]
```

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--api-port` | `<PORT>` | `15514` | Loopback port for the HTTPS API |
| `--web-port` | `<PORT>` | `18090` | Loopback port for the browser UI |
| `--image` | `<REFERENCE>` | `ghcr.io/jakub/trawl:<CLI version>` | Run this trawl image instead of the published one. The published tag is the CLI version without its `+` build metadata, so CLI `1.1.0-rc.1+build.7` runs `ghcr.io/jakub/trawl:1.1.0-rc.1`. `up` and `status` print the override |
| `--no-sample-data` | *(flag)* | `false` | Skip the sample events |

The browser address `up` and `status` print is `http://127.0.0.1:<web-port>`.
A browser tries `localhost` on the IPv6 address `::1` first, where the trial
does not listen and another local program could.

Every `up` stops `trawl-web` and gives it a new session key before it starts
it, so every browser session from before that `up` ends.

Ports and the image are fixed when the trial is created. On a resume an
omitted flag means the recorded value, and a different value is refused with
a message that points to `down`. The samples are loaded once, and a resume
never posts them again. A trial created with `--no-sample-data` gets them on
a later `up` without the flag, when the sample services still hold no events.

When `up` cannot verify a sample post, or the sample services hold events that
the trial did not post, `up` refuses and posts nothing. Then
`trawl trial up --no-sample-data` keeps the trial without samples for good:
every later `up` skips them, and `status` shows them as declined.
`trawl trial down --yes` and then `trawl trial up` start a fresh trial with
samples.

`up` checks Docker before it creates anything, and the message names the
missing requirement: the `docker` command, the Compose plugin or a version
below 2.20, an unreachable engine, or a `DOCKER_HOST` or active Docker context
that is not a `unix://` address. A taken port is refused with the port and
the flag that moves it. A `trawl-trial` project or a `trawl-trial-claim`
container with another trial's id is refused before anything is changed.

### Down

```text
trawl trial down [--yes]
```

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--yes` | *(flag)* | `false` | Delete without asking. Required when stdin is not a terminal |

Without a terminal and without `--yes`, `down` exits `1` and deletes nothing.
Before it asks, `down` waits up to a minute for one-off containers that an
interrupted `trawl trial` command left running. It lists the one-off
containers that are left with the other resources, and deletes nothing
before the answer.
It deletes only resources that carry this trial's id, and it accepts state
written by any CLI version. When no trial exists it prints so and exits `0`.
When the trial directory has no `state.json`, `down` lists the directory and
each entry in it, and asks the same way, even when the directory is empty.
It deletes those entries and the directory only after a yes or with `--yes`.
A listed subdirectory is deleted with everything in it. `down` checks only
the top-level entries against the listing: when an entry appears at the top
level while `down` asks, `down` keeps that entry and the directory, and exits
non-zero.

### State and lock

The trial directory is `$XDG_STATE_HOME/trawl/trial`, or
`~/.local/state/trawl/trial` when `XDG_STATE_HOME` is unset. It is mode
`0700` and holds `state.json`, `compose.json`, the public certificate
`ca.pem`, and the two token files `operator.token` and `ingest.token` at mode
`0600`. A symbolic link at the directory or its parent is refused. For a trial,
`down` lists the directory as one line and then deletes it with everything
in it, including files added while `down` asked.

`up`, `stop`, and `down` hold an exclusive lock on
`$XDG_STATE_HOME/trawl/trial.lock`, outside the directory `down` deletes. A
second command waits and prints that it is waiting. `status`, `key`, and
`-p trial` take no lock.

### The reserved `-p trial` profile

`-p trial`, or `TRAWL_PROFILE=trial`, connects any `trawl` command and the TUI
to the trial. It reads the URL `https://127.0.0.1:<api-port>`, the operator
token, and the certificate from the trial directory, and never edits the
config file. It exits `1` and sends nothing when:

- `TRAWL_URL` or `TRAWL_TOKEN` is set, even to an empty value, or `--url` or
  `--token` is given.
- The config file defines `[profiles.trial]`.
- `--insecure` or `TRAWL_INSECURE` is on, through the `ca_cert` with
  `insecure` rule.
- The trial directory is missing or the trial is unfinished. The message
  names `trawl trial up`.
- The command is `trawl driver`. Start the TUI with `trawl -p trial --driver`
  and run `trawl driver` without the profile.

Every trial verb prints its reason to stderr and exits `1` on refusal, and
nothing is created or deleted before the check that refused.

## Driver mode

```text
trawl driver [--socket PATH] <SUBCOMMAND>
```

Driver subcommands control a TUI started with `--driver`. Each invocation opens
one connection to the socket and closes it.

| Flag | Value | Default | Description |
|------|-------|---------|-------------|
| `--socket` | `<PATH>` | `~/.config/trawl/driver.sock` | Path to the driver Unix socket |

| Subcommand | Arguments | Description |
|------------|-----------|-------------|
| `status` | | Print TUI state as JSON |
| `query <QUERY>` | `-f, --format`, `--timeout <MS>` (default `300000`) | Set the editor content, execute it, and print the results |
| `set-query <QUERY>` | | Set the editor content without executing |
| `capture` | `--width <N>` (default `120`), `--height <N>` (default `40`) | Render the TUI to text |
| `key <KEY>` | | Inject one keystroke, such as `ctrl+enter`, `F5`, or `a` |
| `keys <KEYS>...` | | Inject several keystrokes in order |
| `get-results` | `--tab <N>`, `-f, --format` | Print structured result data from a tab. The tab index is 0-based and defaults to the active tab |
| `quit` | | Ask the TUI to exit cleanly |

`parquet` is not a driver output format. Starting a TUI removes an existing file
at the socket path, so give an automated session its own socket.

```bash
trawl driver --socket /tmp/session.sock capture --width 160 --height 50
```
